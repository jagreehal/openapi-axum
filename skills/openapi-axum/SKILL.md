---
name: openapi-axum
description: >
  Builds axum APIs with the openapi-axum crate: #[route] handlers, ApiRouter,
  ApiResponse enums, ExtractorDoc, request validation, verify_responses and
  conformance tests. Use when adding or documenting axum endpoints, generating
  an OpenAPI document from Rust handlers, or testing an API against its docs.
---

# openapi-axum

openapi-axum reads axum handler signatures and builds the OpenAPI document from them. It validates requests against that document, and its `testing` feature checks an app against it.

## Setup

```toml
[dependencies]
openapi-axum = "0.4"
axum = "0.8"
utoipa = "6"
serde = { version = "1", features = ["derive"] }

[dev-dependencies]
openapi-axum = { version = "0.4", features = ["testing"] }
```

Request and response types derive `utoipa::ToSchema`. Query and path structs derive `IntoParams` or `ToSchema`.

## A route

```rust
use axum::{Json, extract::Path};
use openapi_axum::{ApiResponse, ApiRouter, Rejection, route, routes};

#[derive(ApiResponse)]
enum ApiError {
    /// The request was invalid.
    #[status(BAD_REQUEST)]
    BadRequest(ErrorBody),
    /// No todo with that id.
    #[status(NOT_FOUND)]
    NotFound(ErrorBody),
}

impl From<Rejection> for ApiError {
    fn from(r: Rejection) -> Self {
        Self::BadRequest(ErrorBody { message: r.message })
    }
}

#[route(GET, "/todos/{id}", tag = "Todos")]
/// Get a todo.
async fn get_todo(Path(id): Path<u32>) -> Result<Json<Todo>, ApiError> {
    todo!()
}

let (router, openapi) = ApiRouter::new()
    .routes(routes![get_todo])
    .spec_route("/openapi.json")
    .split();
```

- A handler that takes `Json`, `Path`, `Query` or `Form` returns `Result<_, E>` where `E: From<Rejection>`. Rejections arrive in your error type.
- The first doc comment line is the summary, the rest the description. `#[deprecated]` marks the operation deprecated.
- `split()` returns a plain `axum::Router` and the `OpenApi`.

## Which API

| Need | API |
| --- | --- |
| Responses with their own statuses | `#[derive(ApiResponse)]` enum, `#[status(CREATED)]` per variant |
| A response header | `#[header("Location")] location: HeaderText` in a variant |
| A header value | `HeaderText::new(text)?`, `HeaderText::from_static("no-store")` |
| 201, 202, 204 | `Created<T>`, `Accepted<T>`, `NoContent` |
| Keep axum's rejection for one argument | `#[raw]` on the argument |
| Document a custom extractor | `impl ExtractorDoc`, with `ctx.security`, `ctx.header_param`, `ctx.cookie_param`, `ctx.empty_response` |
| A route outside the docs | `.route_undocumented(path, method_router)` |
| Combine routers | `.merge(other)`, `.nest("/v1", other)` |
| Title, servers, security schemes | `.map_openapi(\|o\| ..)` |
| A schema nothing else registers | `.schema::<T>()` |
| Check responses as they leave | `.verify_responses(VerifyMode::Panic)` in tests, `VerifyMode::Log` in production |
| Bound response checking | `.verify_body_limit(bytes)`, `.verify_sample_every(n)` |
| Test the app against its docs | `openapi_axum::testing::assert_conforms(&router, &openapi).await` |
| Real credentials, case count | `Conformance::new(&router, &openapi).credential("bearer", token).cases(16).run().await` |
| Existing ids and bodies | `.fixture("id", 42)`, `.fixture_for("GET /users/{id}", "id", 7)`, `.body_for("POST /todos", json!(..))` |
| Reset state per request | `.before_each(\|\| async { db.reset().await })` |
| See what a run covered | `println!("{report}")`; `report.get("GET /x").unwrap().not_exercised` |

## Testing

```rust
#[tokio::test]
async fn conforms_to_its_docs() {
    let (router, openapi) = app();
    openapi_axum::testing::assert_conforms(&router, &openapi).await;
}
```

`assert_conforms` sends a request per operation built from the document, then generated valid and invalid variants. Valid ones must succeed. Invalid ones must get a documented 400, 415 or 422. A failure message names the generated request.

The returned `Report` lists, per operation, the statuses seen, the documented statuses no request reached and the inputs it couldn't vary. When a `200` shows under "not exercised", add a fixture for an id that exists.

For CI, commit `openapi.json` with a snapshot test and run `oasdiff/oasdiff-action/breaking@v0` on pull requests. For random and stateful testing over HTTP, serve the app with `.spec_route("/openapi.json")` and run `uvx schemathesis run http://127.0.0.1:3000/openapi.json`.

## Rules to follow

- Header fields hold `HeaderText` or `http::HeaderValue`. A `String` header field is a compile error.
- Mark recursive fields `#[schema(no_recursion)]`.
- Give string fields with a `pattern` an `example`, so `assert_conforms` can build a valid value.
- Write the full path in `#[route]` and use `merge` instead of nesting under a prefix with a capture.
- Use the bare `Json<T>`, `Path<T>` names or `axum::` paths. A renamed import makes an extractor look custom.
