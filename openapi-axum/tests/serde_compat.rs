//! Serde and the published schema must agree on what a valid request is. Validation
//! checks the schema, then serde deserializes, so a disagreement shows up two ways:
//! - serde would accept it but the schema rejects it (a false 400),
//! - the schema accepts it but serde rejects it (must still be a documented 400).

use axum::Json;
use axum::body::Body;
use axum::extract::Query;
use axum::http::{Request, StatusCode};
use openapi_axum::testing::{Conformance, assert_conforms};
use openapi_axum::{ApiResponse, ApiRouter, Rejection, route, routes};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use tower::ServiceExt;
use utoipa::ToSchema;

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

macro_rules! echo_route {
    ($name:ident, $path:literal, $ty:ty) => {
        #[route(POST, $path)]
        async fn $name(Json(_v): Json<$ty>) -> Result<Json<u8>, ApiError> {
            Ok(Json(1))
        }
    };
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct Renamed {
    user_id: u32,
}
echo_route!(renamed, "/renamed", Renamed);

#[derive(Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
#[allow(dead_code)]
enum Shape {
    Circle { r: f64 },
    Square { side: f64 },
}
echo_route!(tagged, "/tagged", Shape);

#[derive(Deserialize, ToSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum IdOrName {
    Id(u32),
    Name(String),
}
echo_route!(untagged, "/untagged", IdOrName);

#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
struct Defaults {
    #[serde(default)]
    page: u32,
    note: Option<String>,
}
echo_route!(defaults, "/defaults", Defaults);

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct Strict {
    a: u32,
}
echo_route!(strict, "/strict", Strict);

/// Serde accepts `5` or `"5"`; the default schema (from `u32`) says integer only.
fn num_or_str<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        N(u32),
        S(String),
    }
    match Raw::deserialize(d)? {
        Raw::N(n) => Ok(n),
        Raw::S(s) => s.parse().map_err(serde::de::Error::custom),
    }
}

#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
struct Lenient {
    #[serde(deserialize_with = "num_or_str")]
    n: u32,
}
echo_route!(lenient, "/lenient", Lenient);

/// The same deserializer, with the schema widened to say so.
#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
struct LenientDocumented {
    #[serde(deserialize_with = "num_or_str")]
    #[schema(schema_with = num_or_numeric_string)]
    n: u32,
}
echo_route!(lenient_documented, "/lenient-documented", LenientDocumented);

fn num_or_numeric_string() -> utoipa::openapi::schema::Schema {
    serde_json::from_value(
        json!({ "oneOf": [{ "type": "integer", "minimum": 0 }, { "type": "string", "pattern": "^[0-9]+$" }] }),
    )
    .unwrap()
}

/// Serde wants a positive even number; the schema (from `u32`) only says `integer >= 0`.
fn even<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let n = u32::deserialize(d)?;
    if n > 0 && n % 2 == 0 { Ok(n) } else { Err(serde::de::Error::custom("must be a positive even number")) }
}

#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
struct Even {
    #[serde(deserialize_with = "even")]
    #[schema(example = 2)]
    n: u32,
}
echo_route!(even_route, "/even", Even);

#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
struct Odd {
    #[serde(deserialize_with = "even")]
    n: u32,
}
echo_route!(odd_route, "/odd", Odd);

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct QueryRenamed {
    page_size: Option<u32>,
}

#[route(GET, "/query-renamed")]
async fn query_renamed(Query(q): Query<QueryRenamed>) -> Result<Json<Option<u32>>, ApiError> {
    Ok(Json(q.page_size))
}

fn app() -> (axum::Router, utoipa::openapi::OpenApi) {
    ApiRouter::new()
        .routes(routes![
            renamed,
            tagged,
            untagged,
            defaults,
            strict,
            lenient,
            lenient_documented,
            even_route,
            query_renamed
        ])
        .split()
}

