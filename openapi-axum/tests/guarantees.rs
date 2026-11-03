//! Guarantees beyond the compile-time checks:
//! - query/path constraints and form bodies are validated against the published doc,
//! - middleware and escape hatches are caught by `verify_responses`,
//! - hand-written `ExtractorDoc`s are checked by `testing::assert_conforms`.

use axum::Json;
use axum::body::Body;
use axum::extract::{Form, FromRequestParts, Path, Query};
use axum::http::request::Parts;
use axum::http::{Request, StatusCode};
use openapi_axum::testing::{assert_conforms, send_checked};
use openapi_axum::{ApiResponse, ApiRouter, DocCtx, ExtractorDoc, Rejection, VerifyMode, route, routes};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower::ServiceExt;
use utoipa::openapi::path::Operation;
use utoipa::{IntoParams, ToSchema};

#[derive(Serialize, ToSchema)]
struct Problem {
    message: String,
}

#[derive(ApiResponse)]
enum ApiError {
    /// Invalid input.
    #[status(400)]
    BadRequest(Problem),
}

impl From<Rejection> for ApiError {
    fn from(r: Rejection) -> Self {
        Self::BadRequest(Problem { message: r.message })
    }
}

// ---------- query / path constraints ----------

#[derive(Deserialize, IntoParams)]
#[allow(dead_code)]
struct Paging {
    #[param(minimum = 1, maximum = 100)]
    limit: Option<u32>,
    #[param(pattern = "^[a-z]+$")]
    q: Option<String>,
    /// Repeated keys: `?tag=a&tag=b`.
    #[param(max_items = 2)]
    tag: Option<Vec<String>>,
}

#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
struct Slug {
    #[schema(min_length = 3, max_length = 8)]
    slug: String,
}

#[route(GET, "/items")]
async fn list(Query(_p): Query<Paging>) -> Result<Json<u8>, ApiError> {
    Ok(Json(1))
}

#[route(GET, "/items/{slug}")]
async fn by_slug(Path(_s): Path<Slug>) -> Result<Json<u8>, ApiError> {
    Ok(Json(1))
}

// ---------- form bodies ----------

#[derive(Serialize, Deserialize, ToSchema)]
struct Signup {
    #[schema(format = "email")]
    email: String,
    #[schema(minimum = 18, maximum = 130)]
    age: u32,
    newsletter: Option<bool>,
}

#[route(POST, "/signup")]
async fn signup(Form(s): Form<Signup>) -> Result<Json<Signup>, ApiError> {
    Ok(Json(s))
}

#[route(GET, "/signup")]
async fn signup_get(Form(s): Form<Signup>) -> Result<Json<Signup>, ApiError> {
    Ok(Json(s))
}

fn app() -> axum::Router {
    ApiRouter::new().routes(routes![list, by_slug, signup, signup_get]).split().0
}

async fn status_and_message(req: Request<Body>) -> (StatusCode, String) {
    let res = app().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body["message"].as_str().unwrap_or_default().to_owned())
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

