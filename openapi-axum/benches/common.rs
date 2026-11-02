//! Shared benchmark fixtures: the same echo endpoint built four ways.
#![allow(dead_code)]

use axum::Json;
use axum::body::Body;
use axum::http::Request;
use openapi_axum::{ApiResponse, ApiRouter, Rejection, route, routes};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Serialize, Deserialize, ToSchema, Clone)]
pub struct Small {
    #[schema(min_length = 1, max_length = 200)]
    pub title: String,
    pub done: bool,
}

#[derive(Serialize, Deserialize, ToSchema, Clone)]
pub struct Owner {
    pub id: u32,
    #[schema(min_length = 1)]
    pub name: String,
    pub tags: Vec<String>,
}

#[derive(Serialize, Deserialize, ToSchema, Clone)]
pub struct Item {
    pub id: u32,
    #[schema(min_length = 1, max_length = 200)]
    pub title: String,
    pub labels: Vec<String>,
}

#[derive(Serialize, Deserialize, ToSchema, Clone)]
pub struct Nested {
    pub owner: Owner,
    #[schema(max_items = 100)]
    pub items: Vec<Item>,
}

#[derive(Serialize, ToSchema)]
pub struct Problem {
    pub message: String,
}

#[derive(ApiResponse)]
pub enum ApiError {
    #[status(400)]
    BadRequest(Problem),
}

impl From<Rejection> for ApiError {
    fn from(r: Rejection) -> Self {
        Self::BadRequest(Problem { message: r.message })
    }
}

pub fn small_json() -> String {
    serde_json::to_string(&Small { title: "buy milk".into(), done: false }).unwrap()
}

pub fn nested_json() -> String {
    let item = |i: u32| Item { id: i, title: format!("item {i}"), labels: vec!["a".into(), "b".into()] };
    let nested = Nested {
        owner: Owner { id: 1, name: "jag".into(), tags: vec!["x".into(), "y".into()] },
        items: (0..20).map(item).collect(),
    };
    serde_json::to_string(&nested).unwrap()
}

pub fn post(path: &str, body: &str) -> Request<Body> {
    Request::post(path).header("content-type", "application/json").body(Body::from(body.to_owned())).unwrap()
}

async fn plain_small(Json(b): Json<Small>) -> Json<Small> {
    Json(b)
}

async fn plain_nested(Json(b): Json<Nested>) -> Json<Nested> {
    Json(b)
}

#[route(POST, "/small")]
async fn small(Json(b): Json<Small>) -> Result<Json<Small>, ApiError> {
    Ok(Json(b))
}

#[route(POST, "/nested")]
async fn nested(Json(b): Json<Nested>) -> Result<Json<Nested>, ApiError> {
    Ok(Json(b))
}

/// Plain axum: the baseline.
pub fn plain() -> axum::Router {
    axum::Router::new()
        .route("/small", axum::routing::post(plain_small))
        .route("/nested", axum::routing::post(plain_nested))
}

/// openapi-axum: request validation when the `validation` feature is on (default),
/// macro and routing overhead only when built with `--no-default-features`.
pub fn openapi() -> axum::Router {
    ApiRouter::new().routes(routes![small, nested]).split().0
}

/// openapi-axum with request validation and response verification.
#[cfg(feature = "validation")]
pub fn openapi_verified() -> axum::Router {
    ApiRouter::new().routes(routes![small, nested]).verify_responses(openapi_axum::VerifyMode::Log).split().0
}

/// The configurations available in this build, labelled.
pub fn configurations() -> Vec<(&'static str, axum::Router)> {
    let mut configs = vec![("plain_axum", plain())];
    if cfg!(feature = "validation") {
        configs.push(("request_validation", openapi()));
        #[cfg(feature = "validation")]
        configs.push(("request_and_response", openapi_verified()));
    } else {
        configs.push(("validation_off", openapi()));
    }
    configs
}
