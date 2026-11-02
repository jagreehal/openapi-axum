//! Proc macros for `openapi-axum`. Use the re-exports from that crate.

mod response;
mod route;
mod util;

use proc_macro::TokenStream;

/// Turn an async axum handler into a documented route.
///
/// `#[route(GET, "/todos/{id}", tag = "Todos")]`. Optional: `tags = ["a", "b"]`,
/// `operation_id = "..."` (defaults to the fn name). Doc comments become the
/// summary (first line) and description (the rest); `#[deprecated]` is honoured.
///
/// Mark an argument `#[raw]` to keep axum's own rejection for it.
#[proc_macro_attribute]
pub fn route(attr: TokenStream, item: TokenStream) -> TokenStream {
    route::expand(attr.into(), item.into()).unwrap_or_else(syn::Error::into_compile_error).into()
}

/// Derive `IntoResponse` **and** the OpenAPI responses from one `#[status(..)]` per
/// variant, so the status the client gets and the status in the docs can't differ.
///
/// Variants are unit (no body), hold one `Serialize + ToSchema` body, or use named
/// fields: one `#[body]` plus `#[header("Location")]` fields that are both sent and
/// documented (`Option<T>` headers are sent only when `Some`). A header value that
/// isn't valid HTTP (e.g. contains a newline) turns the response into a `500`. Doc comments become
/// response descriptions. Generic enums get the bounds they need automatically.
#[proc_macro_derive(ApiResponse, attributes(status, body, header))]
pub fn derive_api_response(item: TokenStream) -> TokenStream {
    response::expand(item.into()).unwrap_or_else(syn::Error::into_compile_error).into()
}
