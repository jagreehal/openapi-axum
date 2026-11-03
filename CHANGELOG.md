# Changelog

## Unreleased

- Docs site: a landing page, highlighted code in both themes, 16px article text, and plain previous/next links.

## 0.4.0

- `assert_conforms` generates requests from the schemas: valid variants must succeed, and invalid ones must get a documented 400, 415 or 422. Set the count with `Conformance::cases(n)`.
- Conformance runs return a `Report`: per operation, the statuses seen, the documented statuses not exercised and the inputs not varied.
- `Conformance` takes fixtures (`fixture`, `fixture_for`, `body_for`) and a `before_each` hook for predictable state.
- `verify_responses` checks documented response header values, checks `HEAD` responses against the `GET` operation, and samples per operation with `verify_sample_every(n)`.
- With validation compiled out, routes carry no per-request layer: the same allocations as plain axum.
- `testing::invalid_values` returns labelled values that break a schema.
- Documentation site at [jagreehal.github.io/openapi-axum](https://jagreehal.github.io/openapi-axum/), with a CI guide and a Schemathesis recipe, and an `openapi-axum` agent skill.

## 0.3.0

- `ResponseHeader` is sealed: `#[header]` fields hold a `HeaderText` or an `http::HeaderValue`, so sending a header can't panic.
- `verify_sample_every(n)`: check the body of one in every `n` responses. Status and content type are still checked on every response. Unsampled bodies are reported as `skipped` with reason `not_sampled`.
- Docs: `VerifyMode` decides what happens on failure, not which responses get checked. A new `stages` benchmark times each step of request validation, and the docs state the cost split from it.

## 0.2.0

Breaking:

- `#[header]` fields must implement the new `ResponseHeader` trait: `HeaderText`, `http::HeaderValue`, or your own type. A `String` or other `Display` field is a compile error, with a message naming the replacements. A header is now a valid value before the response exists, so an invalid one can't turn a response into an undocumented `500`.
- Header names in `#[header("...")]` are checked at compile time.

Other changes:

- Request validation checks `is_valid` first and builds error messages only for invalid input.
- Route lookups clone an `Arc`, not the method and path strings.
- Benchmarks: a hand-written-checks baseline (the same constraints checked in Rust, with no allocations over plain axum) and a `breakdown` benchmark showing where request validation's allocations go.

## 0.1.1

- `verify_responses` reports each outcome as a `tracing` event (target `openapi_axum::verify`): `openapi.response_validation.passed`, `.skipped` (`body_too_large`, `unknown_length`, `streaming`) and `.failed` with a reason such as `schema_mismatch` or `status_not_documented`. You can now count how much traffic had its body checked.
- `verify_body_limit` works before or after `verify_responses`.
- `HeaderText`: a header value checked when you build it. In a `#[header]` field it can't turn a response into an undocumented `500`.
- With the `validation` feature off, JSON bodies deserialize directly, with no `serde_json::Value` round trip.
- Benchmarks (`benches/`, [BENCHMARKS.md](BENCHMARKS.md)): time and allocations per request against plain axum.
- Docs: `assert_conforms` described as a smoke test, with what it can miss.

## 0.1.0

First release.

- `#[route]` turns an async axum handler into a documented route. Parameters, request body, responses, summary, description, tags, `operationId` and `deprecated` come from the signature and doc comments.
- `#[derive(ApiResponse)]` sets the runtime status and the documented response from one `#[status]` per variant, with typed `#[header]` fields and generic enums.
- `Json<T>`, `Created<T>`, `Accepted<T>`, `NoContent`, `String` and `Result<T, E>` describe their own responses.
- Extractor rejections reach your error type through `From<Rejection>`.
- Request validation against the published document: JSON and form bodies, query and path values, including `style`/`explode` arrays.
- `ExtractorDoc` for auth, header and cookie extractors, with security schemes.
- `ApiRouter`: `routes!`, `merge`, `nest` (axum's path rules), `spec_route`, `map_openapi`, `schema::<T>()`, and checks at registration for path, schema-name, security-scheme, duplicate-route and dangling-`$ref` problems.
- `verify_responses` checks outgoing responses against the document: status and content type on every response, bodies up to `verify_body_limit` (1 MiB by default). Streams and larger bodies pass through unbuffered. `VerifyMode::Log` leaves values out of its messages.
- `testing` feature: `assert_conforms` and `Conformance` test an app against its own docs.
