//! A kitchen-sink API exercising every supported shape, checked four ways:
//! 1. the document is valid OpenAPI 3.1 (official meta-schema) with no dangling refs,
//! 2. a golden snapshot (like Dropshot's `test_openapi.json`) so any doc change is reviewed,
//! 3. every operation accepts a request built *only from its docs*, and every response
//!    conforms to them (schemathesis-lite),
//! 4. invalid input gets a documented error.

mod support;

use axum::Json;
use axum::body::Body;
use axum::extract::{Form, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{Request, StatusCode};
use openapi_axum::{Accepted, ApiResponse, ApiRouter, DocCtx, ExtractorDoc, NoContent, Rejection, route, routes};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::openapi::path::Operation;
use utoipa::openapi::security::{ApiKey, ApiKeyValue, SecurityScheme};
use utoipa::{IntoParams, ToSchema};

// ---------- models ----------

/// A pet.
#[derive(Serialize, Deserialize, ToSchema, Clone)]
struct Pet {
    id: u32,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
}

fn pet(id: u32) -> Pet {
    Pet { id, name: "Rex".into(), tag: None }
}

#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
struct NewPet {
    #[schema(min_length = 1, max_length = 20)]
    name: String,
    #[schema(minimum = 0, maximum = 30)]
    age: Option<u8>,
    kind: Kind,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Dog,
    Cat,
}

#[derive(Serialize, ToSchema)]
struct Problem {
    code: String,
    message: String,
}

#[derive(ApiResponse)]
enum ApiError {
    /// Invalid input.
    #[status(BAD_REQUEST)]
    BadRequest(Problem),
    /// Not found.
    #[status(NOT_FOUND)]
    NotFound(Problem),
}

impl From<Rejection> for ApiError {
    fn from(r: Rejection) -> Self {
        Self::BadRequest(Problem { code: "BAD_REQUEST".into(), message: r.message })
    }
}

#[derive(ApiResponse)]
enum CreatedPet {
    /// Created.
    #[status(CREATED)]
    Created {
        #[header("Location")]
        location: String,
        #[header("X-Optional-Trace")]
        trace: Option<String>,
        #[body]
        pet: Pet,
    },
}

#[derive(ApiResponse)]
enum Page<T> {
    /// A page of results.
    #[status(200)]
    Ok(Vec<T>),
}

#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
struct Filter {
    /// Name prefix.
    q: Option<String>,
    limit: Option<u32>,
}

#[derive(Deserialize, IntoParams, ToSchema)]
#[allow(dead_code)]
struct Search {
    /// Required search term.
    term: String,
    /// An enum field: a `$ref` that must resolve.
    kind: Option<Kind>,
}

#[derive(Deserialize, ToSchema)]
struct Repo {
    org: String,
    repo: String,
}

#[derive(Deserialize, IntoParams)]
struct Pair {
    a: u32,
    b: String,
}

#[derive(Serialize, Deserialize, ToSchema)]
struct Signup {
    email: String,
    #[schema(minimum = 18)]
    age: u32,
}

// ---------- an auth extractor documented through ExtractorDoc ----------

struct Key;

impl<S: Send + Sync> FromRequestParts<S> for Key {
    type Rejection = StatusCode;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, StatusCode> {
        parts.headers.contains_key("x-api-key").then_some(Self).ok_or(StatusCode::UNAUTHORIZED)
    }
}

impl ExtractorDoc for Key {
    fn document(ctx: &mut DocCtx, op: &mut Operation) {
        ctx.security(op, "key", &[], SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new("x-api-key"))));
        ctx.header_param::<String>(op, "x-api-key", true, Some("API key"));
        ctx.empty_response(&mut op.responses, 401, Some("Missing API key"));
    }
}

// ---------- routes: one of every shape ----------

#[route(GET, "/health", tag = "meta")]
/// Liveness.
async fn health() -> &'static str {
    "ok"
}

#[route(GET, "/pets", tags = ["pets", "read"])]
/// List pets.
///
/// Filtered by an optional prefix.
async fn list_pets(Query(_f): Query<Filter>) -> Result<Json<Vec<Pet>>, ApiError> {
    Ok(Json(vec![pet(1)]))
}

#[route(GET, "/pets/search")]
async fn search_pets(Query(_s): Query<Search>) -> Result<Page<Pet>, ApiError> {
    Ok(Page::Ok(vec![pet(1)]))
}

#[route(GET, "/pets/{id}")]
async fn get_pet(Path(id): Path<u32>) -> Result<Json<Pet>, ApiError> {
    match id {
        999 => Err(ApiError::NotFound(Problem { code: "NOT_FOUND".into(), message: "no pet 999".into() })),
        _ => Ok(Json(pet(id))),
    }
}

#[route(POST, "/pets")]
async fn create_pet(_k: Key, Json(p): Json<NewPet>) -> Result<CreatedPet, ApiError> {
    let pet = Pet { id: 9, name: p.name, tag: None };
    Ok(CreatedPet::Created { location: format!("/pets/{}", pet.id), trace: None, pet })
}

