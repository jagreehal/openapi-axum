//! Check a router against its own published document (feature `testing`).
//!
//! [`assert_conforms`] builds a request for every documented operation **using only the
//! document** (path/query/header/cookie params, request body), sends it to the real
//! router, and checks the response against the document. This is how hand-written
//! [`ExtractorDoc`](crate::ExtractorDoc) impls and middleware get verified rather than
//! trusted: if an extractor needs a header its docs don't mention, or a layer returns
//! an undocumented status, the docs-only request exposes it.
//!
//! ```
//! # async fn run() {
//! # use openapi_axum::{ApiRouter, route, routes};
//! # #[route(GET, "/health")] async fn health() -> &'static str { "ok" }
//! let (router, openapi) = ApiRouter::new().routes(routes![health]).split();
//! let report = openapi_axum::testing::assert_conforms(&router, &openapi).await;
//! assert_eq!(report.len(), 1);
//! # }
//! ```

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Map, Value, json};
use tower::ServiceExt;
use utoipa::openapi::OpenApi;

use crate::spec_index::{ArrayStyle, SpecIndex, array_style};

fn resolve<'a>(spec: &'a Value, schema: &'a Value) -> &'a Value {
    match schema.get("$ref").and_then(Value::as_str).and_then(|r| spec.pointer(r.trim_start_matches('#'))) {
        Some(target) => resolve(spec, target),
        None => schema,
    }
}

/// Does `value` satisfy `schema` (resolving refs against `spec`)?
fn valid(spec: &Value, schema: &Value, value: &Value) -> bool {
    crate::spec_index::compile(spec, schema).is_valid(value)
}

/// A minimal value satisfying `schema`: the schema's own `example` if it has one,
/// otherwise generated (required fields only, bounds, formats, enums respected).
/// Always schema-valid when the schema can be satisfied by the generator.
pub fn example(spec: &Value, schema: &Value) -> Value {
    candidates(spec, schema, 1).into_iter().next().unwrap_or(Value::Null)
}

/// Up to `n` **distinct, schema-valid** values for `schema`, most natural first.
/// Fewer are returned when the schema doesn't allow (or the generator can't find) more.
pub fn candidates(spec: &Value, schema: &Value, n: usize) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for c in raw_candidates(spec, schema, n) {
        if out.len() == n {
            break;
        }
        if !out.contains(&c) && valid(spec, schema, &c) {
            out.push(c);
        }
    }
    out
}

