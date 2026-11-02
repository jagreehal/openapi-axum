use axum::extract::rejection::{FormRejection, JsonRejection, PathRejection, QueryRejection, RawPathParamsRejection};
use axum::http::StatusCode;
use axum::response::IntoResponse;

/// Which extractor rejected the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionKind {
    Json,
    Path,
    Query,
    Form,
}

/// An extractor rejection (malformed JSON, bad query string, unparsable path...).
///
/// `#[route]` handlers that return `Result<_, E>` receive these as `E::from(rejection)`,
/// so framework errors use *your* documented error shape instead of axum's plain text.
#[derive(Debug, Clone)]
pub struct Rejection {
    pub kind: RejectionKind,
    /// The status axum would have used (400, 415, 422...).
    pub status: StatusCode,
    pub message: String,
}

macro_rules! from_axum {
    ($($t:ty => $kind:ident),*) => {$(
        impl From<$t> for Rejection {
            fn from(r: $t) -> Self {
                Self { kind: RejectionKind::$kind, status: r.status(), message: r.body_text() }
            }
        }
    )*};
}
from_axum!(
    JsonRejection => Json,
    PathRejection => Path,
    QueryRejection => Query,
    FormRejection => Form,
    RawPathParamsRejection => Path
);

/// Response types that can carry an extractor rejection.
#[diagnostic::on_unimplemented(
    message = "`{Self}` can't carry extractor rejections",
    label = "this handler uses Json/Path/Query/Form, so its return type must be able to report a bad request",
    note = "return `Result<_, E>` where `E: From<openapi_axum::Rejection>`, or mark the argument `#[raw]` to keep axum's default rejection"
)]
pub trait FromRejection {
    fn from_rejection(r: Rejection) -> Self;
}

impl<T, E: From<Rejection>> FromRejection for Result<T, E> {
    fn from_rejection(r: Rejection) -> Self {
        Err(E::from(r))
    }
}

#[doc(hidden)]
pub fn reject<R: FromRejection + IntoResponse>(r: Rejection) -> R {
    R::from_rejection(r)
}

/// Axum's own behaviour (status + plain-text message), used where no error type is involved.
impl IntoResponse for Rejection {
    fn into_response(self) -> axum::response::Response {
        (self.status, self.message).into_response()
    }
}
