use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;
use utoipa::openapi::Responses;

use crate::DocCtx;

/// A response type whose OpenAPI responses are derived from the type itself.
///
/// Implemented for `Json<T>` (200), [`Created`] (201), [`Accepted`] (202),
/// [`NoContent`] (204), `String`/`&'static str` (200 text), `Result<T, E>`, and any
/// enum or struct with `#[derive(ApiResponse)]`.
#[diagnostic::on_unimplemented(
    message = "`{Self}` doesn't describe its responses",
    note = "return `Json<T>`, `Created<T>`, `NoContent`, a `Result` of those, or a type with `#[derive(ApiResponse)]`"
)]
pub trait ApiResponse: IntoResponse {
    fn document(ctx: &mut DocCtx, responses: &mut Responses);
}

/// 201 Created with a JSON body.
#[derive(Debug, Clone)]
pub struct Created<T>(pub T);

/// 202 Accepted with a JSON body.
#[derive(Debug, Clone)]
pub struct Accepted<T>(pub T);

/// 204 No Content.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoContent;

impl<T: Serialize> IntoResponse for Created<T> {
    fn into_response(self) -> Response {
        (StatusCode::CREATED, axum::Json(self.0)).into_response()
    }
}

impl<T: Serialize> IntoResponse for Accepted<T> {
    fn into_response(self) -> Response {
        (StatusCode::ACCEPTED, axum::Json(self.0)).into_response()
    }
}

impl IntoResponse for NoContent {
    fn into_response(self) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }
}

impl<T: Serialize + ToSchema> ApiResponse for axum::Json<T> {
    fn document(ctx: &mut DocCtx, responses: &mut Responses) {
        ctx.json_response::<T>(responses, 200, None);
    }
}

impl<T: Serialize + ToSchema> ApiResponse for Created<T> {
    fn document(ctx: &mut DocCtx, responses: &mut Responses) {
        ctx.json_response::<T>(responses, 201, None);
    }
}

impl<T: Serialize + ToSchema> ApiResponse for Accepted<T> {
    fn document(ctx: &mut DocCtx, responses: &mut Responses) {
        ctx.json_response::<T>(responses, 202, None);
    }
}

impl ApiResponse for NoContent {
    fn document(ctx: &mut DocCtx, responses: &mut Responses) {
        ctx.empty_response(responses, 204, None);
    }
}

impl ApiResponse for String {
    fn document(ctx: &mut DocCtx, responses: &mut Responses) {
        ctx.text_response(responses, 200, None);
    }
}

impl ApiResponse for &'static str {
    fn document(ctx: &mut DocCtx, responses: &mut Responses) {
        ctx.text_response(responses, 200, None);
    }
}

impl<T: ApiResponse, E: ApiResponse> ApiResponse for Result<T, E> {
    fn document(ctx: &mut DocCtx, responses: &mut Responses) {
        T::document(ctx, responses);
        E::document(ctx, responses);
    }
}
