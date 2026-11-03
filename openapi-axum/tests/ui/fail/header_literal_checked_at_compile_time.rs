use openapi_axum::HeaderText;

/// A literal header value with a newline is a compile error, not a runtime panic.
const BAD: HeaderText = HeaderText::from_static("bad\nvalue");

fn main() {
    let _ = BAD;
}
