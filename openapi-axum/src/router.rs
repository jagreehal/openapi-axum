use std::collections::BTreeSet;

use axum::Router;
use axum::handler::Handler;
use axum::http::Method;
use axum::routing::{MethodFilter, MethodRouter};
use utoipa::openapi::path::{HttpMethod, Operation, ParameterIn};
use utoipa::openapi::{OpenApi, OpenApiBuilder, RefOr};

use crate::DocCtx;

/// Implemented by `#[route]` for the type-namespace twin of each handler fn.
#[doc(hidden)]
pub trait Route {
    const METHOD: Method;
    const PATH: &'static str;
    const OPERATION_ID: &'static str;
    fn operation(ctx: &mut DocCtx) -> Operation;

    fn def<S, H, T>(handler: H) -> RouteDef<S>
    where
        H: Handler<T, S>,
        T: 'static,
        S: Clone + Send + Sync + 'static,
    {
        let filter = MethodFilter::try_from(Self::METHOD).expect("route method is a standard HTTP method");
        RouteDef {
            method: Self::METHOD,
            path: Self::PATH,
            operation_id: Self::OPERATION_ID,
            document: Self::operation,
            router: axum::routing::on(filter, handler),
        }
    }
}

/// A handler plus its derived OpenAPI operation. Build with [`routes!`](crate::routes).
pub struct RouteDef<S> {
    method: Method,
    path: &'static str,
    operation_id: &'static str,
    document: fn(&mut DocCtx) -> Operation,
    router: MethodRouter<S>,
}

/// Identifies a registered route to the validation layer. Attached to each route at
/// registration, so lookups don't depend on the request's matched path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RouteId(pub(crate) u64);

fn next_route_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Default for [`ApiRouter::verify_body_limit`]: 1 MiB.
pub const DEFAULT_VERIFY_BODY_LIMIT: usize = 1024 * 1024;

/// What `ApiRouter::verify_responses` does when a response contradicts the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMode {
    /// Panic: for tests and CI, so a violation fails the test that caused it.
    Panic,
    /// Log an error (via `tracing` when enabled) and send the response unchanged.
    Log,
}

/// An axum `Router` that keeps an OpenAPI document in lockstep with its routes.
pub struct ApiRouter<S = ()> {
    router: Router<S>,
    openapi: OpenApi,
    registered: BTreeSet<(String, String)>,
    spec_routes: Vec<String>,
    verify: Option<VerifyMode>,
    /// Kept apart from `verify` so `verify_body_limit` works in either call order.
    verify_limit: Option<usize>,
    /// (route id, method, documented path): `nest` re-prefixes, `merge` combines.
    route_keys: Vec<(u64, Method, String)>,
}

impl<S: Clone + Send + Sync + 'static> Default for ApiRouter<S> {
    fn default() -> Self {
        Self::new()
    }
}

fn http_method(m: &Method) -> HttpMethod {
    match *m {
        Method::GET => HttpMethod::Get,
        Method::POST => HttpMethod::Post,
        Method::PUT => HttpMethod::Put,
        Method::DELETE => HttpMethod::Delete,
        Method::PATCH => HttpMethod::Patch,
        Method::HEAD => HttpMethod::Head,
        Method::OPTIONS => HttpMethod::Options,
        Method::TRACE => HttpMethod::Trace,
        _ => unreachable!("#[route] only accepts standard methods"),
    }
}

/// Components are shared by name across the whole document, so the same name with a
/// different definition (two `Todo` types, two `auth` schemes) would silently document
/// one of them wrong. Same definition is fine.
fn conflicts<V: PartialEq>(
    existing: &std::collections::BTreeMap<String, V>,
    incoming: impl IntoIterator<Item = (String, V)>,
    kind: &str,
) -> Vec<String> {
    let mut seen: std::collections::BTreeMap<String, V> = std::collections::BTreeMap::new();
    let mut errors = Vec::new();
    for (name, value) in incoming {
        let clash = existing.get(&name).or_else(|| seen.get(&name)).is_some_and(|v| *v != value);
        if clash {
            errors.push(format!("{kind} `{name}` has two different definitions; give one of them a different name"));
        }
        seen.entry(name).or_insert(value);
    }
    errors
}