fn form(body: &str) -> Request<Body> {
    Request::post("/signup")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

#[tokio::test]
async fn query_and_path_constraints_are_enforced() {
    for (uri, expect) in [
        ("/items?limit=0", "query parameter `limit`"),
        ("/items?limit=101", "query parameter `limit`"),
        ("/items?q=ABC", "query parameter `q`"),
        ("/items?tag=a&tag=b&tag=c", "query parameter `tag`"),
        ("/items/ab", "path parameter `slug`"),
        ("/items/waytoolongslug", "path parameter `slug`"),
    ] {
        let (status, message) = status_and_message(get(uri)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(message.contains(expect), "{uri}: {message}");
    }
    for uri in ["/items", "/items?limit=100&q=abc&tag=a&tag=b", "/items/okay"] {
        assert_eq!(status_and_message(get(uri)).await.0, StatusCode::OK, "{uri}");
    }
}

#[tokio::test]
async fn form_bodies_are_validated_with_coercion() {
    for (body, expect) in [
        ("email=a@example.com&age=17", "field `age`"),
        ("email=a@example.com&age=old", "field `age`"),
        ("email=not-an-email&age=30", "field `email`"),
        ("email=a@example.com&age=30&newsletter=maybe", "field `newsletter`"),
        ("age=30", "email"),
    ] {
        let (status, message) = status_and_message(form(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(message.contains(expect), "{body}: {message}");
    }
    assert_eq!(status_and_message(form("email=a@example.com&age=30&newsletter=true")).await.0, StatusCode::OK);
    // GET forms are query params: same constraints via the params guard.
    assert_eq!(status_and_message(get("/signup?email=a@example.com&age=17")).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(status_and_message(get("/signup?email=a@example.com&age=40")).await.0, StatusCode::OK);
}

// ---------- verify_responses: middleware and escape hatches ----------

#[route(GET, "/ok")]
async fn ok() -> Json<u8> {
    Json(1)
}

#[route(POST, "/raw")]
async fn raw(#[raw] Json(s): Json<Signup>) -> Json<u32> {
    Json(s.age)
}

async fn teapot_middleware(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    if req.headers().contains_key("x-teapot") {
        return StatusCode::IM_A_TEAPOT.into_response_compat();
    }
    next.run(req).await
}

trait Compat {
    fn into_response_compat(self) -> axum::response::Response;
}

impl Compat for StatusCode {
    fn into_response_compat(self) -> axum::response::Response {
        axum::response::IntoResponse::into_response(self)
    }
}

fn verified() -> axum::Router {
    ApiRouter::new()
        .routes(routes![ok, raw])
        .layer(axum::middleware::from_fn(teapot_middleware))
        .verify_responses(VerifyMode::Panic)
        .split()
        .0
}

async fn panics(req: Request<Body>) -> String {
    let fut = verified().oneshot(req);
    let res = tokio::spawn(fut).await;
    match res {
        Err(e) if e.is_panic() => {
            let p = e.into_panic();
            p.downcast_ref::<String>().cloned().unwrap_or_default()
        }
        other => panic!("expected a contract violation, got {:?}", other.map(|r| r.map(|r| r.status()))),
    }
}

#[tokio::test]
async fn verify_catches_undocumented_middleware_responses() {
    let msg = panics(Request::get("/ok").header("x-teapot", "1").body(Body::empty()).unwrap()).await;
    assert!(msg.contains("GET /ok -> 418 I'm a teapot: status is not documented"), "{msg}");
    // A conforming response passes through untouched.
    let res = verified().oneshot(get("/ok")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn verify_catches_raw_escape_hatch_responses() {
    // `#[raw]` keeps axum's 422 text rejection, which isn't documented.
    let req = Request::post("/raw").header("content-type", "application/json").body(Body::from("{}")).unwrap();
    let msg = panics(req).await;
    assert!(msg.contains("POST /raw -> 422"), "{msg}");
}

// ---------- assert_conforms: hand-written ExtractorDoc is checked ----------

/// Documents `x-api-key` and also requires `x-tenant`, which its docs leave out.
struct PartialDocAuth;

impl<S: Send + Sync> FromRequestParts<S> for PartialDocAuth {
    type Rejection = StatusCode;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, StatusCode> {
        let ok = parts.headers.contains_key("x-api-key") && parts.headers.contains_key("x-tenant");
        ok.then_some(Self).ok_or(StatusCode::BAD_REQUEST)
    }
}

impl ExtractorDoc for PartialDocAuth {
    fn document(ctx: &mut DocCtx, op: &mut Operation) {
        ctx.header_param::<String>(op, "x-api-key", true, None);
        ctx.empty_response(&mut op.responses, 400, Some("Missing API key"));
    }
}

#[route(GET, "/secret")]
async fn secret(_a: PartialDocAuth) -> Json<u8> {
    Json(1)
}

/// Documents both headers.
struct FullDocAuth;

impl<S: Send + Sync> FromRequestParts<S> for FullDocAuth {
    type Rejection = StatusCode;
    async fn from_request_parts(parts: &mut Parts, s: &S) -> Result<Self, StatusCode> {
        PartialDocAuth::from_request_parts(parts, s).await.map(|_| Self)
    }
}

impl ExtractorDoc for FullDocAuth {
    fn document(ctx: &mut DocCtx, op: &mut Operation) {
        ctx.header_param::<String>(op, "x-api-key", true, None);
        ctx.header_param::<String>(op, "x-tenant", true, None);
        ctx.empty_response(&mut op.responses, 400, Some("Missing credentials"));
    }
}

#[route(GET, "/secret")]
async fn documented_secret(_a: FullDocAuth) -> Json<u8> {
    Json(1)
}

#[tokio::test]
async fn assert_conforms_catches_an_incomplete_extractor_doc() {
    let (router, openapi) = ApiRouter::new().routes(routes![secret]).split();
    let result = tokio::spawn(async move { assert_conforms(&router, &openapi).await }).await;
    let msg = result.expect_err("incomplete docs must fail").into_panic();
    let msg = msg.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(msg.contains("GET /secret: a request built from the docs was rejected (400"), "{msg}");

    let (router, openapi) = ApiRouter::new().routes(routes![documented_secret]).split();
    let report = assert_conforms(&router, &openapi).await;
    assert_eq!(report.statuses(), [("GET /secret".to_owned(), StatusCode::OK)]);
}

#[tokio::test]
async fn assert_conforms_passes_on_documented_apps() {
    let (router, openapi) = ApiRouter::new().routes(routes![list, by_slug, signup, signup_get]).split();
    let report = assert_conforms(&router, &openapi).await;
    assert_eq!(report.len(), 4);
    assert!(report.statuses().iter().all(|(_, s)| s.is_success()), "{report}");
    let _ = send_checked;
}

// ---------- regressions from review: name collisions, published bodies, hidden auth ----------

#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
struct ItemPath {
    #[schema(maximum = 10)]
    id: u32,
}

#[derive(Deserialize, IntoParams)]
#[allow(dead_code)]
struct ItemQuery {
    #[param(maximum = 1000)]
    id: Option<u32>,
}

#[derive(Deserialize, IntoParams)]
#[allow(dead_code)]
struct RequiredItemQuery {
    #[param(maximum = 1000)]
    id: u32,
}

#[route(GET, "/items/{id}/detail")]
async fn item_detail(Path(_p): Path<ItemPath>, Query(_q): Query<ItemQuery>) -> Result<Json<u8>, ApiError> {
    Ok(Json(1))
}

#[route(GET, "/items/{id}/strict")]
async fn item_strict(Path(_p): Path<ItemPath>, Query(_q): Query<RequiredItemQuery>) -> Result<Json<u8>, ApiError> {
    Ok(Json(1))
}

#[tokio::test]
async fn same_name_in_path_and_query_is_validated_per_location() {
    // Both required: split() must not trip over the shared name.
    let router = ApiRouter::new().routes(routes![item_detail, item_strict]).split().0;
    let call = |uri: &'static str| {
        let router = router.clone();
        async move { router.oneshot(get(uri)).await.unwrap().status() }
    };
    assert_eq!(call("/items/999/detail?id=1").await, StatusCode::BAD_REQUEST, "path id > 10");
    assert_eq!(call("/items/5/detail?id=5000").await, StatusCode::BAD_REQUEST, "query id > 1000");
    assert_eq!(call("/items/5/detail?id=999").await, StatusCode::OK, "query id may exceed the path limit");
    assert_eq!(call("/items/5/strict?id=999").await, StatusCode::OK);
    assert_eq!(call("/items/5/strict").await, StatusCode::BAD_REQUEST, "required query id missing");
}

#[derive(Deserialize, ToSchema)]
struct Input {
    value: u32,
}

#[route(POST, "/input")]
async fn input(Json(p): Json<Input>) -> Result<Json<u32>, ApiError> {
    Ok(Json(p.value))
}

#[route(POST, "/input-form")]
async fn input_form(Form(p): Form<Input>) -> Result<Json<u32>, ApiError> {
    Ok(Json(p.value))
}

#[tokio::test]
async fn bodies_are_validated_against_the_final_published_document() {
    let router = ApiRouter::new()
        .routes(routes![input, input_form])
        .map_openapi(|o| {
            let mut v = serde_json::to_value(o).unwrap();
            v["components"]["schemas"]["Input"]["properties"]["value"]["maximum"] = serde_json::json!(10);
            serde_json::from_value(v).unwrap()
        })
        .split()
        .0;
    let json = |body: &str| {
        Request::post("/input").header("content-type", "application/json").body(Body::from(body.to_owned())).unwrap()
    };
    let form = |body: &str| {
        Request::post("/input-form")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body.to_owned()))
            .unwrap()
    };
    assert_eq!(router.clone().oneshot(json(r#"{"value":999}"#)).await.unwrap().status(), StatusCode::BAD_REQUEST);
    assert_eq!(router.clone().oneshot(json(r#"{"value":7}"#)).await.unwrap().status(), StatusCode::OK);
    assert_eq!(router.clone().oneshot(form("value=999")).await.unwrap().status(), StatusCode::BAD_REQUEST);
    assert_eq!(router.oneshot(form("value=7")).await.unwrap().status(), StatusCode::OK);
}

/// Needs `x-secret` but documents only its 401: authentication the docs don't describe.
struct HiddenAuth;

impl<S: Send + Sync> FromRequestParts<S> for HiddenAuth {
    type Rejection = StatusCode;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, StatusCode> {
        parts.headers.contains_key("x-secret").then_some(Self).ok_or(StatusCode::UNAUTHORIZED)
    }
}

impl ExtractorDoc for HiddenAuth {
    fn document(ctx: &mut DocCtx, op: &mut Operation) {
        ctx.empty_response(&mut op.responses, 401, None);
    }
}

#[route(GET, "/auth")]
async fn hidden_auth(_a: HiddenAuth) -> Json<u32> {
    Json(1)
}

/// Same check, documented properly as an API key scheme.
struct DocumentedAuth;

impl<S: Send + Sync> FromRequestParts<S> for DocumentedAuth {
    type Rejection = StatusCode;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, StatusCode> {
        let ok = parts.headers.get("x-secret").is_some_and(|v| v == "letmein");
        ok.then_some(Self).ok_or(StatusCode::UNAUTHORIZED)
    }
}

impl ExtractorDoc for DocumentedAuth {
    fn document(ctx: &mut DocCtx, op: &mut Operation) {
        use utoipa::openapi::security::{ApiKey, ApiKeyValue, SecurityScheme};
        ctx.security(op, "secret", &[], SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new("x-secret"))));
        ctx.empty_response(&mut op.responses, 401, None);
    }
}

#[route(GET, "/auth")]
async fn documented_auth(_a: DocumentedAuth) -> Json<u32> {
    Json(1)
}

async fn conformance_panic(
    f: impl std::future::Future<Output = openapi_axum::testing::Report> + Send + 'static,
) -> String {
    let err = tokio::spawn(f).await.expect_err("should fail").into_panic();
    err.downcast_ref::<String>().cloned().unwrap_or_default()
}

#[tokio::test]
async fn conformance_rejects_hidden_authentication() {
    let (router, openapi) = ApiRouter::new().routes(routes![hidden_auth]).split();
    let msg = conformance_panic(async move { assert_conforms(&router, &openapi).await }).await;
    assert!(msg.contains("GET /auth: answered 401 Unauthorized but documents no security requirement"), "{msg}");
}

#[tokio::test]
async fn conformance_uses_documented_credentials() {
    use openapi_axum::testing::Conformance;
    // A placeholder credential is rejected by a real check: told how to fix it.
    let (router, openapi) = ApiRouter::new().routes(routes![documented_auth]).split();
    let (r, o) = (router.clone(), openapi.clone());
    let msg = conformance_panic(async move { assert_conforms(&r, &o).await }).await;
    assert!(msg.contains(r#"credentials for ["secret"] were rejected"#), "{msg}");
    // With a real credential for the *documented* scheme, it passes.
    let report = Conformance::new(&router, &openapi).credential("secret", "letmein").run().await;
    assert_eq!(report.statuses(), [("GET /auth".to_owned(), StatusCode::OK)]);
    // Or the outcome can be declared explicitly.
    let report = Conformance::new(&router, &openapi).expect("GET /auth", StatusCode::UNAUTHORIZED).run().await;
    assert_eq!(report.statuses(), [("GET /auth".to_owned(), StatusCode::UNAUTHORIZED)]);
}

// ---------- flatten, referenced bodies, empty text ----------

#[derive(Serialize, Deserialize, ToSchema)]
struct Inner {
    #[schema(maximum = 10)]
    value: u32,
}

#[derive(Serialize, Deserialize, ToSchema)]
struct Flat {
    #[serde(flatten)]
    inner: Inner,
    name: String,
}

#[route(POST, "/flat")]
async fn flat_form(Form(f): Form<Flat>) -> Result<Json<Flat>, ApiError> {
    Ok(Json(f))
}

#[route(GET, "/flat")]
async fn flat_query(Query(f): Query<Flat>) -> Result<Json<Flat>, ApiError> {
    Ok(Json(f))
}

#[tokio::test]
async fn flattened_fields_are_coerced_validated_and_documented() {
    let (router, openapi) = ApiRouter::new().routes(routes![flat_form, flat_query]).split();
    let post = |body: &'static str| {
        Request::post("/flat")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap()
    };
    assert_eq!(router.clone().oneshot(post("value=5&name=a")).await.unwrap().status(), StatusCode::OK);
    assert_eq!(router.clone().oneshot(post("value=50&name=a")).await.unwrap().status(), StatusCode::BAD_REQUEST);
    assert_eq!(router.clone().oneshot(post("value=x&name=a")).await.unwrap().status(), StatusCode::BAD_REQUEST);
    assert_eq!(router.clone().oneshot(get("/flat?value=5&name=a")).await.unwrap().status(), StatusCode::OK);
    assert_eq!(router.clone().oneshot(get("/flat?value=50&name=a")).await.unwrap().status(), StatusCode::BAD_REQUEST);

    let spec = serde_json::to_value(&openapi).unwrap();
    let mut names: Vec<&str> = spec["paths"]["/flat"]["get"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["name", "value"], "flattened fields documented as query params");
    assert_conforms(&router, &openapi).await;
}

#[tokio::test]
async fn referenced_request_bodies_are_validated() {
    let router = ApiRouter::new()
        .routes(routes![input])
        .map_openapi(|o| {
            // Move the body into components.requestBodies (with a tighter maximum) and $ref it.
            let mut v = serde_json::to_value(o).unwrap();
            let mut body = v["paths"]["/input"]["post"]["requestBody"].take();
            body["content"]["application/json"]["schema"] =
                serde_json::json!({ "type": "object", "required": ["value"], "properties": { "value": { "type": "integer", "maximum": 10 } } });
            v["components"]["requestBodies"] = serde_json::json!({ "Input": body });
            v["paths"]["/input"]["post"]["requestBody"] = serde_json::json!({ "$ref": "#/components/requestBodies/Input" });
            serde_json::from_value(v).unwrap()
        })
        .split()
        .0;
    let json = |body: &'static str| {
        Request::post("/input").header("content-type", "application/json").body(Body::from(body)).unwrap()
    };
    assert_eq!(router.clone().oneshot(json(r#"{"value":999}"#)).await.unwrap().status(), StatusCode::BAD_REQUEST);
    assert_eq!(router.oneshot(json(r#"{"value":7}"#)).await.unwrap().status(), StatusCode::OK);
}

#[route(GET, "/empty-text")]
async fn empty_text() -> String {
    String::new()
}

async fn strip_body(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let res = next.run(req).await;
    let (mut parts, _) = res.into_parts();
    parts.headers.remove("content-type");
    parts.headers.remove("content-length");
    axum::response::Response::from_parts(parts, Body::empty())
}

#[tokio::test]
async fn empty_text_is_a_valid_body_but_missing_bodies_are_not() {
    let router = ApiRouter::new().routes(routes![empty_text]).verify_responses(VerifyMode::Panic).split().0;
    let res = router.oneshot(get("/empty-text")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "\"\" satisfies the string schema");

    // A documented JSON body that middleware drops entirely is still a violation.
    let stripped = ApiRouter::new()
        .routes(routes![ok])
        .layer(axum::middleware::from_fn(strip_body))
        .verify_responses(VerifyMode::Panic)
        .split()
        .0;
    let err = tokio::spawn(stripped.oneshot(get("/ok"))).await.expect_err("should panic").into_panic();
    let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(msg.contains("GET /ok -> 200 OK: documented body is missing"), "{msg}");
}

// ---------- harness/runtime agree on referenced bodies and array params ----------

fn with_referenced_body(o: utoipa::openapi::OpenApi) -> utoipa::openapi::OpenApi {
    let mut v = serde_json::to_value(o).unwrap();
    let body = v["paths"]["/input"]["post"]["requestBody"].take();
    v["components"]["requestBodies"] = serde_json::json!({ "Input": body });
    v["paths"]["/input"]["post"]["requestBody"] = serde_json::json!({ "$ref": "#/components/requestBodies/Input" });
    serde_json::from_value(v).unwrap()
}

#[tokio::test]
async fn harness_sends_referenced_request_bodies() {
    let (router, openapi) = ApiRouter::new().routes(routes![input]).map_openapi(with_referenced_body).split();
    let report = assert_conforms(&router, &openapi).await;
    assert_eq!(report.statuses(), [("POST /input".to_owned(), StatusCode::OK)]);
}

#[derive(Deserialize, IntoParams)]
#[allow(dead_code)]
struct Ids {
    #[param(min_items = 2)]
    ids: Vec<u32>,
}

#[route(GET, "/by-ids")]
async fn by_ids(Query(q): Query<Ids>) -> Result<Json<Vec<u32>>, ApiError> {
    Ok(Json(q.ids))
}

async fn body_of(router: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let res = router.clone().oneshot(get(uri)).await.unwrap();
    let status = res.status();
    let bytes = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn exploded_array_params_round_trip() {
    let (router, openapi) = ApiRouter::new().routes(routes![by_ids]).split();
    assert_eq!(body_of(&router, "/by-ids?ids=1&ids=2").await, (StatusCode::OK, serde_json::json!([1, 2])));
    assert_eq!(body_of(&router, "/by-ids?ids=1&ids=x").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(body_of(&router, "/by-ids?ids=1").await.0, StatusCode::BAD_REQUEST, "min_items = 2");
    assert_eq!(assert_conforms(&router, &openapi).await.statuses(), [("GET /by-ids".to_owned(), StatusCode::OK)]);
}

#[tokio::test]
async fn delimited_array_params_round_trip() {
    // `explode: false` (comma-delimited), as another generator or a spec edit would declare it.
    let (router, openapi) = ApiRouter::new()
        .routes(routes![by_ids])
        .map_openapi(|o| {
            let mut v = serde_json::to_value(o).unwrap();
            v["paths"]["/by-ids"]["get"]["parameters"][0]["explode"] = serde_json::json!(false);
            serde_json::from_value(v).unwrap()
        })
        .split();
    assert_eq!(body_of(&router, "/by-ids?ids=1,2").await, (StatusCode::OK, serde_json::json!([1, 2])));
    assert_eq!(body_of(&router, "/by-ids?ids=1,x").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(body_of(&router, "/by-ids?ids=1").await.0, StatusCode::BAD_REQUEST, "min_items = 2");
    assert_eq!(assert_conforms(&router, &openapi).await.statuses(), [("GET /by-ids".to_owned(), StatusCode::OK)]);
}

#[derive(Deserialize, IntoParams)]
#[allow(dead_code)]
struct AnyIds {
    /// Required, no `min_items`: an empty example would vanish from the query string.
    ids: Vec<u32>,
}

#[route(GET, "/any-ids")]
async fn any_ids(Query(q): Query<AnyIds>) -> Result<Json<Vec<u32>>, ApiError> {
    Ok(Json(q.ids))
}

#[tokio::test]
async fn required_arrays_without_min_items_are_sent() {
    let (router, openapi) = ApiRouter::new().routes(routes![any_ids]).split();
    assert_eq!(body_of(&router, "/any-ids?ids=1").await, (StatusCode::OK, serde_json::json!([1])));
    assert_eq!(assert_conforms(&router, &openapi).await.statuses(), [("GET /any-ids".to_owned(), StatusCode::OK)]);
}

#[test]
fn array_examples_respect_bounds_and_uniqueness() {
    use openapi_axum::testing::example;
    let spec = serde_json::json!({});
    let ints = serde_json::json!({ "type": "integer" });
    let arr = |extra: Value| {
        let mut s = serde_json::json!({ "type": "array", "items": ints });
        s.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        example(&spec, &s)
    };
    assert_eq!(arr(serde_json::json!({})), serde_json::json!([1]), "non-empty by default");
    assert_eq!(arr(serde_json::json!({ "maxItems": 0 })), serde_json::json!([]), "but never above maxItems");
    assert_eq!(arr(serde_json::json!({ "minItems": 3 })).as_array().unwrap().len(), 3);
    assert_eq!(arr(serde_json::json!({ "minItems": 3, "uniqueItems": true })), serde_json::json!([1, 2, 3]));
}

#[test]
fn unique_array_items_are_distinct_and_schema_valid() {
    use openapi_axum::testing::{candidates, example};
    let spec = serde_json::json!({});
    let unique = |items: Value, min: u64| {
        example(&spec, &serde_json::json!({ "type": "array", "uniqueItems": true, "minItems": min, "items": items }))
    };
    // The reported case: enum items, two distinct required.
    assert_eq!(
        unique(serde_json::json!({ "type": "string", "enum": ["asc", "desc"] }), 2),
        serde_json::json!(["asc", "desc"])
    );
    // Numeric bounds and multipleOf are respected.
    assert_eq!(
        unique(serde_json::json!({ "type": "integer", "minimum": 5, "multipleOf": 5 }), 2),
        serde_json::json!([5, 10])
    );
    assert_eq!(unique(serde_json::json!({ "type": "integer", "maximum": -3 }), 2), serde_json::json!([-3, -4]));
    // Strings keep their length bounds.
    assert_eq!(
        unique(serde_json::json!({ "type": "string", "minLength": 2, "maxLength": 2 }), 3),
        serde_json::json!(["aa", "bb", "cc"])
    );
    // Booleans: only two distinct values exist.
    assert_eq!(unique(serde_json::json!({ "type": "boolean" }), 2), serde_json::json!([true, false]));
    // Impossible: three distinct values from a two-value enum -> no valid array.
    assert_eq!(unique(serde_json::json!({ "enum": ["asc", "desc"] }), 3), Value::Null);
    // Documented examples win, and are validated like everything else.
    let s = serde_json::json!({ "type": "string", "pattern": "^x[0-9]+$", "examples": ["nope", "x7"] });
    assert_eq!(candidates(&spec, &s, 2), [serde_json::json!("x7")]);
}

#[derive(Deserialize, ToSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)]
enum Direction {
    Asc,
    Desc,
}

#[derive(Deserialize, IntoParams)]
#[allow(dead_code)]
struct Sorting {
    #[param(min_items = 2)]
    dirs: Vec<Direction>,
}

#[route(GET, "/sorted")]
async fn sorted(Query(q): Query<Sorting>) -> Result<Json<usize>, ApiError> {
    Ok(Json(q.dirs.len()))
}

#[derive(Deserialize, IntoParams)]
#[allow(dead_code)]
struct TooStrict {
    #[param(min_items = 3)]
    dirs: Vec<Direction>,
}

#[route(GET, "/too-strict")]
async fn too_strict(Query(_q): Query<TooStrict>) -> Result<Json<u8>, ApiError> {
    Ok(Json(1))
}

/// utoipa's `#[param]` has no `unique_items`; set it the way a spec edit would.
fn unique_dirs(path: &'static str) -> impl FnOnce(utoipa::openapi::OpenApi) -> utoipa::openapi::OpenApi {
    move |o| {
        let mut v = serde_json::to_value(o).unwrap();
        v["paths"][path]["get"]["parameters"][0]["schema"]["uniqueItems"] = serde_json::json!(true);
        serde_json::from_value(v).unwrap()
    }
}

#[tokio::test]
async fn conformance_with_unique_enum_arrays() {
    let (router, openapi) =
        ApiRouter::new().routes(routes![sorted]).schema::<Direction>().map_openapi(unique_dirs("/sorted")).split();
    assert_eq!(assert_conforms(&router, &openapi).await.statuses(), [("GET /sorted".to_owned(), StatusCode::OK)]);

    // Unsatisfiable by generation: the harness says so instead of sending a bad request.
    let (router, openapi) = ApiRouter::new()
        .routes(routes![too_strict])
        .schema::<Direction>()
        .map_openapi(unique_dirs("/too-strict"))
        .split();
    let msg = conformance_panic(async move { assert_conforms(&router, &openapi).await }).await;
    assert!(msg.contains("couldn't generate a valid value for query parameter `dirs`"), "{msg}");
    assert!(msg.contains("add an `example`"), "{msg}");
}

// ---------- security review: lookups survive outer nesting, fail closed, HEAD ----------

/// The split router mounted inside a plain `axum::Router` under a prefix it doesn't know.
fn nested_in_plain_axum(router: axum::Router) -> axum::Router {
    axum::Router::new().nest("/api", router)
}

#[tokio::test]
async fn validation_still_applies_when_nested_in_a_plain_axum_router() {
    let inner = ApiRouter::new()
        .routes(routes![list, input])
        .map_openapi(|o| {
            let mut v = serde_json::to_value(o).unwrap();
            v["components"]["schemas"]["Input"]["properties"]["value"]["maximum"] = serde_json::json!(10);
            serde_json::from_value(v).unwrap()
        })
        .split()
        .0;
    let app = nested_in_plain_axum(inner);
    // Query constraints (limit <= 100) still hold under /api.
    assert_eq!(app.clone().oneshot(get("/api/items?limit=101")).await.unwrap().status(), StatusCode::BAD_REQUEST);
    assert_eq!(app.clone().oneshot(get("/api/items?limit=5")).await.unwrap().status(), StatusCode::OK);
    // Body checks use the published (edited) document.
    let json = |b: &'static str| {
        Request::post("/api/input").header("content-type", "application/json").body(Body::from(b)).unwrap()
    };
    assert_eq!(app.clone().oneshot(json(r#"{"value":999}"#)).await.unwrap().status(), StatusCode::BAD_REQUEST);
    assert_eq!(app.oneshot(json(r#"{"value":7}"#)).await.unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn verify_still_applies_when_nested_in_a_plain_axum_router() {
    let inner = ApiRouter::new()
        .routes(routes![ok, raw])
        .layer(axum::middleware::from_fn(teapot_middleware))
        .verify_responses(VerifyMode::Panic)
        .split()
        .0;
    let app = nested_in_plain_axum(inner);
    // Undocumented status from middleware that answers before the route runs.
    let req = Request::get("/api/ok").header("x-teapot", "1").body(Body::empty()).unwrap();
    let err = tokio::spawn(app.clone().oneshot(req)).await.expect_err("should panic").into_panic();
    assert!(err.downcast_ref::<String>().unwrap().contains("418"), "middleware response checked");
    // Undocumented status from the route itself (`#[raw]` keeps axum's 422).
    let req = Request::post("/api/raw").header("content-type", "application/json").body(Body::from("{}")).unwrap();
    let err = tokio::spawn(app.oneshot(req)).await.expect_err("should panic").into_panic();
    assert!(err.downcast_ref::<String>().unwrap().contains("422"), "route response checked");
}

#[tokio::test]
async fn head_requests_are_validated_against_the_get_operation() {
    let router = ApiRouter::new().routes(routes![list]).split().0;
    let head = |uri: &'static str| Request::head(uri).body(Body::empty()).unwrap();
    assert_eq!(router.clone().oneshot(head("/items?limit=101")).await.unwrap().status(), StatusCode::BAD_REQUEST);
    assert_eq!(router.oneshot(head("/items?limit=5")).await.unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn routes_missing_from_the_published_document_fail_closed() {
    let router = ApiRouter::new()
        .routes(routes![list])
        .map_openapi(|mut o| {
            o.paths.paths.clear(); // the document no longer describes GET /items
            o
        })
        .split()
        .0;
    let (status, message) = {
        let res = router.oneshot(get("/items?limit=5")).await.unwrap();
        let status = res.status();
        let bytes = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    };
    // The handler maps every Rejection to its 400; what matters is that it's rejected.
    assert_eq!(status, StatusCode::BAD_REQUEST, "{message}");
    assert!(message.contains("no operation in the published OpenAPI document"), "{message}");
}

// ---------- verify_responses: bounded buffering, streams untouched ----------

/// Replaces the body: `x-body: small|large|stream` (all invalid for `Json<u8>`),
/// optional `x-ct` content type.
async fn rewrite_body(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let (kind, ct) = (
        req.headers().get("x-body").map(|v| v.to_str().unwrap().to_owned()),
        req.headers().get("x-ct").map(|v| v.to_str().unwrap().to_owned()),
    );
    let res = next.run(req).await;
    let Some(kind) = kind else { return res };
    let (mut parts, _) = res.into_parts();
    parts.headers.remove("content-length");
    if let Some(ct) = ct {
        parts.headers.insert("content-type", ct.parse().unwrap());
    }
    let invalid = format!("\"{}\"", "x".repeat(if kind == "large" { 4096 } else { 8 }));
    let body = match kind.as_str() {
        "stream" => {
            let chunks: Vec<Result<String, std::io::Error>> = vec![Ok(invalid[..4].into()), Ok(invalid[4..].into())];
            Body::from_stream(futures_util::stream::iter(chunks))
        }
        _ => Body::from(invalid),
    };
    axum::response::Response::from_parts(parts, body)
}

fn bounded() -> axum::Router {
    ApiRouter::new()
        .routes(routes![ok])
        .layer(axum::middleware::from_fn(rewrite_body))
        .verify_responses(VerifyMode::Panic)
        .verify_body_limit(1024)
        .split()
        .0
}

fn rewritten(kind: &str, ct: Option<&str>) -> Request<Body> {
    let mut req = Request::get("/ok").header("x-body", kind);
    if let Some(ct) = ct {
        req = req.header("x-ct", ct);
    }
    req.body(Body::empty()).unwrap()
}

async fn outcome(req: Request<Body>) -> Result<(StatusCode, usize), String> {
    match tokio::spawn(async move {
        let res = bounded().oneshot(req).await.unwrap();
        let status = res.status();
        let len = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes().len();
        (status, len)
    })
    .await
    {
        Ok(ok) => Ok(ok),
        Err(e) => Err(e.into_panic().downcast_ref::<String>().cloned().unwrap_or_default()),
    }
}

#[tokio::test]
async fn small_bodies_are_still_schema_checked() {
    let err = outcome(rewritten("small", None)).await.expect_err("invalid small body is caught");
    assert!(err.contains("body doesn't match its schema"), "{err}");
}

#[tokio::test]
async fn bodies_over_the_limit_pass_through_unbuffered() {
    // 4 KiB > 1 KiB limit: not buffered, not schema-checked, delivered intact.
    assert_eq!(outcome(rewritten("large", None)).await, Ok((StatusCode::OK, 4098)));
}

#[tokio::test]
async fn streamed_bodies_pass_through_unbuffered() {
    assert_eq!(outcome(rewritten("stream", None)).await, Ok((StatusCode::OK, 10)));
    let sse = outcome(rewritten("small", Some("text/event-stream"))).await;
    assert!(sse.as_ref().is_err_and(|e| e.contains("content-type `text/event-stream` is not documented")), "{sse:?}");
}

#[tokio::test]
async fn status_and_content_type_are_checked_without_buffering() {
    // Large and streamed bodies still get their content type checked.
    for kind in ["large", "stream"] {
        let err = outcome(rewritten(kind, Some("application/xml"))).await.expect_err(kind);
        assert!(err.contains("content-type `application/xml` is not documented"), "{kind}: {err}");
    }
}

#[tokio::test]
async fn body_limit_applies_in_either_builder_order() {
    // Limit 1024 < the 4 KiB invalid body, so it must pass through unchecked. With the
    // default 1 MiB it would be checked and panic: passing proves the limit applied.
    let builders: [fn() -> axum::Router; 2] = [
        || {
            ApiRouter::new()
                .routes(routes![ok])
                .layer(axum::middleware::from_fn(rewrite_body))
                .verify_body_limit(1024)
                .verify_responses(VerifyMode::Panic)
                .split()
                .0
        },
        || {
            ApiRouter::new()
                .routes(routes![ok])
                .layer(axum::middleware::from_fn(rewrite_body))
                .verify_responses(VerifyMode::Panic)
                .verify_body_limit(1024)
                .split()
                .0
        },
    ];
    for (order, build) in builders.into_iter().enumerate() {
        let res = tokio::spawn(async move { build().oneshot(rewritten("large", None)).await.unwrap().status() }).await;
        assert!(matches!(res, Ok(StatusCode::OK)), "order {order}: the 1024-byte limit was not applied");
    }
}

// ---------- HeaderText: header validity checked at construction ----------

#[derive(ApiResponse)]
enum Moved {
    /// See the new location.
    #[status(303)]
    SeeOther {
        #[header("Location")]
        location: openapi_axum::HeaderText,
    },
}

#[route(GET, "/moved/{to}")]
async fn moved(Path(to): Path<String>) -> Result<Moved, ApiError> {
    // The invalid case is the handler's documented error, not a surprise 500.
    let location = openapi_axum::HeaderText::new(format!("/{to}"))
        .map_err(|e| ApiError::BadRequest(Problem { message: e.to_string() }))?;
    Ok(Moved::SeeOther { location })
}

#[tokio::test]
async fn header_text_is_validated_where_it_is_built() {
    let (router, openapi) = ApiRouter::new().routes(routes![moved]).verify_responses(VerifyMode::Panic).split();
    let res = router.clone().oneshot(get("/moved/new")).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()["location"], "/new");
    // `%0A` decodes to a newline in the path: HeaderText refuses it, the handler maps
    // that to its documented 400, and verify_responses sees a documented response.
    let res = router.oneshot(get("/moved/a%0Ab")).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let spec = serde_json::to_value(&openapi).unwrap();
    assert_eq!(
        spec["paths"]["/moved/{to}"]["get"]["responses"]["303"]["headers"]["Location"]["schema"]["type"],
        "string"
    );
}

// ---------- axum's DefaultBodyLimit still applies, as a documented error ----------

#[derive(ApiResponse)]
enum SizedError {
    /// Invalid input.
    #[status(400)]
    BadRequest(Problem),
    /// Body over the configured limit.
    #[status(413)]
    TooLarge(Problem),
}

impl From<Rejection> for SizedError {
    fn from(r: Rejection) -> Self {
        let problem = Problem { message: r.message };
        if r.status == StatusCode::PAYLOAD_TOO_LARGE { Self::TooLarge(problem) } else { Self::BadRequest(problem) }
    }
}

#[route(POST, "/limited")]
async fn limited(Json(p): Json<Input>) -> Result<Json<u32>, SizedError> {
    Ok(Json(p.value))
}

#[tokio::test]
async fn default_body_limit_applies_before_validation() {
    let router = ApiRouter::new()
        .routes(routes![limited])
        .layer(axum::extract::DefaultBodyLimit::max(16))
        .verify_responses(VerifyMode::Panic)
        .split()
        .0;
    let post = |body: String| {
        Request::post("/limited").header("content-type", "application/json").body(Body::from(body)).unwrap()
    };
    // Over the limit: axum's 413 reaches the error type as `Rejection { status: 413 }`.
    let res = router.clone().oneshot(post(format!(r#"{{"value":{}}}"#, "1".repeat(32)))).await.unwrap();
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    // Under the limit: validated and handled as usual.
    let res = router.oneshot(post(r#"{"value":7}"#.into())).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

// ---------- generated cases: valid variants and invalid values ----------

#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
struct NewTag {
    #[schema(min_length = 1, max_length = 10)]
    name: String,
}

/// Documents a constrained JSON body and accepts any bytes.
struct LaxBody;

impl<S: Send + Sync> axum::extract::FromRequest<S> for LaxBody {
    type Rejection = StatusCode;
    async fn from_request(req: axum::extract::Request, s: &S) -> Result<Self, StatusCode> {
        axum::body::Bytes::from_request(req, s).await.map(|_| Self).map_err(|_| StatusCode::BAD_REQUEST)
    }
}

impl ExtractorDoc for LaxBody {
    fn document(ctx: &mut DocCtx, op: &mut Operation) {
        ctx.json_body::<NewTag>(op);
        ctx.empty_response(&mut op.responses, 422, Some("Invalid tag"));
    }
}

#[route(POST, "/tags")]
async fn lax_tags(_b: LaxBody) -> Json<u8> {
    Json(1)
}

async fn conformance_panic_of(router: axum::Router, openapi: utoipa::openapi::OpenApi) -> String {
    let err = tokio::spawn(async move { assert_conforms(&router, &openapi).await }).await.expect_err("should fail");
    err.into_panic().downcast_ref::<String>().cloned().unwrap_or_default()
}

#[tokio::test]
async fn generated_invalid_values_catch_unenforced_constraints() {
    let (router, openapi) = ApiRouter::new().routes(routes![lax_tags]).split();
    // The docs-built request is valid, so one request per operation passes.
    openapi_axum::testing::Conformance::new(&router, &openapi).cases(0).run().await;
    let msg = conformance_panic_of(router, openapi).await;
    assert!(msg.contains("POST /tags (generated invalid request, body: "), "{msg}");
    assert!(msg.contains("answered 200 OK"), "{msg}");
    assert!(msg.contains("doesn't enforce what the docs promise"), "{msg}");
}

#[derive(Deserialize, IntoParams, ToSchema)]
#[into_params(parameter_in = Query)]
#[allow(dead_code)]
struct Page {
    /// Optional, so the docs-built request leaves it out.
    #[param(maximum = 50)]
    limit: Option<u32>,
}

#[route(GET, "/pages")]
async fn pages(Query(_p): Query<Page>) -> Result<Json<u8>, ApiError> {
    Ok(Json(1))
}

/// Rejects any valid `limit` with the documented 400. Only a request that sends a valid
/// `limit` sees it.
async fn breaks_on_limit(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let limit = req.uri().query().and_then(|q| q.strip_prefix("limit=")).and_then(|v| v.parse::<u32>().ok());
    if limit.is_some_and(|l| l <= 50) {
        let body = Json(serde_json::json!({ "message": "limit unsupported" }));
        return axum::response::IntoResponse::into_response((StatusCode::BAD_REQUEST, body));
    }
    next.run(req).await
}

#[tokio::test]
async fn generated_valid_variants_send_optional_inputs() {
    let build = || ApiRouter::new().routes(routes![pages]).layer(axum::middleware::from_fn(breaks_on_limit)).split();
    let (router, openapi) = build();
    openapi_axum::testing::Conformance::new(&router, &openapi).cases(0).run().await;
    let (router, openapi) = build();
    let msg = conformance_panic_of(router, openapi).await;
    assert!(msg.contains("GET /pages (generated valid request: query limit="), "{msg}");
    assert!(msg.contains("a request built from the docs was rejected (400"), "{msg}");
}

#[tokio::test]
async fn generated_cases_pass_on_an_enforcing_handler() {
    // Optional `limit` gets valid values (incl. the 50 boundary) and invalid ones
    // (51, "x"); validation rejects the invalid ones with the documented 400.
    let (router, openapi) = ApiRouter::new().routes(routes![pages]).split();
    assert_eq!(assert_conforms(&router, &openapi).await.statuses(), [("GET /pages".to_owned(), StatusCode::OK)]);
}

#[test]
fn invalid_values_are_labelled_and_fail_their_schema() {
    let spec = serde_json::json!({});
    let schema = serde_json::json!({
        "type": "object",
        "required": ["name", "age"],
        "properties": {
            "name": { "type": "string", "minLength": 1, "maxLength": 3 },
            "age": { "type": "integer", "minimum": 0, "maximum": 130 },
            "tags": { "type": "array", "items": { "type": "string" }, "maxItems": 2, "uniqueItems": true }
        },
        "additionalProperties": false
    });
    let bad = openapi_axum::testing::invalid_values(&spec, &schema, false);
    let labels: Vec<&str> = bad.iter().map(|(l, _)| l.as_str()).collect();
    for want in [
        "missing required `name`",
        "missing required `age`",
        "`name`: shorter than minLength",
        "`name`: longer than maxLength",
        "`age`: below minimum",
        "`age`: above maximum",
        "`age`: not an integer",
        "`tags`: more than maxItems",
        "`tags`: duplicate items",
        "unexpected field",
    ] {
        assert!(labels.contains(&want), "missing {want:?} in {labels:?}");
    }
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(bad.iter().all(|(_, v)| !validator.is_valid(v)), "{bad:?}");
    // As a parameter, only same-type breaks count: `"x"` for a number, bounds.
    let wire =
        openapi_axum::testing::invalid_values(&spec, &serde_json::json!({"type": "integer", "maximum": 5}), true);
    assert_eq!(
        wire,
        [("above maximum".to_owned(), serde_json::json!(6)), ("not an integer".into(), serde_json::json!("x"))]
    );
}

#[derive(Serialize, Deserialize, ToSchema)]
struct Node {
    value: u8,
    /// Optional and recursive. utoipa needs `no_recursion` on recursive fields.
    #[schema(no_recursion)]
    next: Option<Box<Node>>,
    #[schema(no_recursion)]
    children: Option<Vec<Node>>,
}

#[test]
fn recursive_optional_schemas_terminate() {
    let mut schemas = Vec::new();
    <Node as ToSchema>::schemas(&mut schemas);
    schemas.push(("Node".into(), <Node as utoipa::PartialSchema>::schema()));
    let components: serde_json::Map<String, Value> =
        schemas.into_iter().map(|(n, s)| (n, serde_json::to_value(s).unwrap())).collect();
    let spec = serde_json::json!({ "components": { "schemas": components } });
    let node = serde_json::json!({ "$ref": "#/components/schemas/Node" });
    assert_eq!(openapi_axum::testing::example(&spec, &node), serde_json::json!({ "value": 0 }));
    let many = openapi_axum::testing::candidates(&spec, &node, 8);
    assert!(many.len() > 1 && many.iter().any(|v| v.get("next").is_some()), "{many:?}");
    assert!(!openapi_axum::testing::invalid_values(&spec, &node, false).is_empty());
}

#[route(POST, "/nodes")]
async fn nodes(Json(n): Json<Node>) -> Result<Json<Node>, ApiError> {
    Ok(Json(n))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
#[allow(dead_code)]
struct Code {
    #[param(pattern = "^[a-z]{2}$", example = "ab")]
    code: String,
}

#[route(GET, "/codes/{code}")]
async fn code(Path(c): Path<Code>) -> Result<Json<String>, ApiError> {
    Ok(Json(c.code))
}

#[tokio::test]
async fn generated_cases_cover_recursive_bodies_and_encoded_paths() {
    // `\u{1}!` breaks the pattern; percent-encoded, it reaches the router and gets the 400.
    let (router, openapi) = ApiRouter::new().routes(routes![nodes, code]).split();
    assert_eq!(assert_conforms(&router, &openapi).await.len(), 2);
}

// ---------- conformance reports, fixtures and hooks ----------

#[derive(ApiResponse)]
enum StoreError {
    /// Invalid input.
    #[status(400)]
    BadRequest(Problem),
    /// No item with that id.
    #[status(404)]
    NotFound(Problem),
}

impl From<Rejection> for StoreError {
    fn from(r: Rejection) -> Self {
        Self::BadRequest(Problem { message: r.message })
    }
}

#[route(GET, "/items/{id}")]
/// Only item 42 exists.
async fn item(Path(id): Path<u32>) -> Result<Json<u32>, StoreError> {
    if id == 42 { Ok(Json(id)) } else { Err(StoreError::NotFound(Problem { message: "no such item".into() })) }
}

#[tokio::test]
async fn the_report_shows_what_was_exercised_and_skipped() {
    let (router, openapi) = ApiRouter::new().routes(routes![item, code, secret_header]).split();
    let report = assert_conforms(&router, &openapi).await;
    let item = report.get("GET /items/{id}").unwrap();
    // Generated ids never hit the one that exists.
    assert_eq!(item.status, StatusCode::NOT_FOUND);
    assert_eq!(item.not_exercised, ["200"], "{report}");
    assert!(item.valid_cases > 0 && item.invalid_cases > 0, "{report}");
    let code = report.get("GET /codes/{code}").unwrap();
    assert!(code.skipped.iter().any(|s| s.contains("path parameter `code` uses its example only")), "{report}");
    let secret = report.get("GET /secret-header").unwrap();
    assert!(secret.skipped.iter().any(|s| s.contains("header parameter `x-api-key` isn't generated")), "{report}");
    let text = report.to_string();
    assert!(text.contains("GET /items/{id}:") && text.contains("not exercised: 200"), "{text}");
}

#[tokio::test]
async fn fixtures_reach_existing_data() {
    let (router, openapi) = ApiRouter::new().routes(routes![item]).split();
    let report = openapi_axum::testing::Conformance::new(&router, &openapi).fixture("id", 42).run().await;
    let item = report.get("GET /items/{id}").unwrap();
    assert_eq!(item.status, StatusCode::OK, "{report}");
    assert!(item.statuses.contains(&200) && item.statuses.contains(&400), "{report}");
    // Per-operation fixtures win over the shared one.
    let report = openapi_axum::testing::Conformance::new(&router, &openapi)
        .fixture("id", 7)
        .fixture_for("GET /items/{id}", "id", 42)
        .run()
        .await;
    assert_eq!(report.get("GET /items/{id}").unwrap().status, StatusCode::OK);
}

#[tokio::test]
async fn fixtures_must_match_their_schema() {
    let (router, openapi) = ApiRouter::new().routes(routes![item]).split();
    let err = tokio::spawn(async move {
        openapi_axum::testing::Conformance::new(&router, &openapi).fixture("id", "forty-two").run().await
    })
    .await
    .expect_err("an invalid fixture fails");
    let msg = err.into_panic().downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(msg.contains("the value supplied for path parameter `id` doesn't match its schema"), "{msg}");
}

#[tokio::test]
async fn body_fixtures_and_before_each_hooks() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let (router, openapi) = ApiRouter::new().routes(routes![nodes]).split();
    let counter = calls.clone();
    let report = openapi_axum::testing::Conformance::new(&router, &openapi)
        .body_for("POST /nodes", serde_json::json!({ "value": 9, "next": { "value": 10 } }))
        .before_each(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        })
        .run()
        .await;
    let nodes = report.get("POST /nodes").unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1 + nodes.valid_cases + nodes.invalid_cases);
}

/// Documents a required `x-api-key` header.
struct ApiKey;

impl<S: Send + Sync> FromRequestParts<S> for ApiKey {
    type Rejection = StatusCode;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, StatusCode> {
        parts.headers.contains_key("x-api-key").then_some(Self).ok_or(StatusCode::BAD_REQUEST)
    }
}

impl ExtractorDoc for ApiKey {
    fn document(ctx: &mut DocCtx, op: &mut Operation) {
        ctx.header_param::<String>(op, "x-api-key", true, None);
        ctx.empty_response(&mut op.responses, 400, Some("Missing API key"));
    }
}

#[route(GET, "/secret-header")]
async fn secret_header(_k: ApiKey) -> Json<u8> {
    Json(1)
}

#[tokio::test]
async fn send_checked_accepts_head_responses() {
    let (router, openapi) = ApiRouter::new().routes(routes![item]).split();
    let req = Request::head("/items/42").body(Body::empty()).unwrap();
    let (status, _) = send_checked(&router, &openapi, "/items/{id}", req).await;
    assert_eq!(status, StatusCode::OK);
}