/// Unfiltered guesses, in preference order: documented examples, then generated.
fn raw_candidates(spec: &Value, schema: &Value, n: usize) -> Vec<Value> {
    let schema = resolve(spec, schema);
    let mut out: Vec<Value> = Vec::new();
    // Documented examples first: the author knows what a valid value looks like.
    out.extend(schema.get("example").cloned());
    out.extend(schema.get("examples").and_then(Value::as_array).into_iter().flatten().cloned());
    if let Some(c) = schema.get("const") {
        out.push(c.clone());
        return out;
    }
    if let Some(Value::Array(values)) = schema.get("enum") {
        out.extend(values.iter().cloned());
        return out;
    }
    // allOf: every part applies (`#[serde(flatten)]`), so merge their examples.
    if let Some(Value::Array(parts)) = schema.get("allOf") {
        let mut merged = Map::new();
        for part in parts {
            match example(spec, part) {
                Value::Object(fields) => merged.extend(fields),
                other if parts.len() == 1 => return vec![other],
                _ => {}
            }
        }
        out.push(Value::Object(merged));
        return out;
    }
    // oneOf/anyOf: any option will do; non-null options first.
    for key in ["oneOf", "anyOf"] {
        if let Some(Value::Array(options)) = schema.get(key) {
            let (non_null, null): (Vec<&Value>, Vec<&Value>) =
                options.iter().partition(|o| resolve(spec, o).get("type") != Some(&json!("null")));
            for option in non_null.into_iter().chain(null) {
                out.extend(candidates(spec, option, n));
            }
            return out;
        }
    }
    let ty = match schema.get("type") {
        Some(Value::String(t)) => t.as_str(),
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).find(|t| *t != "null").unwrap_or("null"),
        _ if schema.get("properties").is_some() => "object",
        _ => "null",
    };
    // A few more guesses than needed, since some may fail other constraints.
    let tries = n + 8;
    match ty {
        "object" => {
            let required: Vec<&str> =
                schema["required"].as_array().map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let props = schema["properties"].as_object().cloned().unwrap_or_default();
            let fields: Map<String, Value> = props
                .iter()
                .filter(|(k, _)| required.contains(&k.as_str()))
                .map(|(k, s)| (k.clone(), example(spec, s)))
                .collect();
            out.push(Value::Object(fields));
        }
        // The smallest *non-empty* array allowed: an empty array is valid, but a
        // required array query param serialized with repeated keys would vanish.
        "array" => {
            let min = schema.get("minItems").and_then(Value::as_u64).unwrap_or(0) as usize;
            let max = schema.get("maxItems").and_then(Value::as_u64).map_or(usize::MAX, |m| m as usize);
            let len = min.max(1).min(max);
            let items = &schema["items"];
            if schema.get("uniqueItems").and_then(Value::as_bool).unwrap_or(false) {
                // Distinct items that each satisfy the item schema.
                let distinct = candidates(spec, items, len);
                if distinct.len() >= min.max(1).min(max) {
                    out.push(Value::Array(distinct));
                }
            } else {
                out.push(Value::Array(vec![example(spec, items); len]));
            }
            if min == 0 {
                out.push(json!([]));
            }
        }
        "string" => {
            let min = schema.get("minLength").and_then(Value::as_u64).unwrap_or(0) as usize;
            let max = schema.get("maxLength").and_then(Value::as_u64).map_or(usize::MAX, |m| m as usize);
            let len = min.max(1).min(max);
            for i in 0..tries {
                out.push(match schema.get("format").and_then(Value::as_str) {
                    Some("uuid") => json!(format!("00000000-0000-4000-8000-{:012x}", i + 1)),
                    Some("date-time") => json!(format!("2026-01-01T00:00:{:02}Z", i % 60)),
                    Some("date") => json!(format!("2026-01-{:02}", i % 28 + 1)),
                    Some("email") => json!(format!("a{i}@example.com")),
                    _ => {
                        let letter = char::from(b'a' + (i % 26) as u8);
                        json!(std::iter::repeat_n(letter, len).collect::<String>())
                    }
                });
            }
        }
        "integer" | "number" => {
            let bound = |key: &str| schema.get(key).and_then(Value::as_f64);
            let step = bound("multipleOf").unwrap_or(1.0);
            let lo = match (bound("minimum"), bound("exclusiveMinimum")) {
                (Some(m), _) => m,
                (None, Some(m)) => m + step,
                _ => match bound("maximum") {
                    Some(max) if max < 1.0 => max,
                    _ => 1.0,
                },
            };
            let start = (lo / step).ceil() * step;
            for i in 0..tries {
                let v = start + step * i as f64;
                out.push(if ty == "integer" { json!(v as i64) } else { json!(v) });
            }
            // Also try downward from a maximum, for ranges below the default start.
            if let Some(max) = bound("maximum") {
                for i in 0..tries {
                    let v = (max / step).floor() * step - step * i as f64;
                    out.push(if ty == "integer" { json!(v as i64) } else { json!(v) });
                }
            }
        }
        "boolean" => out.extend([json!(true), json!(false)]),
        _ => out.push(Value::Null),
    }
    out
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Every documented `(path, method, operation)`.
pub fn operations(spec: &Value) -> Vec<(String, Method, Value)> {
    let mut ops = Vec::new();
    for (path, item) in spec["paths"].as_object().into_iter().flatten() {
        for (m, op) in item.as_object().into_iter().flatten() {
            if let Ok(method) = m.to_uppercase().parse::<Method>() {
                ops.push((path.clone(), method, op.clone()));
            }
        }
    }
    ops
}

