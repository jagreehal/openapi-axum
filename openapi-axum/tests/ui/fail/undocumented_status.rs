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
async fn get(Path(_id): Path<u32>) -> Result<Json<Pet>, ApiError> {
    // ApiError has no 409 variant, so there is no way to send an undocumented 409.
    Err(ApiError::Conflict)
}

fn main() {}