#[route(PUT, "/pets/{id}")]
async fn update_pet(Path(id): Path<u32>, Json(p): Json<NewPet>) -> Result<Accepted<Pet>, ApiError> {
    Ok(Accepted(Pet { id, name: p.name, tag: None }))
}

#[route(DELETE, "/pets/{id}")]
async fn delete_pet(Path(_id): Path<u32>) -> Result<NoContent, ApiError> {
    Ok(NoContent)
}

#[route(GET, "/owners/{owner}/pets/{id}")]
async fn owner_pet(Path((_owner, id)): Path<(String, u32)>) -> Result<Json<Pet>, ApiError> {
    Ok(Json(pet(id)))
}

#[route(GET, "/by-uuid/{id}")]
async fn by_uuid(Path(id): Path<uuid::Uuid>) -> Result<Json<String>, ApiError> {
    Ok(Json(id.to_string()))
}

#[route(GET, "/repos/{org}/{repo}")]
async fn repo(Path(r): Path<Repo>) -> Result<Json<String>, ApiError> {
    Ok(Json(format!("{}/{}", r.org, r.repo)))
}

#[route(GET, "/pairs/{a}/{b}")]
async fn pair(Path(p): Path<Pair>) -> Result<Json<String>, ApiError> {
    Ok(Json(format!("{}{}", p.a, p.b)))
}

#[route(POST, "/signup")]
async fn signup_form(Form(s): Form<Signup>) -> Result<Json<Signup>, ApiError> {
    Ok(Json(s))
}

#[route(GET, "/signup")]
/// GET forms are read from the query string, so they're documented as query params.
async fn signup_query(Form(s): Form<Signup>) -> Result<Json<Signup>, ApiError> {
    Ok(Json(s))
}

#[route(GET, "/counter")]
async fn counter(State(n): State<u32>) -> Json<u32> {
    Json(n)
}

#[route(POST, "/raw")]
async fn raw(#[raw] Json(p): Json<Signup>) -> Json<String> {
    Json(p.email)
}

#[route(GET, "/old")]
#[deprecated = "use /pets"]
async fn old() -> Json<u8> {
    Json(0)
}

#[route(POST, "/admin/stats")]
/// Registered on the parent at the same path the nested router adds `GET` to.
async fn reset_stats() -> NoContent {
    NoContent
}

mod admin {
    use super::*;

    #[route(GET, "/stats", operation_id = "admin_stats")]
    pub async fn stats() -> Json<u32> {
        Json(1)
    }
}

fn kitchen_sink() -> (axum::Router, Value) {
    let (router, spec) = ApiRouter::new()
        .map_openapi(|mut o| {
            o.info.title = "kitchen sink".into();
            o.info.version = "1.0.0".into();
            o
        })
        .routes(routes![
            health,
            list_pets,
            search_pets,
            get_pet,
            create_pet,
            update_pet,
            delete_pet,
            owner_pet,
            by_uuid,
            repo,
            pair,
            signup_form,
            signup_query,
            counter,
            raw,
            old,
            reset_stats
        ])
        .nest("/admin", ApiRouter::new().routes(routes![admin::stats]))
        .spec_route("/openapi.json")
        .verify_responses(openapi_axum::VerifyMode::Panic) // every test request is also verified
        .with_state(7u32)
        .split();
    (router, serde_json::to_value(spec).unwrap())
}

// ---------- 1. valid OpenAPI ----------

#[test]
fn document_is_valid_openapi_3_1() {
    support::assert_valid_document(&kitchen_sink().1);
}

// ---------- 2. golden snapshot ----------

#[test]
fn golden_spec() {
    let actual = serde_json::to_string_pretty(&kitchen_sink().1).unwrap() + "\n";
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots/kitchen_sink.json");
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(path, &actual).unwrap();
    }
    let expected = std::fs::read_to_string(path).unwrap_or_default();
    assert!(
        expected == actual,
        "tests/snapshots/kitchen_sink.json changed: review, then UPDATE_SNAPSHOTS=1 cargo test"
    );
}

// ---------- 3. docs → request → runtime → docs ----------

#[tokio::test]
async fn every_operation_accepts_a_request_built_from_its_docs() {
    let (router, spec) = kitchen_sink();
    let ops = support::operations(&spec);
    assert_eq!(ops.len(), 18, "every route is documented (incl. both methods on /admin/stats)");
    for (path, method, op) in ops {
        let req = support::request_from_spec(&spec, &path, &method, &op);
        let (status, body) = support::send_checked(&router, &spec, &path, req).await;
        assert!(status.is_success(), "{method} {path}: docs-only request got {status}: {body}");
    }
}

#[tokio::test]
async fn library_conformance_harness_agrees() {
    let (router, spec) = kitchen_sink();
    let openapi: utoipa::openapi::OpenApi = serde_json::from_value(spec).unwrap();
    let report = openapi_axum::testing::assert_conforms(&router, &openapi).await;
    assert_eq!(report.len(), 18);
}

// ---------- 4. invalid input → documented errors ----------

