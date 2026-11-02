#![allow(unused_imports, dead_code)]
use axum::Json;
use axum::extract::Path;
use openapi_axum::{ApiResponse, Rejection, route};
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
struct Pet { id: u32 }

#[derive(Serialize, ToSchema)]
struct Problem { message: String }

#[derive(ApiResponse)]
enum ApiError {
    #[status(404)]
    NotFound(Problem),
}

impl From<Rejection> for ApiError {
    fn from(r: Rejection) -> Self { Self::NotFound(Problem { message: r.message }) }
}

#[route(GET, "/pets/{id}")]
async fn get(Path(id): Path<u32>) -> Result<Json<Pet>, ApiError> { Ok(Json(Pet { id })) }

#[route(GET, "/health")]
async fn health() -> &'static str { "ok" }

#[route(DELETE, "/pets/{owner}/{id}")]
async fn delete(Path((_o, _i)): Path<(String, u32)>) -> Result<openapi_axum::NoContent, ApiError> { Ok(openapi_axum::NoContent) }

#[allow(dead_code)]
fn router() -> openapi_axum::ApiRouter { openapi_axum::ApiRouter::new().routes(openapi_axum::routes![get, health, delete]) }

fn main() {}
