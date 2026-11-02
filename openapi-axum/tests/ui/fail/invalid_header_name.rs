#![allow(unused_imports, dead_code)]
use openapi_axum::{ApiResponse, HeaderText};

#[derive(ApiResponse)]
enum Bad {
    /// Header names are checked at compile time.
    #[status(201)]
    Created {
        #[header("Bad Name")]
        location: HeaderText,
    },
}

fn main() {}