async fn post(path: &str, body: Value) -> StatusCode {
    let req =
        Request::post(path).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap();
    app().0.oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn schema_and_serde_agree() {
    use StatusCode as S;
    for (path, body, want) in [
        ("/renamed", json!({ "userId": 1 }), S::OK),
        ("/renamed", json!({ "user_id": 1 }), S::BAD_REQUEST),
        ("/tagged", json!({ "type": "circle", "r": 1.5 }), S::OK),
        ("/tagged", json!({ "type": "square", "side": 2 }), S::OK),
        ("/tagged", json!({ "type": "triangle", "side": 2 }), S::BAD_REQUEST),
        ("/tagged", json!({ "type": "circle" }), S::BAD_REQUEST),
        ("/untagged", json!(5), S::OK),
        ("/untagged", json!("five"), S::OK),
        ("/untagged", json!(true), S::BAD_REQUEST),
        ("/defaults", json!({}), S::OK),
        ("/defaults", json!({ "page": 2, "note": null }), S::OK),
        ("/defaults", json!({ "page": "two" }), S::BAD_REQUEST),
        ("/strict", json!({ "a": 1 }), S::OK),
        ("/strict", json!({ "a": 1, "b": 2 }), S::BAD_REQUEST),
    ] {
        assert_eq!(post(path, body.clone()).await, want, "{path} {body}");
    }
    let (router, _) = app();
    let get = |uri: &'static str| router.clone().oneshot(Request::get(uri).body(Body::empty()).unwrap());
    assert_eq!(get("/query-renamed?pageSize=5").await.unwrap().status(), S::OK);
    assert_eq!(get("/query-renamed?pageSize=x").await.unwrap().status(), S::BAD_REQUEST);
}

#[tokio::test]
async fn serde_accepting_more_than_the_schema_is_a_false_reject_until_documented() {
    // The schema governs: `"5"` is valid for the deserializer but not for the schema
    // derived from `u32`, so it's rejected. Widening the schema fixes it.
    assert_eq!(post("/lenient", json!({ "n": 5 })).await, StatusCode::OK);
    assert_eq!(post("/lenient", json!({ "n": "5" })).await, StatusCode::BAD_REQUEST);
    assert_eq!(post("/lenient-documented", json!({ "n": "5" })).await, StatusCode::OK);
    assert_eq!(post("/lenient-documented", json!({ "n": "five" })).await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn serde_rejecting_what_the_schema_accepts_is_still_a_documented_400() {
    // The schema says integer; serde also wants it even. 3 passes validation, then
    // serde rejects it: the client gets the documented 400, never a 500.
    let req = Request::post("/even").header("content-type", "application/json").body(Body::from(r#"{"n":3}"#)).unwrap();
    let res = app().0.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let bytes = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["message"].as_str().unwrap().contains("must be a positive even number"), "{body}");
}

#[tokio::test]
async fn conformance_flags_schemas_serde_cannot_satisfy() {
    // With an `example` that serde accepts, the docs-built request succeeds...
    let (router, openapi) = ApiRouter::new().routes(routes![even_route]).split();
    Conformance::new(&router, &openapi).cases(0).run().await;
    // ...but generated cases try other schema-valid values, and serde rejects `n = 0`.
    let err = tokio::spawn(async move { assert_conforms(&router, &openapi).await }).await.expect_err("should fail");
    let msg = err.into_panic().downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(msg.contains(r#"POST /even (generated valid request: body {"n":0})"#), "{msg}");
    // Without an example, the generated `n = 0` (u32's minimum) is schema-valid but serde
    // rejects it, and
    // the harness reports that the docs don't describe a valid request.
    let (router, openapi) = ApiRouter::new().routes(routes![odd_route]).split();
    let err = tokio::spawn(async move { assert_conforms(&router, &openapi).await }).await.expect_err("should fail");
    let msg = err.into_panic().downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(msg.contains("POST /odd: a request built from the docs was rejected"), "{msg}");
}

#[tokio::test]
async fn the_whole_app_conforms() {
    let (router, openapi) = app();
    // `/even` is the schema/serde mismatch from the test above.
    let report = Conformance::new(&router, &openapi).expect("POST /even", StatusCode::OK).run().await;
    assert_eq!(report.len(), 9);
}