/// A request built only from what the document says this operation needs.
///
/// # Panics
/// If the path has a capture that isn't documented as a parameter.
pub fn request_from_spec(spec: &Value, path: &str, method: &Method, op: &Value) -> Request<Body> {
    let mut uri = path.to_owned();
    let mut query = Vec::new();
    let mut cookies = Vec::new();
    let mut req = Request::builder().method(method.clone());
    for p in op["parameters"].as_array().into_iter().flatten() {
        let p = resolve(spec, p);
        let required = p["required"].as_bool().unwrap_or(false);
        let name = p["name"].as_str().unwrap_or_default();
        let location = p["in"].as_str().unwrap_or_default();
        if location != "path" && !required {
            continue;
        }
        // Serialize the way the parameter declares (style/explode), the same rule
        // the runtime uses to read it back.
        let schema = &p["schema"];
        let value =
            p.get("example").cloned().filter(|e| valid(spec, schema, e)).unwrap_or_else(|| example(spec, schema));
        assert!(
            valid(spec, schema, &value),
            "{method} {path}: couldn't generate a valid value for {location} parameter `{name}` (got {value}); \
             add an `example` to its schema"
        );
        let items: Vec<String> = match &value {
            Value::Array(items) => items.iter().map(scalar).collect(),
            other => vec![scalar(other)],
        };
        let values: Vec<String> = match (value.is_array(), array_style(p)) {
            (true, ArrayStyle::Repeated) => items,
            (true, ArrayStyle::Delimited(sep)) => vec![items.join(&sep.to_string())],
            (false, _) => items,
        };
        match location {
            "path" => uri = uri.replace(&format!("{{{name}}}"), &values.join(",")),
            "query" => query.extend(values.into_iter().map(|v| (name.to_owned(), v))),
            "header" => req = req.header(name, values.join(",")),
            "cookie" => cookies.extend(values.into_iter().map(|v| format!("{name}={v}"))),
            _ => {}
        }
    }
    assert!(!uri.contains('{'), "{method} {path}: a path capture isn't documented as a parameter");
    if !query.is_empty() {
        uri = format!("{uri}?{}", serde_urlencoded::to_string(&query).expect("query encodes"));
    }
    if !cookies.is_empty() {
        req = req.header("cookie", cookies.join("; "));
    }
    // `requestBody` may be a `$ref` to components.requestBodies.
    let content =
        resolve(spec, &op["requestBody"]).get("content").and_then(Value::as_object).and_then(|c| c.iter().next());
    let body = match content {
        Some((ct, media)) => {
            let schema = &media["schema"];
            let value = media
                .get("example")
                .cloned()
                .filter(|e| valid(spec, schema, e))
                .unwrap_or_else(|| example(spec, schema));
            assert!(
                valid(spec, schema, &value),
                "{method} {path}: couldn't generate a valid `{ct}` body (got {value}); add an `example` to its schema"
            );
            req = req.header("content-type", ct.as_str());
            if ct.starts_with("application/x-www-form-urlencoded") {
                let pairs: Vec<(String, String)> =
                    value.as_object().into_iter().flatten().map(|(k, v)| (k.clone(), scalar(v))).collect();
                Body::from(serde_urlencoded::to_string(pairs).expect("form encodes"))
            } else {
                Body::from(value.to_string())
            }
        }
        None => Body::empty(),
    };
    req.uri(uri).body(body).expect("valid request")
}

/// Send `req` to `router` and check the response against the document.
///
/// # Panics
/// On an undocumented status, content type or body shape.
pub async fn send_checked(router: &Router, openapi: &OpenApi, path: &str, req: Request<Body>) -> (StatusCode, Value) {
    let index = SpecIndex::new(openapi);
    let method = req.method().clone();
    let res = router.clone().oneshot(req).await.expect("router is infallible");
    let status = res.status();
    let content_type = res.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let bytes = res.into_body().collect().await.expect("body collects").to_bytes();
    if let Err(violation) = index.check_response(&method, path, status, content_type.as_deref(), &bytes) {
        panic!("{violation}\n  body: {}", String::from_utf8_lossy(&bytes));
    }
    let body = serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, body)
}

/// One documented security scheme's credential placement.
enum Placement {
    Header(String, String),
    Query(String, String),
    Cookie(String, String),
}

/// Where a scheme's credential goes, per its definition in `components.securitySchemes`.
fn placement(spec: &Value, scheme: &str, credential: Option<&str>) -> Option<Placement> {
    let def = &spec["components"]["securitySchemes"][scheme];
    let value = |dummy: &str| credential.unwrap_or(dummy).to_owned();
    match def["type"].as_str()? {
        "apiKey" => {
            let name = def["name"].as_str()?.to_owned();
            match def["in"].as_str()? {
                "header" => Some(Placement::Header(name, value("a"))),
                "query" => Some(Placement::Query(name, value("a"))),
                "cookie" => Some(Placement::Cookie(name, value("a"))),
                _ => None,
            }
        }
        "http" if def["scheme"].as_str()?.eq_ignore_ascii_case("basic") => {
            Some(Placement::Header("authorization".into(), format!("Basic {}", value("YTph"))))
        }
        // http bearer, oauth2, openIdConnect: a bearer token.
        _ => Some(Placement::Header("authorization".into(), format!("Bearer {}", value("a")))),
    }
}

/// Checks a router against its own published document. Build with [`Conformance::new`],
/// or use [`assert_conforms`] for the defaults.
///
/// For every documented operation it sends a request built **only from the document**:
/// documented params and body, plus credentials for the operation's documented security
/// requirement (placed where the scheme says). It then requires:
/// - not rejected as malformed (400/415/422): the docs describe a valid request,
/// - not rejected as unauthenticated (401/403): the docs describe how to authenticate,
/// - the response matches the document (status, content type, body schema).
///
/// Other documented statuses (a 404 for a made-up id) are fine. Credentials can only be
/// supplied for *documented* schemes, so an extractor that needs an undocumented header
/// can't be made to pass.
pub struct Conformance<'a> {
    router: &'a Router,
    openapi: &'a OpenApi,
    credentials: Vec<(String, String)>,
    expected: Vec<(String, StatusCode)>,
}

