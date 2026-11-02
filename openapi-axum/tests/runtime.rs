//! Behaviour of documented routes: docs derived from types, rejections typed,
//! contract problems caught at registration.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use openapi_axum::{ApiResponse, ApiRouter, Created, NoContent, Rejection, route, routes};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tower::ServiceExt;
use utoipa::{IntoParams, ToSchema};

#[derive(Serialize, Deserialize, ToSchema)]
struct Pet {
    id: u32,
    name: String,
}

#[derive(Deserialize, ToSchema)]
struct NewPet {
    name: String,
}

#[derive(Deserialize, IntoParams)]
struct Search {
    /// Name prefix.
    q: Option<String>,
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
    /// No such pet.
    #[status(NOT_FOUND)]
    NotFound,
}

impl From<Rejection> for PetError {
    fn from(r: Rejection) -> Self {
        Self::BadRequest(Problem { message: r.message })
    }
}

#[route(GET, "/pets", tag = "Pets")]
/// List pets.
///
/// Optionally filtered by name prefix.
async fn list_pets(Query(s): Query<Search>) -> Result<Json<Vec<Pet>>, PetError> {
    let pets = vec![Pet { id: 1, name: "Rex".into() }];
    Ok(Json(pets.into_iter().filter(|p| s.q.as_ref().is_none_or(|q| p.name.starts_with(q.as_str()))).collect()))
}

#[route(GET, "/pets/{id}")]
async fn get_pet(Path(id): Path<u32>) -> Result<Json<Pet>, PetError> {
    if id == 1 { Ok(Json(Pet { id, name: "Rex".into() })) } else { Err(PetError::NotFound) }
}

#[route(POST, "/pets")]
async fn create_pet(State(next): State<u32>, Json(p): Json<NewPet>) -> Result<Created<Pet>, PetError> {
    Ok(Created(Pet { id: next, name: p.name }))
}

#[route(DELETE, "/owners/{owner}/pets/{id}")]
#[deprecated]
async fn delete_pet(Path((_owner, _id)): Path<(String, u32)>) -> Result<NoContent, PetError> {
    Ok(NoContent)
}

#[route(POST, "/raw")]
async fn raw_body(#[raw] Json(p): Json<NewPet>) -> Json<String> {
    Json(p.name)
}

fn app() -> (axum::Router, Value) {
    let (router, spec) = ApiRouter::new()
        .routes(routes![list_pets, get_pet, create_pet, delete_pet, raw_body])
        .spec_route("/openapi.json")
        .with_state(7u32)
        .split();
    (router, serde_json::to_value(spec).unwrap())
}

