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
#[derive(ApiResponse)]
enum Bad {
    #[status(201)]
    Created { #[header("Location")] location: Pet, #[body] pet: Pet },
}

fn main() {}
