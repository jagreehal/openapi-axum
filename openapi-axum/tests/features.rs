//! The former "known gaps": validation, security/headers via ExtractorDoc, typed
//! response headers, generic responses, ToSchema-struct params, Vec<T> refs, custom extractors.

use axum::Json;
use axum::body::Body;
use axum::extract::{FromRequestParts, Path, Query};
use axum::http::request::Parts;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use openapi_axum::{ApiResponse, ApiRouter, DocCtx, ExtractorDoc, Rejection, route, routes};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tower::ServiceExt;
use utoipa::ToSchema;
use utoipa::openapi::path::Operation;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};

#[derive(Serialize, Deserialize, ToSchema, Clone)]
struct Pet {
    id: u32,
    name: String,
}

#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
struct NewPet {
    #[schema(min_length = 1, max_length = 10)]
    name: String,
    #[schema(minimum = 0, maximum = 30)]
    age: Option<u8>,
    owner: Option<uuid::Uuid>,
}

#[derive(Serialize, ToSchema)]
struct Problem {
    message: String,
}

#[derive(ApiResponse)]
enum PetError {
    /// Bad input.
    #[status(400)]
    BadRequest(Problem),
}

impl From<Rejection> for PetError {
    fn from(r: Rejection) -> Self {
        Self::BadRequest(Problem { message: r.message })
    }
}

/// Typed response headers: the field is sent *and* documented.
#[derive(ApiResponse)]
enum CreatedPet {
    /// Created.
    #[status(201)]
    Created {
        #[header("Location")]
        location: openapi_axum::HeaderText,
        #[header("X-Request-Id")]
        request_id: Option<openapi_axum::HeaderText>,
        #[body]
        pet: Pet,
    },
}

/// Generic response enums.
#[derive(ApiResponse)]
enum Page<T> {
    #[status(200)]
    Ok(Vec<T>),
}

// ---------- a custom auth extractor documented via ExtractorDoc ----------

struct Bearer(#[allow(dead_code)] String);

impl<S: Send + Sync> FromRequestParts<S> for Bearer {
    type Rejection = StatusCode;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, StatusCode> {
        let header = parts.headers.get("authorization").and_then(|v| v.to_str().ok());
        header.and_then(|h| h.strip_prefix("Bearer ")).map(|t| Self(t.into())).ok_or(StatusCode::UNAUTHORIZED)
    }
}

impl ExtractorDoc for Bearer {
    fn document(ctx: &mut DocCtx, op: &mut Operation) {
        let scheme = SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build());
        ctx.security(op, "bearer", &[], scheme);
        ctx.header_param::<String>(op, "Authorization", true, Some("Bearer token"));
        ctx.empty_response(&mut op.responses, 401, Some("Missing or invalid token"));
    }
}

// ---------- a non-axum extractor that shares a name ----------

mod extra {
    use axum::extract::FromRequestParts;
    use axum::http::{StatusCode, request::Parts};

    /// Same name as axum's `Query`, different type: must not be wrapped as axum's.
    pub struct Query<T>(pub T);

    impl<S: Send + Sync> FromRequestParts<S> for Query<u8> {
        type Rejection = StatusCode;
        async fn from_request_parts(_: &mut Parts, _: &S) -> Result<Self, StatusCode> {
            Ok(Self(1))
        }
    }
}

// ---------- ToSchema structs as params (no IntoParams) ----------

#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
struct Filter {
    /// Name prefix.
    q: String,
    limit: Option<u32>,
}

#[derive(Deserialize, ToSchema)]
struct OwnerPet {
    owner: String,
    id: u32,
}

#[route(POST, "/pets")]
async fn create_pet(_auth: Bearer, Json(p): Json<NewPet>) -> Result<CreatedPet, PetError> {
    let pet = Pet { id: 7, name: p.name };
    let location = openapi_axum::HeaderText::new(format!("/pets/{}", pet.id))
        .map_err(|e| PetError::BadRequest(Problem { message: e.to_string() }))?;
    Ok(CreatedPet::Created { location, request_id: None, pet })
}

#[route(GET, "/pets")]
async fn list_pets(Query(f): Query<Filter>) -> Result<Json<Vec<Pet>>, PetError> {
    Ok(Json(vec![Pet { id: 1, name: f.q }]))
}

#[route(GET, "/page")]
async fn page() -> Page<Pet> {
    Page::Ok(vec![])
}