fn component_conflicts(existing: Option<&utoipa::openapi::Components>, incoming: &OpenApi) -> Vec<String> {
    let (Some(existing), Some(incoming)) = (existing, &incoming.components) else { return Vec::new() };
    let mut errors = conflicts(&existing.schemas, incoming.schemas.clone(), "schema");
    errors.extend(conflicts(&existing.security_schemes, incoming.security_schemes.clone(), "security scheme"));
    errors
}

/// Join a nest prefix and a route path exactly like axum does
/// (`axum::routing::path_router::path_for_nested_route`), so documented paths,
/// duplicate tracking and spec routes match what axum actually serves.
fn nested_path(prefix: &str, path: &str) -> String {
    if prefix.ends_with('/') {
        format!("{prefix}{}", path.trim_start_matches('/'))
    } else if path == "/" {
        prefix.to_owned()
    } else {
        format!("{prefix}{path}")
    }
}

/// `{name}` / `{*name}` segments of an axum path template.
fn template_params(path: &str) -> BTreeSet<String> {
    path.split('/')
        .filter_map(|seg| seg.strip_prefix('{')?.strip_suffix('}'))
        .map(|name| name.trim_start_matches('*').to_owned())
        .collect()
}

impl<S: Clone + Send + Sync + 'static> ApiRouter<S> {
    pub fn new() -> Self {
        Self::with_openapi(OpenApiBuilder::new().build())
    }

    /// Start from your own document (info, servers, security schemes...).
    pub fn with_openapi(openapi: OpenApi) -> Self {
        Self {
            router: Router::new(),
            openapi,
            registered: BTreeSet::new(),
            spec_routes: Vec::new(),
            verify: None,
            verify_limit: None,
            route_keys: Vec::new(),
        }
    }

    /// Register a documented route.
    ///
    /// # Panics
    /// If the route's contract is inconsistent: path template params that don't match
    /// the documented path params, a status documented twice, two body extractors, or
    /// the same method + path registered twice, or a schema / security scheme name
    /// that already means something different in this document. This runs at startup and in any test
    /// that builds the router, so it fails before deploy.
    #[must_use]
    pub fn route(mut self, def: RouteDef<S>) -> Self {
        let mut ctx = DocCtx::new(def.operation_id);
        let op = (def.document)(&mut ctx);

        let documented: BTreeSet<String> = op
            .parameters
            .iter()
            .flatten()
            .filter_map(|p| match p {
                RefOr::T(p) if p.parameter_in == ParameterIn::Path => Some(p.name.clone()),
                _ => None,
            })
            .collect();
        let template = template_params(def.path);
        if documented != template && ctx.errors.is_empty() {
            ctx.error(format!("path {} has params {template:?} but the handler extracts {documented:?}", def.path));
        }
        if !self.registered.insert((def.method.to_string(), def.path.to_owned())) {
            ctx.error(format!("{} {} is registered twice", def.method, def.path));
        }
        let components = self.openapi.components.get_or_insert_default();
        let schemes: Vec<(String, RefOr<_>)> =
            ctx.security_schemes.into_iter().map(|(n, s)| (n, RefOr::T(s))).collect();
        let mut problems = conflicts(&components.schemas, ctx.schemas.clone(), "schema");
        problems.extend(conflicts(&components.security_schemes, schemes.clone(), "security scheme"));
        ctx.errors.extend(problems.into_iter().map(|p| format!("{}: {p}", def.operation_id)));
        assert!(ctx.errors.is_empty(), "invalid API contract:\n  {}", ctx.errors.join("\n  "));

        for (name, schema) in ctx.schemas {
            components.schemas.entry(name).or_insert(schema);
        }
        for (name, scheme) in schemes {
            components.security_schemes.entry(name).or_insert(scheme);
        }
        self.openapi.paths.add_path_operation(def.path, vec![http_method(&def.method)], op);
        let id = next_route_id();
        self.route_keys.push((id, def.method.clone(), def.path.to_owned()));
        // The id rides on the request (for validation) and the response (for
        // `verify_responses`, which runs outside the route).
        let tagged = def.router.layer(axum::Extension(RouteId(id))).layer(axum::middleware::map_response(
            move |mut res: axum::response::Response| async move {
                res.extensions_mut().insert(RouteId(id));
                res
            },
        ));
        self.router = self.router.route(def.path, tagged);
        self
    }

    /// Register many documented routes: `.routes(routes![a, b, c])`.
    #[must_use]
    pub fn routes(self, defs: impl IntoIterator<Item = RouteDef<S>>) -> Self {
        defs.into_iter().fold(self, Self::route)
    }

    /// A plain axum route that is deliberately left out of the OpenAPI document.
    #[must_use]
    pub fn route_undocumented(mut self, path: &str, method_router: MethodRouter<S>) -> Self {
        self.router = self.router.route(path, method_router);
        self
    }

    /// Serve the final document as JSON at `path` (excluded from the document itself).
    /// Survives `merge`/`nest` (prefixed when nested); it always serves the *whole* document.
    #[must_use]
    pub fn spec_route(mut self, path: impl Into<String>) -> Self {
        self.spec_routes.push(path.into());
        self
    }

    #[must_use]
    pub fn merge(mut self, other: ApiRouter<S>) -> Self {
        for key in &other.registered {
            assert!(self.registered.insert(key.clone()), "{} {} is registered twice", key.0, key.1);
        }
        Self::assert_compatible(&self.openapi, &other.openapi);
        self.router = self.router.merge(other.router);
        self.openapi.merge(other.openapi);
        self.spec_routes.extend(other.spec_routes);
        self.verify = self.verify.or(other.verify);
        self.verify_limit = self.verify_limit.or(other.verify_limit);
        self.route_keys.extend(other.route_keys);
        self
    }

    #[must_use]
    /// # Panics
    /// If `prefix` contains a capture (`/tenant/{tenant}`): the nested handlers would
    /// receive a path value they don't extract or document. Put the full path in
    /// `#[route]` and use [`merge`](Self::merge) instead.
    pub fn nest(mut self, prefix: &str, mut other: ApiRouter<S>) -> Self {
        let captures = template_params(prefix);
        assert!(
            captures.is_empty(),
            "can't nest under `{prefix}`: captured prefixes ({captures:?}) would add path params the nested \
             handlers don't extract or document. Write the full path in #[route] (e.g. `{prefix}/...`) and merge instead"
        );
        for (m, p) in std::mem::take(&mut other.registered) {
            let key = (m, nested_path(prefix, &p));
            assert!(self.registered.insert(key.clone()), "{} {} is registered twice", key.0, key.1);
        }
        // utoipa's `OpenApi::nest` replaces whole path items, which would drop e.g. an
        // existing `POST /v1/x` when nesting a `GET /x`. Prefix, then merge per method.
        let paths = std::mem::take(&mut other.openapi.paths.paths);
        other.openapi.paths.paths = paths.into_iter().map(|(p, item)| (nested_path(prefix, &p), item)).collect();
        Self::assert_compatible(&self.openapi, &other.openapi);
        self.openapi.merge(other.openapi);
        self.spec_routes.extend(other.spec_routes.iter().map(|p| nested_path(prefix, p)));
        self.verify = self.verify.or(other.verify);
        self.verify_limit = self.verify_limit.or(other.verify_limit);
        self.route_keys.extend(other.route_keys.iter().map(|(id, m, p)| (*id, m.clone(), nested_path(prefix, p))));
        self.router = self.router.nest(prefix, other.router);
        self
    }

    fn assert_compatible(this: &OpenApi, other: &OpenApi) {
        let errors = component_conflicts(this.components.as_ref(), other);
        assert!(errors.is_empty(), "invalid API contract:\n  {}", errors.join("\n  "));
    }

    /// Apply a tower layer to every route (see [`axum::Router::layer`]).
    #[must_use]
    pub fn layer<L>(mut self, layer: L) -> Self
    where
        L: tower_layer::Layer<axum::routing::Route> + Clone + Send + Sync + 'static,
        L::Service: tower_service::Service<axum::extract::Request> + Clone + Send + Sync + 'static,
        <L::Service as tower_service::Service<axum::extract::Request>>::Response:
            axum::response::IntoResponse + 'static,
        <L::Service as tower_service::Service<axum::extract::Request>>::Error: Into<std::convert::Infallible> + 'static,
        <L::Service as tower_service::Service<axum::extract::Request>>::Future: Send + 'static,
    {
        self.router = self.router.layer(layer);
        self
    }

    /// Provide the state; returns a router that needs none (`S2 = ()` usually).
    pub fn with_state<S2>(self, state: S) -> ApiRouter<S2> {
        ApiRouter {
            router: self.router.with_state(state),
            openapi: self.openapi,
            registered: self.registered,
            spec_routes: self.spec_routes,
            verify: self.verify,
            verify_limit: self.verify_limit,
            route_keys: self.route_keys,
        }
    }

    pub fn openapi(&self) -> &OpenApi {
        &self.openapi
    }

    /// Mutate the document (title, servers, security...) before splitting.
    #[must_use]
    pub fn map_openapi(mut self, f: impl FnOnce(OpenApi) -> OpenApi) -> Self {
        self.openapi = f(self.openapi);
        self
    }

    /// Check every response of a documented route against the document. Applied
    /// outermost, so it sees what *middleware* and escape hatches (`#[raw]`, layers)
    /// actually send.
    ///
    /// - Status and content type: checked on every response, without touching the body.
    /// - Body schema: checked when the body has a known length up to the limit
    ///   ([`DEFAULT_VERIFY_BODY_LIMIT`], change it with
    ///   [`verify_body_limit`](Self::verify_body_limit)). Only those bodies are
    ///   buffered. Streams (unknown length, `text/event-stream`) and larger bodies pass
    ///   through untouched and unbuffered.
    ///
    /// Use `VerifyMode::Panic` in tests and CI, `VerifyMode::Log` to sample production.
    #[cfg(feature = "validation")]
    #[must_use]
    pub fn verify_responses(mut self, mode: VerifyMode) -> Self {
        self.verify = Some(mode);
        self
    }

    /// Largest response body (bytes) `verify_responses` buffers to check its schema.
    /// Call it before or after `verify_responses`; it does nothing without it.
    #[cfg(feature = "validation")]
    #[must_use]
    pub fn verify_body_limit(mut self, bytes: usize) -> Self {
        self.verify_limit = Some(bytes);
        self
    }

    /// Register a schema component explicitly, for types only referenced from places
    /// that can't register them (e.g. an enum field of an `IntoParams`-only struct).
    ///
    /// # Panics
    /// If a different schema is already registered under the same name.
    #[must_use]
    pub fn schema<T: utoipa::ToSchema>(mut self) -> Self {
        let mut schemas = Vec::new();
        T::schemas(&mut schemas);
        schemas.push((T::name().into_owned(), T::schema()));
        let components = self.openapi.components.get_or_insert_default();
        let errors = conflicts(&components.schemas, schemas.clone(), "schema");
        assert!(errors.is_empty(), "invalid API contract:\n  {}", errors.join("\n  "));
        for (name, schema) in schemas {
            components.schemas.entry(name).or_insert(schema);
        }
        self
    }

    /// The axum router and the generated document.
    ///
    /// # Panics
    /// If the document has a `$ref` that doesn't resolve: client generators can't use it.
    pub fn split(mut self) -> (Router<S>, OpenApi) {
        crate::doc::reference_components(&mut self.openapi);
        let dangling = crate::doc::dangling_refs(&self.openapi);
        assert!(
            dangling.is_empty(),
            "invalid API contract: unresolved schema references\n  {}",
            dangling
                .iter()
                .map(|(r, at)| {
                    let name = r.rsplit('/').next().unwrap_or(r);
                    format!(
                        "{r} (used at `{at}`) is never registered. Derive `ToSchema` on the params struct \
                         too, or call `.schema::<{name}>()` on the router"
                    )
                })
                .collect::<Vec<_>>()
                .join("\n  ")
        );
        let mut router = self.router;
        #[cfg(feature = "validation")]
        {
            let index = std::sync::Arc::new(crate::spec_index::SpecIndex::with_routes(&self.openapi, &self.route_keys));
            if let Some(mode) = self.verify {
                let limit = self.verify_limit.unwrap_or(DEFAULT_VERIFY_BODY_LIMIT);
                router = router.layer(axum::middleware::from_fn_with_state((index.clone(), mode, limit), verify));
            }
            // Lets `ParamsGuard` validate params against the published document.
            router = router.layer(axum::Extension(index));
        }
        if !self.spec_routes.is_empty() {
            let json = std::sync::Arc::new(serde_json::to_value(&self.openapi).expect("OpenAPI serializes"));
            for path in self.spec_routes.iter().collect::<BTreeSet<_>>() {
                let json = json.clone();
                router =
                    router.route(path, axum::routing::get(move || std::future::ready(axum::Json((*json).clone()))));
            }
        }
        (router, self.openapi)
    }
}