#[tokio::test]
async fn invalid_input_gets_documented_errors() {
    let (router, spec) = kitchen_sink();
    let post = |uri: &str, ct: &str, body: &str| {
        Request::post(uri)
            .header("content-type", ct)
            .header("x-api-key", "k")
            .body(Body::from(body.to_owned()))
            .unwrap()
    };
    let cases = [
        ("/pets", post("/pets", "application/json", r#"{"kind":"dog"}"#)), // missing field
        ("/pets", post("/pets", "application/json", r#"{"name":"","kind":"dog"}"#)), // minLength
        ("/pets", post("/pets", "application/json", r#"{"name":"a","kind":"cow"}"#)), // enum
        ("/pets", post("/pets", "application/json", r#"{"name":"a","kind":"dog","age":99}"#)), // maximum
        ("/pets", post("/pets", "application/json", "{nope")),             // syntax
        ("/pets", post("/pets", "text/plain", "{}")),                      // content type
        ("/pets/{id}", Request::get("/pets/abc").body(Body::empty()).unwrap()), // path type
        ("/by-uuid/{id}", Request::get("/by-uuid/nope").body(Body::empty()).unwrap()),
        ("/pets/{id}", Request::get("/pets/999").body(Body::empty()).unwrap()), // domain 404
        ("/pets/search", Request::get("/pets/search").body(Body::empty()).unwrap()), // missing required query
        ("/signup", post("/signup", "application/x-www-form-urlencoded", "email=a")), // form missing field
        ("/signup", Request::get("/signup?email=a&age=x").body(Body::empty()).unwrap()), // GET form bad type
    ];
    for (path, req) in cases {
        let label = format!("{} {}", req.method(), req.uri());
        let (status, body) = support::send_checked(&router, &spec, path, req).await;
        assert!(status.is_client_error(), "{label}: {status}");
        let expected = if status == StatusCode::NOT_FOUND { "NOT_FOUND" } else { "BAD_REQUEST" };
        assert_eq!(body["code"], expected, "{label}: {body}");
    }
    // The auth extractor's own 401 is documented on the route that uses it.
    let req = Request::post("/pets").header("content-type", "application/json").body(Body::from("{}")).unwrap();
    assert_eq!(support::send_checked(&router, &spec, "/pets", req).await.0, StatusCode::UNAUTHORIZED);
}

#[test]
fn get_and_head_forms_are_query_parameters() {
    let spec = kitchen_sink().1;
    let get = &spec["paths"]["/signup"]["get"];
    assert!(get.get("requestBody").is_none(), "no body on GET: {get}");
    let mut names: Vec<&str> =
        get["parameters"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
    names.sort_unstable(); // utoipa orders properties alphabetically unless `preserve_order` is on
    assert_eq!(names, ["age", "email"]);
    assert!(get["parameters"].as_array().unwrap().iter().all(|p| p["in"] == "query"));
    assert!(
        spec["paths"]["/signup"]["post"]["requestBody"]["content"]["application/x-www-form-urlencoded"].is_object()
    );
}

// ---------- nesting matches axum's routing exactly ----------

mod nested {
    use super::*;

    #[route(GET, "/")]
    pub async fn root() -> Json<u8> {
        Json(1)
    }

    #[route(GET, "/x")]
    pub async fn x() -> Json<u8> {
        Json(2)
    }

    #[route(GET, "/x/{id}")]
    pub async fn x_id(Path(id): Path<u32>) -> Result<Json<u32>, ApiError> {
        Ok(Json(id))
    }
}

#[tokio::test]
async fn nest_matrix_matches_axum_routing() {
    for prefix in ["/v1", "/v1/", "/a/b"] {
        for which in 0..3 {
            let inner = match which {
                0 => ApiRouter::new().routes(routes![nested::root]),
                1 => ApiRouter::new().routes(routes![nested::x]),
                _ => ApiRouter::new().routes(routes![nested::x_id]),
            };
            let (router, spec) = ApiRouter::new().nest(prefix, inner).split();
            let spec = serde_json::to_value(spec).unwrap();
            support::assert_valid_document(&spec);
            for (path, method, op) in support::operations(&spec) {
                let req = support::request_from_spec(&spec, &path, &method, &op);
                let (status, _) = support::send_checked(&router, &spec, &path, req).await;
                assert_eq!(status, StatusCode::OK, "nest({prefix:?}) documented {path} but axum doesn't serve it");
            }
        }
    }
}

#[test]
fn captured_nest_prefixes_are_rejected() {
    let err = std::panic::catch_unwind(|| {
        drop(ApiRouter::<()>::new().nest("/tenant/{tenant}", ApiRouter::new().routes(routes![nested::x_id])))
    })
    .expect_err("should panic");
    let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(msg.contains("can't nest under `/tenant/{tenant}`"), "{msg}");
}

#[tokio::test]
async fn spec_route_serves_the_document() {
    let (router, spec) = kitchen_sink();
    let res =
        tower::ServiceExt::oneshot(router, Request::get("/openapi.json").body(Body::empty()).unwrap()).await.unwrap();
    let bytes = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes();
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), spec);
}
