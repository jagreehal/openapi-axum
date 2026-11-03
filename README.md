# openapi-axum

[![crates.io](https://img.shields.io/crates/v/openapi-axum.svg)](https://crates.io/crates/openapi-axum)
[![docs.rs](https://docs.rs/openapi-axum/badge.svg)](https://docs.rs/openapi-axum)
[![CI](https://github.com/jagreehal/openapi-axum/actions/workflows/ci.yml/badge.svg)](https://github.com/jagreehal/openapi-axum/actions/workflows/ci.yml)

Documentation: [jagreehal.github.io/openapi-axum](https://jagreehal.github.io/openapi-axum/)

You write ordinary axum handlers. openapi-axum reads their signatures and builds the OpenAPI document from them. At runtime it checks requests against that same document.

You declare each endpoint once. For typed handlers the response docs come from the return type, so returning a body or status the docs don't list is a compile error. Custom extractors, middleware and `#[raw]` arguments can still behave in ways the document doesn't describe. For those, openapi-axum gives you runtime checks and a test harness to catch the mismatch.

```rust
use axum::{Json, extract::{Path, State}};
use openapi_axum::{ApiResponse, ApiRouter, Rejection, route, routes};
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
struct Todo {
    id: u32,
    title: String,
}

#[derive(Serialize, ToSchema)]
struct ErrorBody {
    message: String,
}

#[derive(ApiResponse)]
enum ApiError {
    /// The request was invalid.
    #[status(BAD_REQUEST)]
    BadRequest(ErrorBody),
    /// No todo with that id.
    #[status(NOT_FOUND)]
    NotFound(ErrorBody),
}

// Malformed JSON, query strings and paths arrive here, in your error shape.
impl From<Rejection> for ApiError {
    fn from(r: Rejection) -> Self {
        Self::BadRequest(ErrorBody { message: r.message })
    }
}

#[route(GET, "/todos/{id}", tag = "Todos")]
/// Get a todo.
async fn get_todo(State(prefix): State<String>, Path(id): Path<u32>) -> Result<Json<Todo>, ApiError> {
    match id {
        1 => Ok(Json(Todo { id, title: format!("{prefix} one") })),
        _ => Err(ApiError::NotFound(ErrorBody { message: "no such todo".into() })),
    }
}

let (router, openapi) = ApiRouter::new()
    .routes(routes![get_todo])
    .spec_route("/openapi.json")
    .with_state("todo".to_string())
    .split();
# let _: axum::Router = router;
# assert!(openapi.paths.paths.contains_key("/todos/{id}"));
```

`router` is a plain `axum::Router`. `openapi` describes `GET /todos/{id}` with an integer `id` path parameter, a `200` returning `Todo`, and `400` and `404` returning `ErrorBody`, each with the description from its doc comment. You didn't write any of that a second time.

## Install

```toml
[dependencies]
openapi-axum = { version = "0.4", features = ["uuid"] }
axum = "0.8"
utoipa = "6"
serde = { version = "1", features = ["derive"] }
```

Your request and response types derive `utoipa::ToSchema`. openapi-axum uses utoipa for schemas and builds everything else.

## How the document is built

| In the handler | In the OpenAPI document |
|---|---|
| Return `Json<T>`, `Created<T>`, `Accepted<T>`, `NoContent` | `200`, `201`, `202`, `204` with `T`'s schema |
| Return `Result<T, E>` | the responses of `T` and of `E` |
| A `#[derive(ApiResponse)]` enum | one response per variant, using its `#[status]` |
| `Path<u32>`, `Path<(String, u32)>`, `Path<SomeStruct>` | path parameters, named from the route template |
| `Query<T>` | query parameters from `T`'s fields |
| `Json<T>` or `Form<T>` argument | the request body (a `GET` form becomes query parameters) |
| `///` doc comment | summary (first line) and description (the rest) |
| `#[deprecated]`, the fn name | `deprecated`, `operationId` |

Query and path structs can derive either `utoipa::IntoParams` or `utoipa::ToSchema`. You don't need `#[into_params(parameter_in = ...)]`: the extractor already says where the values come from.

## Responses

A response enum lists every status a handler can return. The same `#[status]` sets the status your client receives and the status in the docs.

```rust
use openapi_axum::{ApiResponse, HeaderText, InvalidHeaderText};
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
struct Todo { id: u32 }

#[derive(ApiResponse)]
enum CreateTodo {
    /// Created.
    #[status(CREATED)]
    Created {
        #[header("Location")]
        location: HeaderText,
        #[body]
        todo: Todo,
    },
    /// A todo with that title exists.
    #[status(409)]
    Conflict,
}

fn created(todo: Todo) -> Result<CreateTodo, InvalidHeaderText> {
    let location = HeaderText::new(format!("/todos/{}", todo.id))?;
    Ok(CreateTodo::Created { location, todo })
}
# assert!(created(Todo { id: 7 }).is_ok());
```

`Location` goes out on every `Created` response and appears in the docs. Unit variants have no body. Generic enums work too.

A `#[header]` field holds a `HeaderText` or an `http::HeaderValue`. Each is a valid header value once it exists, so sending the header can't fail. The `ResponseHeader` trait behind this is sealed, so no other implementation can break that. Convert your own types into a `HeaderText` where you build the response. `HeaderText::new` checks the text where you build it, and `HeaderText::from_static` takes a literal. A `String` field is a compile error, since a string can hold a newline. Header names are checked at compile time too.

If you return `Result<_, E>`, `?` works with your domain errors through `From`. A handler can only produce the variants it has, so an undocumented status is a compile error.

## Request validation

openapi-axum validates input against the published document before your handler runs:

- JSON and form bodies against their schema,
- query and path values against their parameter schemas.

So `#[schema(min_length = 1, maximum = 100)]`, `format = "uuid"`, enums and patterns hold at runtime too. A failed check reaches your error type as a `Rejection` with a message such as ``query parameter `limit`: 500 is greater than the maximum of 100``. Changes you make with `map_openapi` apply to validation as well.

To turn it off, leave out the `validation` feature.

## Custom extractors

Auth, header and cookie extractors describe themselves by implementing `ExtractorDoc`. Each route that takes the extractor gets those docs.

```rust
use openapi_axum::{DocCtx, ExtractorDoc};
use utoipa::openapi::path::Operation;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};

struct Bearer(String);

impl ExtractorDoc for Bearer {
    fn document(ctx: &mut DocCtx, op: &mut Operation) {
        let scheme = SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build());
        ctx.security(op, "bearer", &[], scheme);
        ctx.empty_response(&mut op.responses, 401, Some("Missing or invalid token"));
    }
}
```

`ctx.header_param` and `ctx.cookie_param` cover headers and cookies. Rust can't check these docs against your extractor, so the next section tests them.

## Checking an app against its own docs

With the `testing` feature, one call sends each documented operation a request built from nothing but the document, then checks each response against the document.

```rust,ignore
#[tokio::test]
async fn conforms_to_its_docs() {
    let (router, openapi) = app();
    openapi_axum::testing::assert_conforms(&router, &openapi).await;
}
```

`assert_conforms` starts with one request per operation, built from the document. Then it generates more from the schemas:

- **Valid variants**: other values, boundary values such as a `maximum` or `maxLength`, and optional params and fields. Each one must succeed the way the first request did.
- **Invalid variants**: one path param, query param or JSON body broken at a time (out of range, too long, wrong type, a required field missing). Each one must get a 400, 415 or 422 that the document lists.

The cases are deterministic, so a failure names the request and reproduces on every run:

```text
POST /tags (generated invalid request, body: `name`: longer than maxLength, value {"name":"aaaaaaaaaaa"}): answered 200 OK
```

It catches an extractor that needs something the docs leave out, a handler that accepts what the docs forbid, a serde rule stricter than the schema, and responses the docs don't list.

The run returns a report. Print it to see what each operation exercised: the statuses it got, the documented statuses no request reached, and the inputs it couldn't vary.

```text
GET /codes/{code}: 1 valid, 1 invalid, statuses 200, 400
  skipped valid variants: path parameter `code` uses its example only (a `pattern` can't be generated)
GET /items/{id}: 9 valid, 3 invalid, statuses 400, 404; not exercised: 200
GET /secret-header: 9 valid, 0 invalid, statuses 200; not exercised: 400
  skipped invalid values: header parameter `x-api-key` isn't generated
```

Here no generated id exists, so `GET /items/{id}` never returned its `200`. Point the test at your data with `Conformance`:

```rust,ignore
let report = Conformance::new(&router, &openapi)
    .credential("bearer", &token)
    .fixture("id", 42)                                   // every param named `id`
    .fixture_for("GET /users/{id}", "id", 7)             // one operation
    .body_for("POST /todos", json!({ "title": "milk" })) // the docs-built body
    .before_each(|| async { db.reset().await })          // same state for every request
    .cases(16)
    .run()
    .await;
assert!(report.get("GET /items/{id}").unwrap().not_exercised.is_empty(), "{report}");
```

Valid variants keep fixture values, so reads and writes hit real rows; invalid variants still break them. `.expect("GET /path", status)` requires a status for one operation and skips its generated cases. `.cases(0)` sends one request per operation.

## Checking responses at runtime

`ApiRouter::verify_responses(VerifyMode::Panic)` checks each response against the document as it leaves. It sits outside your routes, so it sees what middleware and `#[raw]` arguments send.

- It checks the status, content type and documented header values of every response without reading the body. Header values are decoded the way OpenAPI serializes them, so an integer-array header reads `1, 2` as `[1, 2]`.
- It checks a `HEAD` response's status and headers against the `GET` operation and reports its body as skipped.
- It checks the body against its schema when the body has a known length of at most 1 MiB. Change the limit with `.verify_body_limit(bytes)`.
- It buffers a body only when it's eligible for that check. Streams (unknown length, `text/event-stream`) and bodies over the limit pass through with no buffering and no schema validation. Each skip is reported as a `tracing` event with its reason, so you can see how much traffic had its body checked.

The mode decides what happens when a check fails. `Panic` fails the test that caused it. `Log` records the failure and sends the response unchanged; it reports where a body fails and which rule, and leaves the values out of the message, since they may be user data.

To control the cost, `.verify_sample_every(n)` checks the body of one in every `n` responses of each operation, so a busy route can't crowd out a quiet one. Status and content type are still checked on every response, since they need no buffering. Unsampled bodies are reported as `skipped` with reason `not_sampled`.

## What gets checked, and when

Each guarantee has a test in the repository: compile errors in `tests/ui/fail`, the rest in `tests/`.

These fail at compile time:

- a body type or status the handler's return type doesn't allow,
- a tuple path extractor whose length doesn't match the template,
- duplicate statuses in a response enum, or a body on a `204`,
- a `#[header]` field whose type isn't a valid header value by construction (a `String`, say), an invalid header name, or an invalid literal in `HeaderText::from_static`,
- extractors that can reject a request in a handler that has no error type to put the rejection in.

These fail when the router is built, at startup or in your first test:

- a path struct whose fields don't match the template,
- one name used for two different schemas or security schemes,
- the same method and path registered twice,
- a `$ref` that doesn't resolve.

These get checked at runtime, on each request or response:

- request bodies, query and path values against the published schemas,
- with `verify_responses`, statuses, content types and bounded bodies, including what middleware sends.

These need a test, using `assert_conforms`:

- an `ExtractorDoc` that leaves out a header, credential or response its extractor needs, or promises constraints its extractor doesn't enforce,
- a schema that serde can't satisfy, such as a custom deserializer stricter than the type's schema.

The document can still describe less than your code does in one place. A custom deserializer that accepts more than its type's schema gets rejected by validation until you describe it with `#[schema(schema_with = ...)]`.

To catch breaking changes between releases, commit your generated document and run a diff tool such as [oasdiff](https://github.com/oasdiff/oasdiff) on it in CI. That answers a different question from the checks above: whether the new document still works for clients that use the old one. The [CI guide](https://jagreehal.github.io/openapi-axum/guides/ci/) has a workflow you can copy, and the [Schemathesis recipe](https://jagreehal.github.io/openapi-axum/testing/schemathesis/) adds random and stateful testing over HTTP.

## Routers

`ApiRouter` wraps `axum::Router` and keeps the document in step with it:

- `.routes(routes![a, b])` adds documented routes.
- `.route_undocumented(path, method_router)` adds a route that stays out of the docs.
- `.merge(other)` and `.nest("/v1", other)` combine routers and their docs, using axum's path rules.
- `.spec_route("/openapi.json")` serves the document.
- `.map_openapi(|o| ...)` sets the title, servers and security schemes.
- `.schema::<T>()` registers a schema that nothing else registers.
- `.split()` returns the `axum::Router` and the `OpenApi`.

Handlers stay ordinary async functions, so a unit test can call `get_todo(State(s), Path(1))` without a router.

## Cost

These are in-process microbenchmarks on an M1 Pro, not end-to-end HTTP latency. With validation compiled out, openapi-axum adds nothing per request beyond your handler: the same allocations as plain axum, and timings within noise. Request validation adds about 1 µs to a small JSON request and about 6 µs to a 1 KB nested one, 52 to 81% more than plain axum on these small workloads. For the nested payload, parsing into an intermediate `serde_json::Value` and converting it into your type accounts for about 79% of that, and the schema check for about 21%. `verify_responses` adds about 1.5 µs and 10.5 µs more. In a real service, network, database and handler work tend to outweigh this. [BENCHMARKS.md](BENCHMARKS.md) has allocation counts, the method, its limits, and how to run it yourself.

### Choosing what to turn on

- **Request validation** is on by default. It's a crate feature, so it applies to the whole build. To skip body validation for one argument, mark it `#[raw]`; that argument then keeps axum's own rejection. Measure before you turn validation off for performance.
- **In tests and CI**, add `verify_responses(VerifyMode::Panic)` and `assert_conforms`.
- **In production**, decide on `verify_responses` per workload. Use `VerifyMode::Log`, a body limit and `verify_sample_every(n)` to bound the cost. Its `tracing` events show what it checked and skipped.

## Features

| Feature | Default | Adds |
|---|---|---|
| `validation` | yes | request validation and `verify_responses` |
| `tracing` | yes | a span per handler, named after its `operationId` |
| `testing` | no | `openapi_axum::testing` |
| `uuid`, `chrono` | no | `Path<Uuid>`, `Path<DateTime<_>>`, `Path<NaiveDate>` |

## Limits

- Extractors count as axum's when you write the bare name (`Json<T>`) or an `axum::` path. A renamed import (`use axum::Json as J`) or a type alias makes them look custom.
- openapi-axum checks `ExtractorDoc` impls with `assert_conforms`. It can't prove them correct.
- Validation reads a JSON body twice: once into `serde_json::Value` to check it, then into your type.
- Recursive types need utoipa's `#[schema(no_recursion)]` on the recursive field.
- `assert_conforms` can't generate a string for a `pattern`. Give such params and fields an `example`, and it tells you which one when it's missing.
- Nesting under a prefix with a capture (`/tenant/{tenant}`) panics. Write the full path in `#[route]` and use `merge`.

## Agent skill

Install the [`openapi-axum`](skills/openapi-axum/SKILL.md) skill so your coding assistant uses the crate's API:

```bash
npx skills add jagreehal/openapi-axum --skill openapi-axum
```

## Status

0.4. The API may change before 1.0. Issues and small reproductions are welcome.

## License

MIT