#[cfg(feature = "validation")]
use crate::spec_index::Violation;

#[cfg(feature = "validation")]
async fn verify(
    axum::extract::State((index, mode, limit)): axum::extract::State<(
        std::sync::Arc<crate::spec_index::SpecIndex>,
        VerifyMode,
        usize,
    )>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::body::Body;
    use axum::response::{IntoResponse, Response};

    let method = req.method().clone();
    let matched = req.extensions().get::<axum::extract::MatchedPath>().map(|p| p.as_str().to_owned());
    let res = next.run(req).await;
    if method == Method::HEAD {
        return res;
    }
    // The route stamps its id on its response. Middleware that answers before the
    // route runs leaves no id; then match the path (exact, or a unique suffix when this
    // router is nested inside another axum Router).
    let operation = match res.extensions().get::<RouteId>() {
        Some(RouteId(id)) => match index.route(*id) {
            Some(op) => Some(op),
            None => {
                let at = format!("{method} {}", matched.unwrap_or_default());
                let message = format!("{at}: registered route has no published operation");
                report(mode, &at, Violation::new("operation_missing", message));
                return res;
            }
        },
        None => matched.and_then(|m| index.by_matched_path(&method, &m)),
    };
    let Some((doc_method, path)) = operation else { return res };
    let at = format!("{doc_method} {path}");
    let content_type = res.headers().get(axum::http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok());
    // Log mode may run on production traffic: never put response values in the message.
    let redact = mode == VerifyMode::Log;
    // Status and content type: no buffering needed.
    if let Err(violation) = index.check_response_with(&doc_method, &path, res.status(), content_type, None, redact) {
        report(mode, &at, violation);
        return res;
    }
    // Body schema: only for bodies of known length within the limit. Streams and large
    // bodies pass through untouched, so verification never holds them in memory. Each
    // skip is reported, so you can see how much of your traffic had its body checked.
    use axum::body::HttpBody as _;
    let size = res.body().size_hint().exact();
    let skip = if content_type.is_some_and(|ct| ct.starts_with("text/event-stream")) {
        Some("streaming")
    } else {
        match size {
            None => Some("unknown_length"),
            Some(n) if n > limit as u64 => Some("body_too_large"),
            Some(_) => None,
        }
    };
    if let Some(reason) = skip {
        outcome("skipped", reason, &at);
        return res;
    }
    let (parts, body) = res.into_parts();
    let bytes = match axum::body::to_bytes(body, limit).await {
        Ok(b) => b,
        Err(e) => return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let content_type = parts.headers.get(axum::http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok());
    match index.check_response_with(&doc_method, &path, parts.status, content_type, Some(&bytes), redact) {
        Ok(()) => outcome("passed", "body_checked", &at),
        Err(violation) => report(mode, &at, violation),
    }
    Response::from_parts(parts, Body::from(bytes))
}

/// A `tracing` event per verified response (target `openapi_axum::verify`), so a
/// metrics bridge can count checked, skipped and failed responses by `reason`:
/// `openapi.response_validation.passed` / `.skipped` at debug, `.failed` at error.
#[cfg(feature = "validation")]
fn outcome(kind: &'static str, reason: &'static str, operation: &str) {
    #[cfg(feature = "tracing")]
    match kind {
        "passed" => tracing::debug!(
            target: "openapi_axum::verify",
            event = "openapi.response_validation.passed",
            reason,
            operation
        ),
        _ => tracing::debug!(
            target: "openapi_axum::verify",
            event = "openapi.response_validation.skipped",
            reason,
            operation
        ),
    }
    #[cfg(not(feature = "tracing"))]
    let _ = (kind, reason, operation);
}

#[cfg(feature = "validation")]
fn report(mode: VerifyMode, operation: &str, violation: Violation) {
    #[cfg(feature = "tracing")]
    tracing::error!(
        target: "openapi_axum::verify",
        event = "openapi.response_validation.failed",
        reason = violation.reason,
        operation,
        message = %violation.message,
    );
    match mode {
        VerifyMode::Panic => panic!("response contract violated: {}", violation.message),
        VerifyMode::Log => {
            #[cfg(not(feature = "tracing"))]
            eprintln!("response contract violated ({operation}, {}): {}", violation.reason, violation.message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::nested_path;

    #[test]
    fn joins_like_axum() {
        assert_eq!(nested_path("/v1", "/x"), "/v1/x");
        assert_eq!(nested_path("/v1/", "/x"), "/v1/x");
        assert_eq!(nested_path("/v1", "/"), "/v1");
        assert_eq!(nested_path("/v1/", "/"), "/v1/");
        assert_eq!(nested_path("/v1", "/{id}"), "/v1/{id}");
    }
}