impl<'a> Conformance<'a> {
    pub fn new(router: &'a Router, openapi: &'a OpenApi) -> Self {
        Self { router, openapi, credentials: Vec::new(), expected: Vec::new() }
    }

    /// A real credential for a documented security scheme (e.g. a test token for
    /// `"bearer"`). Without one, a placeholder is sent, which works for extractors that
    /// only check presence.
    #[must_use]
    pub fn credential(mut self, scheme: &str, value: &str) -> Self {
        self.credentials.push((scheme.to_owned(), value.to_owned()));
        self
    }

    /// Require operation `"GET /path"` to answer the docs-built request with `status`
    /// exactly (e.g. an endpoint that is *meant* to be unauthenticated-hostile).
    #[must_use]
    pub fn expect(mut self, operation: &str, status: StatusCode) -> Self {
        self.expected.push((operation.to_owned(), status));
        self
    }

    /// Run the checks. Returns `("METHOD /path", status)` per operation.
    ///
    /// # Panics
    /// On the first operation that violates one of the rules above.
    pub async fn run(self) -> Vec<(String, StatusCode)> {
        let spec = serde_json::to_value(self.openapi).expect("OpenAPI serializes");
        let mut report = Vec::new();
        for (path, method, op) in operations(&spec) {
            let key = format!("{method} {path}");
            let mut req = request_from_spec(&spec, &path, &method, &op);
            // The first documented requirement (alternatives are separate entries).
            let schemes: Vec<String> = op["security"]
                .get(0)
                .and_then(Value::as_object)
                .map(|r| r.keys().cloned().collect())
                .unwrap_or_default();
            let mut cookies = Vec::new();
            for scheme in &schemes {
                let credential = self.credentials.iter().find(|(s, _)| s == scheme).map(|(_, v)| v.as_str());
                match placement(&spec, scheme, credential) {
                    Some(Placement::Header(name, value)) => {
                        req.headers_mut().insert(
                            axum::http::HeaderName::try_from(name).expect("header name"),
                            value.parse().expect("header value"),
                        );
                    }
                    Some(Placement::Query(name, value)) => {
                        let uri = req.uri().to_string();
                        let sep = if uri.contains('?') { '&' } else { '?' };
                        let pair = serde_urlencoded::to_string([(name, value)]).expect("encodes");
                        *req.uri_mut() = format!("{uri}{sep}{pair}").parse().expect("uri");
                    }
                    Some(Placement::Cookie(name, value)) => cookies.push(format!("{name}={value}")),
                    None => panic!("{key}: security scheme `{scheme}` isn't defined in components.securitySchemes"),
                }
            }
            if !cookies.is_empty() {
                let existing = req.headers().get("cookie").and_then(|v| v.to_str().ok()).map(str::to_owned);
                let all = existing.into_iter().chain(cookies).collect::<Vec<_>>().join("; ");
                req.headers_mut().insert("cookie", all.parse().expect("cookie"));
            }

            let (status, body) = send_checked(self.router, self.openapi, &path, req).await;
            if let Some((_, want)) = self.expected.iter().find(|(k, _)| *k == key) {
                assert_eq!(status, *want, "{key}: expected {want}, got {status}: {body}");
            } else if matches!(status.as_u16(), 400 | 415 | 422) {
                panic!(
                    "{key}: a request built from the docs was rejected ({status}): {body}\n  \
                     the docs don't describe a valid request (missing param/header, wrong body?)"
                );
            } else if matches!(status.as_u16(), 401 | 403) {
                if schemes.is_empty() {
                    panic!(
                        "{key}: answered {status} but documents no security requirement: it needs \
                         authentication the docs don't describe (document it with ctx.security in ExtractorDoc)"
                    );
                }
                panic!(
                    "{key}: credentials for {schemes:?} were rejected ({status}); pass real ones with \
                     `.credential(scheme, value)`, or `.expect(\"{key}\", StatusCode::...)` if that's intended"
                );
            }
            report.push((key, status));
        }
        report
    }
}

/// [`Conformance`] with defaults: placeholder credentials, no expected statuses.
///
/// # Panics
/// See [`Conformance::run`].
pub async fn assert_conforms(router: &Router, openapi: &OpenApi) -> Vec<(String, StatusCode)> {
    Conformance::new(router, openapi).run().await
}
