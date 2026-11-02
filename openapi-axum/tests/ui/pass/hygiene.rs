#![allow(unused_imports, dead_code)]
//! The macros' output must not depend on the caller's prelude or imports (cf. poem-openapi's hygiene.rs).

pub mod models {
    #[derive(serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
    pub struct Pet {
        pub id: u32,
    }

    #[derive(serde::Serialize, utoipa::ToSchema)]
    pub struct Problem {
        pub message: std::string::String,
    }
}

#[no_implicit_prelude]
mod hygiene {
    use ::axum::Json;
    use ::axum::extract::{Path, Query};
    use super::models::{Pet, Problem};

    #[derive(::openapi_axum::ApiResponse)]
    pub enum ApiError {
        /// Bad.
        #[status(400)]
        BadRequest(Problem),
        #[status(NOT_FOUND)]
        NotFound,
    }

    impl ::core::convert::From<::openapi_axum::Rejection> for ApiError {
        fn from(r: ::openapi_axum::Rejection) -> Self {
            Self::BadRequest(Problem { message: r.message })
        }
    }

    #[derive(::openapi_axum::ApiResponse)]
    pub enum Created<T> {
        #[status(201)]
        Created {
            #[header("Location")]
            location: ::std::string::String,
            #[header("X-Trace")]
            trace: ::core::option::Option<::std::string::String>,
            #[body]
            body: ::std::vec::Vec<T>,
        },
    }

    #[::openapi_axum::route(GET, "/pets/{id}", tags = ["a", "b"], operation_id = "get")]
    /// Summary.
    ///
    /// Description.
    #[deprecated]
    pub async fn get_pet(Path(id): Path<u32>) -> ::core::result::Result<Json<Pet>, ApiError> {
        ::core::result::Result::Ok(Json(Pet { id }))
    }

    #[::openapi_axum::route(POST, "/pets")]
    pub async fn create(Json(p): Json<Pet>) -> ::core::result::Result<Created<Pet>, ApiError> {
        let location = ::std::string::String::new();
        ::core::result::Result::Ok(Created::Created { location, trace: ::core::option::Option::None, body: ::std::vec![p] })
    }

    pub fn router() -> ::openapi_axum::ApiRouter {
        ::openapi_axum::ApiRouter::new().routes(::openapi_axum::routes![get_pet, create])
    }
}

fn main() {
    let _ = hygiene::router().split();
}
