//! # openapi-axum
//!
//! Write ordinary axum handlers. The OpenAPI operation is derived from the
//! handler's signature, so you declare each endpoint once. Typed responses are
//! checked at compile time; custom extractors, middleware and `#[raw]` arguments are
//! covered by runtime validation, `verify_responses` and the `testing` harness.
//!
//! - **Responses** come from the return type (`Json<T>` → 200, `Created<T>` → 201,
//!   `NoContent` → 204, `Result<T, E>` → T's + E's). Error enums use
//!   `#[derive(ApiResponse)]`: one `#[status(..)]` drives both the runtime status
//!   *and* the docs.
//! - **Params and bodies** come from `Path`, `Query`, `Json` and `Form` arguments.
//! - **Extractor rejections** (bad JSON, bad query, bad path) are routed into your
//!   error type via `From<Rejection>`, so they're documented too, and the compiler
//!   insists you handle them.
//! - **Path templates are checked**: tuple arity at compile time, everything else
//!   when the route is registered (startup or your first test).
//!
//! ```
//! use axum::{Json, extract::{Path, State}};
//! use openapi_axum::{ApiResponse, ApiRouter, Rejection, route, routes};
//! use serde::Serialize;
//! use utoipa::ToSchema;
//!
//! #[derive(Serialize, ToSchema)]
//! struct Todo { id: u32, title: String }
//!
//! #[derive(Serialize, ToSchema)]
//! struct ErrorBody { message: String }
//!
//! #[derive(ApiResponse)]
//! enum ApiError {
//!     /// The request was invalid.
//!     #[status(BAD_REQUEST)]
//!     BadRequest(ErrorBody),
//!     /// No todo with that id.
//!     #[status(NOT_FOUND)]
//!     NotFound(ErrorBody),
//! }
//!
//! // Malformed path/query/body arrive here, so they're documented too.
//! impl From<Rejection> for ApiError {
//!     fn from(r: Rejection) -> Self {
//!         Self::BadRequest(ErrorBody { message: r.message })
//!     }
//! }
//!
//! #[route(GET, "/todos/{id}", tag = "Todos")]
//! /// Get a todo.
//! async fn get_todo(State(prefix): State<String>, Path(id): Path<u32>) -> Result<Json<Todo>, ApiError> {
//!     match id {
//!         1 => Ok(Json(Todo { id, title: format!("{prefix} one") })),
//!         _ => Err(ApiError::NotFound(ErrorBody { message: "no such todo".into() })),
//!     }
//! }
//!
//! let (router, openapi) = ApiRouter::new()
//!     .routes(routes![get_todo])
//!     .spec_route("/openapi.json")
//!     .with_state("todo".to_string())
//!     .split();
//!
//! let op = &openapi.paths.paths["/todos/{id}"].get.as_ref().unwrap();
//! assert_eq!(op.operation_id.as_deref(), Some("get_todo"));
//! assert!(op.responses.responses.contains_key("404"));
//! # let _: axum::Router = router;
//! ```

// Compile and run the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../../README.md")]
pub struct ReadmeDoctests;

mod doc;
mod rejection;
mod response;
mod router;
#[cfg(feature = "validation")]
mod spec_index;
#[cfg(feature = "testing")]
pub mod testing;
mod validation;

pub use doc::{DocCtx, ExtractorDoc, PathValue};
pub use openapi_axum_macros::{ApiResponse, route};
pub use rejection::{Rejection, RejectionKind};
pub use response::{Accepted, ApiResponse, Created, HeaderText, InvalidHeaderText, NoContent, ResponseHeader};
pub use router::{ApiRouter, DEFAULT_VERIFY_BODY_LIMIT, RouteDef, VerifyMode};

/// Collect `#[route]` handlers for [`ApiRouter::routes`]: `routes![get_todo, todos::create_todo]`.
#[macro_export]
macro_rules! routes {
    ($($route:path),* $(,)?) => {
        [$( <$route as $crate::__private::Route>::def(<$route>::handler) ),*]
    };
}

#[doc(hidden)]
pub mod __private {
    pub use crate::doc::{
        NoExtractorDoc, NoNested, NoRegister, ViaExtractorDoc, ViaNested, ViaParams, ViaRegister, ViaSchema, ViaValue,
    };
    pub use crate::validation::{ParamsGuard, ValidForm, ValidJson, ValidQuery};

    /// Insert a typed `#[header]` field. `ResponseHeader` values are valid by
    /// construction, so this can't fail.
    pub fn insert_header(res: &mut axum::response::Response, name: &'static str, value: &impl crate::ResponseHeader) {
        res.headers_mut().insert(axum::http::HeaderName::from_static(name), value.to_header_value());
    }
    pub use crate::rejection::{FromRejection, reject};
    pub use crate::router::Route;
    pub use axum;
    pub use axum::http::Method;
    pub use serde;
    pub use utoipa;

    /// Runs the handler future, inside a span named after the operation when the `tracing` feature is on.
    pub async fn instrument<F: Future>(operation_id: &'static str, route: &'static str, fut: F) -> F::Output {
        #[cfg(feature = "tracing")]
        {
            use tracing::Instrument;
            fut.instrument(tracing::info_span!("handler", otel.name = operation_id, operation_id, http.route = route))
                .await
        }
        #[cfg(not(feature = "tracing"))]
        {
            let _ = (operation_id, route);
            fut.await
        }
    }
}
