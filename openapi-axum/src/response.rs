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

/// A response header value checked when you construct it. In a `#[header]` field it
/// can't fail at send time, unlike a `String` or other `Display` value: an invalid
/// header (a newline, say) is an error *you* handle, not an undocumented `500`.
///
/// ```
/// use openapi_axum::HeaderText;
/// let location = HeaderText::new(format!("/todos/{}", 7)).unwrap();
/// assert_eq!(location.as_str(), "/todos/7");
/// assert!(HeaderText::new("bad\nvalue").is_err());
/// const CACHE: HeaderText = HeaderText::from_static("no-store");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HeaderText(axum::http::HeaderValue);

/// Returned by [`HeaderText::new`] for text that isn't a valid HTTP header value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidHeaderText(String);

impl std::fmt::Display for InvalidHeaderText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not a valid HTTP header value: {:?}", self.0)
    }
}

impl std::error::Error for InvalidHeaderText {}

impl HeaderText {
    /// Validate `text` as a header value (visible ASCII, spaces and tabs).
    ///
    /// # Errors
    /// If `text` contains control characters such as `\n`, or non-ASCII bytes.
    pub fn new(text: impl Into<String>) -> Result<Self, InvalidHeaderText> {
        let text = text.into();
        match axum::http::HeaderValue::from_str(&text) {
            Ok(v) if v.to_str().is_ok() => Ok(Self(v)),
            _ => Err(InvalidHeaderText(text)),
        }
    }

    /// A header value from a literal, checked at compile time in `const` context.
    ///
    /// # Panics
    /// If `text` isn't a valid header value (a compile error in `const` context).
    #[must_use]
    pub const fn from_static(text: &'static str) -> Self {
        Self(axum::http::HeaderValue::from_static(text))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.to_str().expect("validated on construction")
    }
}

impl std::fmt::Display for HeaderText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for HeaderText {
    type Err = InvalidHeaderText;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl From<HeaderText> for axum::http::HeaderValue {
    fn from(h: HeaderText) -> Self {
        h.0
    }
}

impl utoipa::PartialSchema for HeaderText {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::schema::ObjectBuilder::new().schema_type(utoipa::openapi::schema::Type::String).into()
    }
}

impl ToSchema for HeaderText {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("HeaderText")
    }
}
