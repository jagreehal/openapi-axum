# Changelog

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
