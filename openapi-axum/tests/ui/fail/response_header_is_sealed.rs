#![allow(unused_imports, dead_code)]
use openapi_axum::ResponseHeader;

/// A user type can't implement ResponseHeader: an impl could panic at send time.
struct ETag(String);

impl ResponseHeader for ETag {
    fn to_header_value(&self) -> axum::http::HeaderValue {
        axum::http::HeaderValue::from_str(&self.0).unwrap()
    }
}

fn main() {}