#[route(GET, "/owners/{owner}/pets/{id}")]
async fn owner_pet(Path(p): Path<OwnerPet>) -> Result<Json<String>, PetError> {
    Ok(Json(format!("{}/{}", p.owner, p.id)))
}

#[route(GET, "/extra")]
async fn custom_named_query(extra::Query(n): extra::Query<u8>) -> Json<u8> {
    Json(n)
}

fn app() -> (axum::Router, Value) {
    let (router, spec) =
        ApiRouter::new().routes(routes![create_pet, list_pets, page, owner_pet, custom_named_query]).split();
    (router, serde_json::to_value(spec).unwrap())
}

fn create(body: Value) -> Request<Body> {
    Request::post("/pets")
        .header("content-type", "application/json")
        .header("authorization", "Bearer t")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn call(req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Value) {
    let res = app().0.oneshot(req).await.unwrap();
    let (status, headers) = (res.status(), res.headers().clone());
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, headers, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn bodies_are_validated_against_the_published_schema() {
    for (bad, field) in [
        (json!({ "name": "" }), "name"),
        (json!({ "name": "much-too-long-name" }), "name"),
        (json!({ "name": "Rex", "age": 99 }), "age"),
        (json!({ "name": "Rex", "owner": "not-a-uuid" }), "owner"),
    ] {
        let (status, _, body) = call(create(bad.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert!(body["message"].as_str().unwrap().starts_with(&format!("field `{field}`")), "{bad} -> {body}");
    }
    let (status, ..) = call(create(json!({ "name": "Rex", "age": 3, "owner": uuid::Uuid::new_v4() }))).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn typed_response_headers_are_sent_and_documented() {
    let (status, headers, body) = call(create(json!({ "name": "Rex" }))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["location"], "/pets/7");
    assert!(headers.get("x-request-id").is_none(), "None optional header is not sent");
    assert_eq!(body["name"], "Rex");

    let (_, spec) = app();
    let created = &spec["paths"]["/pets"]["post"]["responses"]["201"];
    assert_eq!(created["headers"]["Location"]["schema"]["type"], "string");
    assert!(created["headers"]["X-Request-Id"].is_object());
}

#[tokio::test]
async fn auth_extractor_documents_security_and_401() {
    let (_, spec) = app();
    let op = &spec["paths"]["/pets"]["post"];
    assert_eq!(op["security"], json!([{ "bearer": [] }]));
    assert_eq!(op["responses"]["401"]["description"], "Missing or invalid token");
    assert_eq!(op["parameters"][0]["name"], "Authorization");
    assert_eq!(op["parameters"][0]["in"], "header");
    assert_eq!(spec["components"]["securitySchemes"]["bearer"]["scheme"], "bearer");

    // The extractor's own rejection (401) is kept, matching what it documents.
    let req = Request::post("/pets").header("content-type", "application/json").body(Body::from("{}")).unwrap();
    assert_eq!(call(req).await.0, StatusCode::UNAUTHORIZED);
}

#[test]
fn toschema_structs_work_as_query_and_path_params() {
    let (_, spec) = app();
    let params = &spec["paths"]["/pets"]["get"]["parameters"];
    let q = params.as_array().unwrap().iter().find(|p| p["name"] == "q").unwrap();
    let limit = params.as_array().unwrap().iter().find(|p| p["name"] == "limit").unwrap();
    assert_eq!((q["in"].as_str(), q["required"].as_bool()), (Some("query"), Some(true)));
    assert_eq!(limit["required"], false);

    let names: Vec<&str> = spec["paths"]["/owners/{owner}/pets/{id}"]["get"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"owner") && names.contains(&"id"), "{names:?}");
}

#[test]
fn vec_items_and_generic_responses_reference_components() {
    let (_, spec) = app();
    let list = &spec["paths"]["/pets"]["get"]["responses"]["200"]["content"]["application/json"]["schema"];
    assert_eq!(list["items"]["$ref"], "#/components/schemas/Pet");
    let page = &spec["paths"]["/page"]["get"]["responses"]["200"]["content"]["application/json"]["schema"];
    assert_eq!(page["items"]["$ref"], "#/components/schemas/Pet");
}

#[tokio::test]
async fn non_axum_extractor_with_same_name_is_not_wrapped() {
    // Compiles with a non-Result return type: `extra::Query` keeps its own rejection.
    let (status, _, body) = call(Request::get("/extra").body(Body::empty()).unwrap()).await;
    assert_eq!((status, body), (StatusCode::OK, json!(1)));
}

#[derive(Deserialize, ToSchema, utoipa::IntoParams)]
#[allow(dead_code)]
struct Both {
    /// From IntoParams.
    #[param(example = 5)]
    n: u32,
}

#[route(GET, "/both")]
async fn both(Query(_b): Query<Both>) -> Result<Json<u32>, PetError> {
    Ok(Json(1))
}

#[test]
fn into_params_wins_over_toschema() {
    let (_, spec) = ApiRouter::<()>::new().routes(routes![both]).split();
    let spec = serde_json::to_value(spec).unwrap();
    assert_eq!(spec["paths"]["/both"]["get"]["parameters"][0]["example"], 5, "IntoParams metadata used");
}

// ---------- regressions: composition and multiple auth ----------

struct ApiKey;

impl<S: Send + Sync> FromRequestParts<S> for ApiKey {
    type Rejection = StatusCode;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, StatusCode> {
        parts.headers.contains_key("x-api-key").then_some(Self).ok_or(StatusCode::UNAUTHORIZED)
    }
}

impl ExtractorDoc for ApiKey {
    fn document(ctx: &mut DocCtx, op: &mut Operation) {
        use utoipa::openapi::security::{ApiKey as Key, ApiKeyValue};
        ctx.security(op, "api_key", &[], SecurityScheme::ApiKey(Key::Header(ApiKeyValue::new("x-api-key"))));
    }
}

#[route(GET, "/both-auth")]
async fn both_auth(_a: Bearer, _k: ApiKey) -> Json<u8> {
    Json(1)
}

#[test]
fn multiple_auth_extractors_are_all_required() {
    let (_, spec) = ApiRouter::<()>::new().routes(routes![both_auth]).split();
    let spec = serde_json::to_value(spec).unwrap();
    // One requirement object = AND. Two objects would mean either is enough.
    assert_eq!(spec["paths"]["/both-auth"]["get"]["security"], json!([{ "bearer": [], "api_key": [] }]));
}

#[route(POST, "/v1/pets")]
async fn post_v1_pets() -> Json<u8> {
    Json(1)
}

#[route(GET, "/pets")]
async fn get_pets() -> Json<u8> {
    Json(2)
}

#[tokio::test]
async fn nest_merges_methods_on_an_existing_path() {
    let inner = ApiRouter::new().routes(routes![get_pets]);
    let (router, spec) = ApiRouter::new().routes(routes![post_v1_pets]).nest("/v1", inner).split();
    let spec = serde_json::to_value(spec).unwrap();
    let item = &spec["paths"]["/v1/pets"];
    assert!(item["post"].is_object() && item["get"].is_object(), "both methods documented: {item}");
    let get = router.clone().oneshot(Request::get("/v1/pets").body(Body::empty()).unwrap()).await.unwrap();
    let post = router.oneshot(Request::post("/v1/pets").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!((get.status(), post.status()), (StatusCode::OK, StatusCode::OK));
}

#[tokio::test]
async fn spec_routes_survive_merge_and_nest() {
    let merged = ApiRouter::new().routes(routes![get_pets]).spec_route("/spec.json");
    let nested = ApiRouter::new().routes(routes![post_v1_pets]).spec_route("/spec.json");
    let (router, spec) = ApiRouter::new().merge(merged).nest("/api", nested).split();
    let spec = serde_json::to_value(spec).unwrap();
    for uri in ["/spec.json", "/api/spec.json"] {
        let res = router.clone().oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{uri}");
        let body: Value = serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body, spec, "{uri} serves the whole document");
    }
}

#[test]
#[should_panic(expected = "GET /v1/pets is registered twice")]
fn nest_detects_duplicates() {
    let inner = ApiRouter::<()>::new().routes(routes![get_pets]);
    let other = ApiRouter::<()>::new().routes(routes![get_pets]);
    let _ = ApiRouter::<()>::new().nest("/v1", inner).nest("/v1", other);
}

#[route(GET, "/")]
async fn root() -> Json<u8> {
    Json(3)
}

/// Docs, duplicate tracking and spec routes use axum's joining rules, checked against
/// what axum actually serves.
#[tokio::test]
async fn nested_paths_match_what_axum_serves() {
    for (prefix, route_path, expected) in [("/v1/", "/pets", "/v1/pets"), ("/v1", "/", "/v1")] {
        let inner = if route_path == "/" {
            ApiRouter::new().routes(routes![root])
        } else {
            ApiRouter::new().routes(routes![get_pets])
        };
        let (router, spec) = ApiRouter::new().nest(prefix, inner).split();
        let spec = serde_json::to_value(spec).unwrap();
        let documented: Vec<&String> = spec["paths"].as_object().unwrap().keys().collect();
        assert_eq!(documented, [expected], "prefix {prefix:?} + {route_path:?}");
        let res = router.oneshot(Request::get(expected).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{expected} is served");
    }

    // Spec routes follow the same rules: `/v1/` + `/spec.json` is served at `/v1/spec.json`.
    let inner = ApiRouter::new().routes(routes![get_pets]).spec_route("/spec.json");
    let (router, _) = ApiRouter::new().nest("/v1/", inner).split();
    let res = router.oneshot(Request::get("/v1/spec.json").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

struct Scoped<const WRITE: bool>;

impl<S: Send + Sync, const WRITE: bool> FromRequestParts<S> for Scoped<WRITE> {
    type Rejection = StatusCode;
    async fn from_request_parts(_: &mut Parts, _: &S) -> Result<Self, StatusCode> {
        Ok(Self)
    }
}

impl<const WRITE: bool> ExtractorDoc for Scoped<WRITE> {
    fn document(ctx: &mut DocCtx, op: &mut Operation) {
        let scheme = SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build());
        ctx.security(op, "oauth", &[if WRITE { "write" } else { "read" }], scheme);
    }
}

#[route(PUT, "/scoped")]
async fn scoped(_r: Scoped<false>, _w: Scoped<true>) -> Json<u8> {
    Json(1)
}

#[test]
fn repeated_scheme_unions_scopes() {
    let (_, spec) = ApiRouter::<()>::new().routes(routes![scoped]).split();
    let spec = serde_json::to_value(spec).unwrap();
    assert_eq!(spec["paths"]["/scoped"]["put"]["security"], json!([{ "oauth": ["read", "write"] }]));
}

// ---------- one name, two definitions ----------

struct KeyAuth;

impl<S: Send + Sync> FromRequestParts<S> for KeyAuth {
    type Rejection = StatusCode;
    async fn from_request_parts(_: &mut Parts, _: &S) -> Result<Self, StatusCode> {
        Ok(Self)
    }
}

impl ExtractorDoc for KeyAuth {
    fn document(ctx: &mut DocCtx, op: &mut Operation) {
        use utoipa::openapi::security::{ApiKey as Key, ApiKeyValue};
        // Same name as the `oauth` bearer scheme used by `Scoped`, different definition.
        ctx.security(op, "oauth", &[], SecurityScheme::ApiKey(Key::Header(ApiKeyValue::new("x-key"))));
    }
}

#[route(GET, "/key")]
async fn key_route(_k: KeyAuth) -> Json<u8> {
    Json(1)
}

#[route(GET, "/mixed")]
async fn mixed_schemes(_r: Scoped<false>, _k: KeyAuth) -> Json<u8> {
    Json(1)
}

fn panic_message(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
    let err = std::panic::catch_unwind(f).expect_err("should panic");
    err.downcast_ref::<String>().cloned().unwrap_or_default()
}

#[test]
fn conflicting_security_scheme_definitions_are_rejected() {
    let expected = "security scheme `oauth` has two different definitions";
    // Across routes,
    let msg = panic_message(|| drop(ApiRouter::<()>::new().routes(routes![scoped, key_route])));
    assert!(msg.contains(expected), "{msg}");
    // within one route,
    let msg = panic_message(|| drop(ApiRouter::<()>::new().routes(routes![mixed_schemes])));
    assert!(msg.contains(expected), "{msg}");
    // and when composing routers.
    let msg = panic_message(|| {
        let a = ApiRouter::<()>::new().routes(routes![scoped]);
        let b = ApiRouter::<()>::new().routes(routes![key_route]);
        drop(a.merge(b));
    });
    assert!(msg.contains(expected), "{msg}");
    let msg = panic_message(|| {
        let a = ApiRouter::<()>::new().routes(routes![scoped]);
        let b = ApiRouter::<()>::new().routes(routes![key_route]);
        drop(a.nest("/v2", b));
    });
    assert!(msg.contains(expected), "{msg}");
}

#[test]
fn same_scheme_definition_is_shared_freely() {
    // Two routes using the same `oauth` definition: fine.
    let (_, spec) =
        ApiRouter::<()>::new().routes(routes![scoped]).merge(ApiRouter::new().routes(routes![both_auth])).split();
    assert!(serde_json::to_value(spec).unwrap()["components"]["securitySchemes"]["oauth"].is_object());
}

mod v2 {
    use super::*;

    /// A different type that is also called `Pet`.
    #[derive(Serialize, ToSchema)]
    pub struct Pet {
        pub tag: String,
    }

    #[route(GET, "/v2/pet")]
    pub async fn get_pet_v2() -> Json<Pet> {
        Json(Pet { tag: "x".into() })
    }
}

#[test]
fn conflicting_schema_names_are_rejected() {
    let msg = panic_message(|| drop(ApiRouter::<()>::new().routes(routes![list_pets, v2::get_pet_v2])));
    assert!(msg.contains("schema `Pet` has two different definitions"), "{msg}");
}

// ---------- params that reference other schemas ----------

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)]
enum Sort {
    Asc,
    Desc,
}

/// `IntoParams` only: utoipa doesn't expose `Sort` from here.
#[derive(Deserialize, utoipa::IntoParams)]
#[allow(dead_code)]
struct SortedOnly {
    sort: Sort,
}

/// `IntoParams` + `ToSchema`: nested schemas are registered automatically.
#[derive(Deserialize, utoipa::IntoParams, ToSchema)]
#[allow(dead_code)]
struct SortedBoth {
    sort: Sort,
}

#[route(GET, "/sorted-only")]
async fn sorted_only(Query(_q): Query<SortedOnly>) -> Result<Json<u8>, PetError> {
    Ok(Json(1))
}

#[route(GET, "/sorted-both")]
async fn sorted_both(Query(_q): Query<SortedBoth>) -> Result<Json<u8>, PetError> {
    Ok(Json(1))
}

#[test]
fn unregistered_param_schema_is_an_actionable_error() {
    let msg = panic_message(|| drop(ApiRouter::<()>::new().routes(routes![sorted_only]).split()));
    assert!(msg.contains("#/components/schemas/Sort"), "{msg}");
    assert!(msg.contains("paths /sorted-only get parameters 0 schema"), "says where: {msg}");
    assert!(msg.contains("call `.schema::<Sort>()` on the router"), "says how to fix: {msg}");
}

#[test]
fn explicit_schema_registration_resolves_the_ref() {
    let (_, spec) = ApiRouter::<()>::new().routes(routes![sorted_only]).schema::<Sort>().split();
    let spec = serde_json::to_value(spec).unwrap();
    assert_eq!(spec["paths"]["/sorted-only"]["get"]["parameters"][0]["schema"]["$ref"], "#/components/schemas/Sort");
    assert!(spec["components"]["schemas"]["Sort"].is_object());
}

#[test]
fn params_struct_deriving_toschema_registers_nested_schemas() {
    let (_, spec) = ApiRouter::<()>::new().routes(routes![sorted_both]).split();
    assert!(serde_json::to_value(spec).unwrap()["components"]["schemas"]["Sort"].is_object());
}

#[test]
fn explicit_schema_registration_rejects_conflicts() {
    mod other {
        #[derive(utoipa::ToSchema)]
        #[allow(dead_code)]
        pub struct Sort {
            pub by: String,
        }
    }
    let msg = panic_message(|| drop(ApiRouter::<()>::new().schema::<Sort>().schema::<other::Sort>()));
    assert!(msg.contains("schema `Sort` has two different definitions"), "{msg}");
}

// ---------- `$ref` inside data is data (walker unit tests live in src/doc.rs) ----------

#[test]
fn example_with_literal_ref_via_map_openapi() {
    // The reported reproduction: an example added through map_openapi.
    let (_, spec) = ApiRouter::<()>::new()
        .routes(routes![sorted_both])
        .map_openapi(|mut o| {
            let schemas = &mut o.components.as_mut().unwrap().schemas;
            let mut v = serde_json::to_value(&schemas["Sort"]).unwrap();
            v["example"] = json!({ "data": { "$ref": "#/user-supplied-value" } });
            schemas.insert("Sort".into(), serde_json::from_value(v).unwrap());
            o
        })
        .split();
    assert!(serde_json::to_value(spec).unwrap()["components"]["schemas"]["Sort"]["example"].is_object());
}