async fn call(router: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = router.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn post_json(uri: &str, body: &str) -> Request<Body> {
    Request::post(uri).header("content-type", "application/json").body(Body::from(body.to_owned())).unwrap()
}

#[test]
fn operations_are_derived_from_signatures() {
    let (_, spec) = app();
    let list = &spec["paths"]["/pets"]["get"];
    assert_eq!(list["operationId"], "list_pets");
    assert_eq!(list["summary"], "List pets.");
    assert_eq!(list["description"], "Optionally filtered by name prefix.");
    assert_eq!(list["tags"], json!(["Pets"]));
    assert_eq!(list["parameters"][0]["name"], "q");
    assert_eq!(list["parameters"][0]["in"], "query"); // no #[into_params(parameter_in)] needed
    assert_eq!(
        list["responses"]["200"]["content"]["application/json"]["schema"]["items"]["$ref"],
        "#/components/schemas/Pet"
    );
    assert_eq!(list["responses"]["400"]["description"], "Bad input.");
    assert_eq!(list["responses"]["404"]["description"], "No such pet.");
    assert!(list["responses"]["404"].get("content").is_none());

    let create = &spec["paths"]["/pets"]["post"];
    assert_eq!(create["requestBody"]["content"]["application/json"]["schema"]["$ref"], "#/components/schemas/NewPet");
    assert!(create["responses"]["201"].is_object());
    assert!(create["responses"].get("200").is_none());

    let get = &spec["paths"]["/pets/{id}"]["get"];
    assert_eq!(get["parameters"][0]["name"], "id");
    assert_eq!(get["parameters"][0]["in"], "path");
    assert_eq!(get["parameters"][0]["schema"]["type"], "integer");

    let delete = &spec["paths"]["/owners/{owner}/pets/{id}"]["delete"];
    assert_eq!(delete["deprecated"], true);
    let names: Vec<&str> =
        delete["parameters"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["owner", "id"]);
    assert!(delete["responses"]["204"].get("content").is_none());

    for name in ["Pet", "NewPet", "Problem"] {
        assert!(spec["components"]["schemas"][name].is_object(), "{name} registered");
    }
    assert!(spec["paths"].get("/openapi.json").is_none(), "spec route is undocumented");
}

#[tokio::test]
async fn runtime_status_matches_documented_status() {
    let (router, spec) = app();
    let (status, body) = call(router.clone(), post_json("/pets", r#"{"name":"Fido"}"#)).await;
    assert_eq!((status, body), (StatusCode::CREATED, json!({ "id": 7, "name": "Fido" })));
    let (status, _) = call(router.clone(), Request::get("/pets/2").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(router.clone(), Request::delete("/owners/a/pets/1").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, served) = call(router, Request::get("/openapi.json").body(Body::empty()).unwrap()).await;
    assert_eq!((status, served), (StatusCode::OK, spec));
}

#[tokio::test]
async fn rejections_use_the_documented_error_type() {
    let (router, _) = app();
    // Missing field (axum: 422 text), bad JSON (400 text), bad path (400 text), wrong content type (415 text).
    for req in [
        post_json("/pets", r#"{}"#),
        post_json("/pets", "{not json"),
        Request::get("/pets/abc").body(Body::empty()).unwrap(),
        Request::post("/pets").body(Body::from(r#"{"name":"x"}"#)).unwrap(),
    ] {
        let (status, body) = call(router.clone(), req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["message"].is_string(), "documented Problem body: {body}");
    }
}

#[tokio::test]
async fn raw_keeps_axums_rejection() {
    let (router, _) = app();
    let res = router.oneshot(post_json("/raw", "{}")).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn handlers_stay_plain_functions() {
    // Unit-test the handler directly; no router or HTTP needed.
    let Json(pet) = get_pet(Path(1)).await.ok().unwrap();
    assert_eq!(pet.name, "Rex");
}

// ---------- contract problems fail at registration ----------

#[derive(Deserialize, IntoParams)]
#[allow(dead_code)]
struct PetPath {
    id: u32,
    name: String,
}

#[route(GET, "/mismatch/{id}")]
async fn struct_mismatch(Path(_p): Path<PetPath>) -> Result<Json<u32>, PetError> {
    Ok(Json(1))
}

#[route(GET, "/two/{a}/{b}")]
async fn single_for_two(Path(_a): Path<u32>) -> Result<Json<u32>, PetError> {
    Ok(Json(1))
}

#[derive(ApiResponse)]
enum AlsoOk {
    #[status(200)]
    Fine(Problem),
}

impl From<Rejection> for AlsoOk {
    fn from(r: Rejection) -> Self {
        Self::Fine(Problem { message: r.message })
    }
}

#[route(GET, "/dup")]
async fn duplicate_status() -> Result<Json<u32>, AlsoOk> {
    Ok(Json(1))
}

fn register_err(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
    let err = std::panic::catch_unwind(f).expect_err("registration should panic");
    err.downcast_ref::<String>().cloned().unwrap_or_default()
}

#[test]
fn path_struct_mismatch_is_caught() {
    let msg = register_err(|| drop(ApiRouter::<()>::new().routes(routes![struct_mismatch])));
    assert!(msg.contains(r#"path /mismatch/{id} has params {"id"} but the handler extracts {"id", "name"}"#), "{msg}");
}

#[test]
fn single_value_for_many_params_is_caught() {
    let msg = register_err(|| drop(ApiRouter::<()>::new().routes(routes![single_for_two])));
    assert!(msg.contains("extracts one value but the path template has 2 parameters"), "{msg}");
}

#[test]
fn duplicate_status_is_caught() {
    let msg = register_err(|| drop(ApiRouter::<()>::new().routes(routes![duplicate_status])));
    assert!(msg.contains("status 200 is documented twice"), "{msg}");
}

#[test]
fn duplicate_route_is_caught() {
    let msg = register_err(|| drop(ApiRouter::<()>::new().routes(routes![get_pet, get_pet])));
    assert!(msg.contains("GET /pets/{id} is registered twice"), "{msg}");
}

#[tokio::test]
async fn nest_prefixes_routes_and_docs() {
    let inner = ApiRouter::new().routes(routes![get_pet]);
    let (router, spec) = ApiRouter::new().nest("/v1", inner).split();
    let spec = serde_json::to_value(spec).unwrap();
    assert!(spec["paths"]["/v1/pets/{id}"]["get"].is_object());
    let (status, _) = call(router, Request::get("/v1/pets/1").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
}
