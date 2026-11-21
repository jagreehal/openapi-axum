//! `Multipart<T>` bodies: documented from `T`, validated against the published schema,
//! read into `FilePart` fields, and exercised by the conformance harness.

use axum::Json;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, FromRequest, Request};
use axum::http::StatusCode;
use http_body_util::BodyExt;
use openapi_axum::testing::{Conformance, assert_conforms};
use openapi_axum::{ApiResponse, ApiRouter, DocCtx, ExtractorDoc, FilePart, Multipart, Rejection, route, routes};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tower::ServiceExt;
use utoipa::ToSchema;
use utoipa::openapi::path::Operation;

#[derive(Deserialize, ToSchema)]
struct Upload {
    file: FilePart,
    #[schema(min_length = 1, max_length = 20)]
    note: String,
    #[schema(minimum = 1, maximum = 10)]
    copies: Option<u8>,
    tags: Option<Vec<String>>,
    attachments: Option<Vec<FilePart>>,
    thumbnail: Option<FilePart>,
}

#[derive(Serialize, ToSchema)]
struct Received {
    filename: Option<String>,
    content_type: Option<String>,
    size: usize,
    note: String,
    copies: Option<u8>,
    tags: Vec<String>,
    attachments: usize,
    thumbnail: bool,
}

#[derive(Serialize, ToSchema)]
struct Problem {
    message: String,
}

#[derive(ApiResponse)]
enum ApiError {
    /// The request was malformed.
    #[status(BAD_REQUEST)]
    BadRequest(Problem),
    /// The body is larger than the route allows.
    #[status(PAYLOAD_TOO_LARGE)]
    TooLarge(Problem),
    /// The body broke its schema.
    #[status(UNPROCESSABLE_ENTITY)]
    Invalid(Problem),
}

impl From<Rejection> for ApiError {
    fn from(r: Rejection) -> Self {
        let problem = Problem { message: r.message };
        match r.status {
            StatusCode::PAYLOAD_TOO_LARGE => Self::TooLarge(problem),
            StatusCode::UNPROCESSABLE_ENTITY => Self::Invalid(problem),
            _ => Self::BadRequest(problem),
        }
    }
}

/// Upload a file with a note.
#[route(POST, "/files", tag = "Files")]
async fn upload(Multipart(u): Multipart<Upload>) -> Result<Json<Received>, ApiError> {
    Ok(Json(Received {
        filename: u.file.filename,
        content_type: u.file.content_type,
        size: u.file.bytes.len(),
        note: u.note,
        copies: u.copies,
        tags: u.tags.unwrap_or_default(),
        attachments: u.attachments.map_or(0, |a| a.len()),
        thumbnail: u.thumbnail.is_some(),
    }))
}

/// A path-qualified extractor is recognized too.
#[route(POST, "/qualified")]
async fn qualified(openapi_axum::Multipart(u): openapi_axum::Multipart<Upload>) -> Result<Json<usize>, ApiError> {
    Ok(Json(u.file.bytes.len()))
}

fn app() -> (axum::Router, utoipa::openapi::OpenApi) {
    let (router, openapi) = ApiRouter::new().routes(routes![upload, qualified]).split();
    (router.layer(DefaultBodyLimit::max(1024)), openapi)
}

enum Part<'a> {
    Text(&'a str, &'a [u8]),
    File(&'a str, &'a str, &'a [u8]),
}

fn multipart(parts: &[Part]) -> Request<Body> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(b"--X\r\n");
        let (head, content) = match part {
            Part::Text(name, value) => (format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n"), *value),
            Part::File(name, filename, value) => (
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: text/csv\r\n\r\n"
                ),
                *value,
            ),
        };
        body.extend_from_slice(head.as_bytes());
        body.extend_from_slice(content);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"--X--\r\n");
    Request::post("/files").header("content-type", "multipart/form-data; boundary=X").body(Body::from(body)).unwrap()
}

async fn send(req: Request<Body>) -> (StatusCode, Value) {
    let (router, _) = app();
    let res = router.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[test]
fn the_body_is_documented_as_multipart_with_binary_files() {
    let (_, openapi) = app();
    let spec = serde_json::to_value(&openapi).unwrap();
    let content = &spec["paths"]["/files"]["post"]["requestBody"]["content"];
    assert_eq!(content.as_object().unwrap().keys().collect::<Vec<_>>(), ["multipart/form-data"]);
    let schema = &content["multipart/form-data"]["schema"];
    let upload = match schema.get("$ref").and_then(Value::as_str) {
        Some(r) => spec.pointer(r.trim_start_matches('#')).unwrap(),
        None => schema,
    };
    assert_eq!(upload["required"], json!(["file", "note"]));
    let file = &upload["properties"]["file"];
    let file = match file.get("$ref").and_then(Value::as_str) {
        Some(r) => spec.pointer(r.trim_start_matches('#')).unwrap(),
        None => file,
    };
    assert_eq!(file, &json!({ "type": "string", "format": "binary", "contentMediaType": "application/octet-stream" }));
    assert!(spec["paths"]["/qualified"]["post"]["requestBody"]["content"]["multipart/form-data"].is_object());
}

#[tokio::test]
async fn parts_are_read_into_the_struct() {
    let (status, body) = send(multipart(&[
        Part::File("file", "statement.csv", b"date,amount\n"),
        Part::Text("note", b"May"),
        Part::Text("copies", b"3"),
        Part::Text("tags", b"a"),
        Part::Text("tags", b"b"),
        Part::File("attachments", "one.pdf", b"1"),
        Part::File("attachments", "two.pdf", b"2"),
    ]))
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({
            "filename": "statement.csv", "content_type": "text/csv", "size": 12, "note": "May",
            "copies": 3, "tags": ["a", "b"], "attachments": 2, "thumbnail": false,
        })
    );
}

#[tokio::test]
async fn a_file_field_takes_a_part_without_a_filename() {
    let (status, body) = send(multipart(&[Part::Text("file", b"raw"), Part::Text("note", b"x")])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!((body["filename"].clone(), body["size"].clone()), (Value::Null, json!(3)));
}

#[tokio::test]
async fn parts_that_break_the_schema_are_rejected() {
    for (label, parts, field) in [
        ("missing file", vec![Part::Text("note", b"x")], "file"),
        ("missing note", vec![Part::File("file", "a.csv", b"x")], "note"),
        ("note too long", vec![Part::File("file", "a.csv", b"x"), Part::Text("note", &[b'n'; 21])], "note"),
        (
            "copies above maximum",
            vec![Part::File("file", "a.csv", b"x"), Part::Text("note", b"x"), Part::Text("copies", b"11")],
            "copies",
        ),
        (
            "copies not a number",
            vec![Part::File("file", "a.csv", b"x"), Part::Text("note", b"x"), Part::Text("copies", b"x")],
            "copies",
        ),
        (
            "two files for one",
            vec![Part::File("file", "a.csv", b"x"), Part::File("file", "b.csv", b"y"), Part::Text("note", b"x")],
            "file",
        ),
    ] {
        let (status, body) = send(multipart(&parts)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{label}: {body}");
        let message = body["message"].as_str().unwrap();
        assert!(message.contains(&format!("`{field}`")) || message.contains(field), "{label}: {message}");
    }
}

#[tokio::test]
async fn malformed_bodies_keep_axums_status() {
    let not_multipart =
        Request::post("/files").header("content-type", "application/json").body(Body::from("{}")).unwrap();
    assert_eq!(send(not_multipart).await.0, StatusCode::BAD_REQUEST);

    let not_utf8 = multipart(&[Part::File("file", "a.csv", b"x"), Part::Text("note", &[0xff, 0xfe])]);
    let (status, body) = send(not_utf8).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["message"], "field `note`: a text part must be UTF-8");

    let too_large = multipart(&[Part::File("file", "a.csv", &[b'x'; 4096]), Part::Text("note", b"x")]);
    assert_eq!(send(too_large).await.0, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn the_app_conforms_with_generated_multipart_cases() {
    let (router, openapi) = app();
    let report = assert_conforms(&router, &openapi).await;
    let files = report.get("POST /files").unwrap();
    assert!(files.valid_cases > 0 && files.invalid_cases > 0, "{report}");
    assert!(files.statuses.contains(&422), "{report}");
    assert!(!files.skipped.iter().any(|s| s.contains("body")), "{report}");
}

// ---------- the harness catches a multipart body the handler doesn't enforce ----------

/// Documents a multipart body with a required `file`, but accepts anything.
struct Lenient;

impl<S: Send + Sync> FromRequest<S> for Lenient {
    type Rejection = StatusCode;
    async fn from_request(_: Request, _: &S) -> Result<Self, StatusCode> {
        Ok(Self)
    }
}

impl ExtractorDoc for Lenient {
    fn document(ctx: &mut DocCtx, op: &mut Operation) {
        ctx.multipart_body::<Upload>(op);
        ctx.json_response::<Problem>(&mut op.responses, 422, None);
    }
}

#[route(POST, "/lenient")]
async fn lenient(_: Lenient) -> Json<u8> {
    Json(1)
}

#[tokio::test]
async fn conformance_catches_an_unenforced_multipart_body() {
    let (router, openapi) = ApiRouter::new().routes(routes![lenient]).split();
    Conformance::new(&router, &openapi).cases(0).run().await;
    let err = tokio::spawn(async move { assert_conforms(&router, &openapi).await }).await.expect_err("should fail");
    let message = err.into_panic().downcast::<String>().map(|m| *m).unwrap_or_default();
    assert!(message.contains("POST /lenient") && message.contains("generated invalid request"), "{message}");
}
