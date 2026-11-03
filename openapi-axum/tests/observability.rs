//! `verify_responses` reports each outcome as a `tracing` event (target
//! `openapi_axum::verify`) with a stable `reason`, so you can tell how much traffic
//! had its body checked, and how much failed.

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::body::Body;
use axum::http::Request;
use openapi_axum::{ApiRouter, VerifyMode, route, routes};
use tower::ServiceExt;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};

#[derive(Default, Clone)]
struct Captured(Arc<Mutex<Vec<(String, String, String)>>>);

struct Fields(Vec<(String, String)>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push((field.name().to_owned(), format!("{value:?}").trim_matches('"').to_owned()));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push((field.name().to_owned(), value.to_owned()));
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Captured {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() != "openapi_axum::verify" {
            return;
        }
        let mut f = Fields(Vec::new());
        event.record(&mut f);
        let get = |k: &str| f.0.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
        let message = format!("{} {}", get("message"), get("operation"));
        self.0.lock().unwrap().push((get("event"), get("reason"), message));
    }
}

#[route(GET, "/ok")]
async fn ok() -> Json<u8> {
    Json(1)
}

/// `x-body: valid|invalid|large|stream`, optional `x-ct`.
async fn rewrite(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let kind = req.headers().get("x-body").map(|v| v.to_str().unwrap().to_owned());
    let ct = req.headers().get("x-ct").map(|v| v.to_str().unwrap().to_owned());
    let res = next.run(req).await;
    let Some(kind) = kind else { return res };
    let (mut parts, _) = res.into_parts();
    parts.headers.remove("content-length");
    if let Some(ct) = ct {
        parts.headers.insert("content-type", ct.parse().unwrap());
    }
    let body = match kind.as_str() {
        "valid" => Body::from("7"),
        "invalid" => Body::from(r#""secret-value""#),
        "large" => Body::from(format!("\"{}\"", "x".repeat(4096))),
        _ => Body::from_stream(futures_util::stream::iter([Ok::<_, std::io::Error>("1")])),
    };
    axum::response::Response::from_parts(parts, body)
}

async fn events_for(kind: &str, ct: Option<&str>) -> Vec<(String, String, String)> {
    let captured = Captured::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let router = ApiRouter::new()
        .routes(routes![ok])
        .layer(axum::middleware::from_fn(rewrite))
        .verify_responses(VerifyMode::Log)
        .verify_body_limit(1024)
        .split()
        .0;
    let mut req = Request::get("/ok").header("x-body", kind);
    if let Some(ct) = ct {
        req = req.header("x-ct", ct);
    }
    let res = router.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let _ = http_body_util::BodyExt::collect(res.into_body()).await.unwrap();
    captured.0.lock().unwrap().clone()
}

fn only(events: &[(String, String, String)]) -> (&str, &str) {
    assert_eq!(events.len(), 1, "{events:?}");
    (events[0].0.as_str(), events[0].1.as_str())
}

#[tokio::test]
async fn checked_bodies_report_passed() {
    let events = events_for("valid", None).await;
    assert_eq!(only(&events), ("openapi.response_validation.passed", "body_checked"));
}

#[tokio::test]
async fn skipped_bodies_report_why() {
    let large = events_for("large", None).await;
    assert_eq!(only(&large), ("openapi.response_validation.skipped", "body_too_large"));
    let stream = events_for("stream", None).await;
    assert_eq!(only(&stream), ("openapi.response_validation.skipped", "unknown_length"));
}

#[tokio::test]
async fn failures_report_a_reason_without_values() {
    let invalid = events_for("invalid", None).await;
    assert_eq!(only(&invalid), ("openapi.response_validation.failed", "schema_mismatch"));
    assert!(!invalid[0].2.contains("secret-value"), "Log mode keeps values out: {}", invalid[0].2);
    assert!(invalid[0].2.contains("GET /ok"), "names the operation: {}", invalid[0].2);

    let wrong_type = events_for("valid", Some("application/xml")).await;
    assert_eq!(only(&wrong_type), ("openapi.response_validation.failed", "content_type_not_documented"));
}

async fn run_sampled(every: u32, requests: usize, kind: &str, teapot: bool) -> Vec<(String, String, String)> {
    let captured = Captured::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let router = ApiRouter::new()
        .routes(routes![ok])
        .layer(axum::middleware::from_fn(rewrite))
        .layer(axum::middleware::from_fn(teapot_layer))
        .verify_responses(VerifyMode::Log)
        .verify_sample_every(every)
        .split()
        .0;
    for _ in 0..requests {
        let mut req = Request::get("/ok").header("x-body", kind);
        if teapot {
            req = req.header("x-teapot", "1");
        }
        let res = router.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let _ = http_body_util::BodyExt::collect(res.into_body()).await.unwrap();
    }
    captured.0.lock().unwrap().clone()
}

/// Answers 418 (undocumented) when `x-teapot` is set.
async fn teapot_layer(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    if req.headers().contains_key("x-teapot") {
        let mut res = axum::response::IntoResponse::into_response(axum::http::StatusCode::IM_A_TEAPOT);
        *res.body_mut() = Body::empty();
        return res;
    }
    next.run(req).await
}

fn count(events: &[(String, String, String)], event: &str, reason: &str) -> usize {
    events.iter().filter(|(e, r, _)| e == event && r == reason).count()
}

#[tokio::test]
async fn sampling_checks_one_body_in_every_n() {
    let events = run_sampled(3, 9, "invalid", false).await;
    assert_eq!(count(&events, "openapi.response_validation.failed", "schema_mismatch"), 3, "{events:?}");
    assert_eq!(count(&events, "openapi.response_validation.skipped", "not_sampled"), 6, "{events:?}");
}

#[tokio::test]
async fn sampling_never_skips_status_and_content_type_checks() {
    // Every response is checked for an undocumented status, sampled or not.
    let events = run_sampled(1000, 5, "valid", true).await;
    assert_eq!(count(&events, "openapi.response_validation.failed", "status_not_documented"), 5, "{events:?}");
}

#[route(GET, "/other")]
async fn other() -> Json<u8> {
    Json(2)
}

/// Run `f` with events captured, on any runtime thread.
async fn capture<F: std::future::Future>(f: F) -> (F::Output, Vec<(String, String, String)>) {
    use tracing::instrument::WithSubscriber;
    let captured = Captured::default();
    let out = f.with_subscriber(tracing_subscriber::registry().with(captured.clone())).await;
    let events = captured.0.lock().unwrap().clone();
    (out, events)
}

async fn get(router: &axum::Router, uri: &str, kind: Option<&str>) -> (axum::http::StatusCode, Vec<u8>) {
    let mut req = Request::get(uri);
    if let Some(kind) = kind {
        req = req.header("x-body", kind);
    }
    let res = router.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = res.status();
    (status, http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes().to_vec())
}

fn passed_for(events: &[(String, String, String)], op: &str) -> usize {
    events.iter().filter(|(e, _, m)| e == "openapi.response_validation.passed" && m.ends_with(op)).count()
}

#[test]
#[should_panic(expected = "verify_sample_every(0)")]
fn sampling_zero_is_rejected() {
    let _ = ApiRouter::<()>::new().verify_sample_every(0);
}

#[tokio::test]
async fn sampling_one_checks_every_body() {
    let events = run_sampled(1, 4, "valid", false).await;
    assert_eq!(count(&events, "openapi.response_validation.passed", "body_checked"), 4, "{events:?}");
    assert_eq!(count(&events, "openapi.response_validation.skipped", "not_sampled"), 0, "{events:?}");
}

#[tokio::test]
async fn sampling_counts_per_operation() {
    // Alternating A, B with n = 2: each operation keeps its own count.
    let router =
        ApiRouter::new().routes(routes![ok, other]).verify_responses(VerifyMode::Log).verify_sample_every(2).split().0;
    let ((), events) = capture(async {
        for _ in 0..4 {
            get(&router, "/ok", None).await;
            get(&router, "/other", None).await;
        }
    })
    .await;
    assert_eq!(passed_for(&events, "GET /ok"), 2, "{events:?}");
    assert_eq!(passed_for(&events, "GET /other"), 2, "{events:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sampling_is_exact_under_concurrency() {
    let router =
        ApiRouter::new().routes(routes![ok]).verify_responses(VerifyMode::Log).verify_sample_every(4).split().0;
    let ((), events) = capture(async {
        let tasks: Vec<_> = (0..200)
            .map(|_| {
                use tracing::instrument::WithSubscriber;
                let router = router.clone();
                tokio::spawn(async move { get(&router, "/ok", None).await }.with_current_subscriber())
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
    })
    .await;
    assert_eq!(count(&events, "openapi.response_validation.passed", "body_checked"), 50);
    assert_eq!(count(&events, "openapi.response_validation.skipped", "not_sampled"), 150);
}

#[tokio::test]
async fn sampling_works_through_nesting() {
    let api = ApiRouter::new()
        .nest("/v1", ApiRouter::new().routes(routes![ok]))
        .verify_responses(VerifyMode::Log)
        .verify_sample_every(2)
        .split()
        .0;
    // Also mounted inside a plain axum Router, under a prefix the document doesn't know.
    let outer = axum::Router::new().nest("/api", api);
    let ((), events) = capture(async {
        for _ in 0..4 {
            assert_eq!(get(&outer, "/api/v1/ok", None).await.0, 200);
        }
    })
    .await;
    assert_eq!(passed_for(&events, "GET /v1/ok"), 2, "{events:?}");
    assert_eq!(count(&events, "openapi.response_validation.skipped", "not_sampled"), 2, "{events:?}");
}

fn rewriting_router(every: u32) -> axum::Router {
    ApiRouter::new()
        .routes(routes![ok])
        .layer(axum::middleware::from_fn(rewrite))
        .verify_responses(VerifyMode::Log)
        .verify_body_limit(1024)
        .verify_sample_every(every)
        .split()
        .0
}

#[tokio::test]
async fn bodies_pass_through_byte_for_byte() {
    let large = format!("\"{}\"", "x".repeat(4096));
    let cases: [(&str, &[u8]); 4] =
        [("valid", b"7"), ("invalid", b"\"secret-value\""), ("large", large.as_bytes()), ("stream", b"1")];
    // n = 2: each kind is sent twice, once checked and once not sampled.
    for (kind, want) in cases {
        let router = rewriting_router(2);
        for _ in 0..2 {
            let (status, body) = get(&router, "/ok", Some(kind)).await;
            assert_eq!(status, 200, "{kind}");
            assert_eq!(body, want, "{kind}: body changed by verification");
        }
    }
}

/// Claims an exact length of 1 byte, then sends `len` bytes.
struct OversizedBody(usize);

impl http_body::Body for OversizedBody {
    type Data = axum::body::Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let n = std::mem::take(&mut self.0);
        std::task::Poll::Ready((n > 0).then(|| Ok(http_body::Frame::data(vec![b'1'; n].into()))))
    }
    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(1)
    }
}

async fn reshape(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let mode = req.headers().get("x-mode").map(|v| v.to_str().unwrap().to_owned());
    let res = next.run(req).await;
    let (mut parts, body) = res.into_parts();
    match mode.as_deref() {
        Some("oversized") => return axum::response::Response::from_parts(parts, Body::new(OversizedBody(1 << 20))),
        Some("short-content-length") => {
            parts.headers.insert("content-length", "1".parse().unwrap());
            return axum::response::Response::from_parts(parts, Body::from(vec![b'1'; 4096]));
        }
        Some("non-utf8-ct") => {
            parts.headers.insert("content-type", axum::http::HeaderValue::from_bytes(b"application/\xffjson").unwrap());
        }
        Some("ct-params") => {
            parts.headers.insert("content-type", "application/json; charset=utf-8".parse().unwrap());
        }
        _ => {}
    }
    axum::response::Response::from_parts(parts, body)
}

async fn reshaped(mode: &str) -> (axum::http::StatusCode, usize, Vec<(String, String, String)>) {
    let router = ApiRouter::new()
        .routes(routes![ok])
        .layer(axum::middleware::from_fn(reshape))
        .verify_responses(VerifyMode::Log)
        .verify_body_limit(1024)
        .split()
        .0;
    let ((status, len), events) = capture(async {
        let req = Request::get("/ok").header("x-mode", mode).body(Body::empty()).unwrap();
        let res = router.oneshot(req).await.unwrap();
        let status = res.status();
        (status, http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes().len())
    })
    .await;
    (status, len, events)
}

#[tokio::test]
async fn an_oversized_body_is_bounded_and_reported() {
    // Claims 1 byte, sends 1 MiB: buffering stops at the 1 KiB limit and it's reported.
    let (status, _, events) = reshaped("oversized").await;
    assert_eq!(status, 500);
    assert_eq!(only(&events), ("openapi.response_validation.failed", "body_unreadable"));
}

#[tokio::test]
async fn content_length_headers_are_not_used_for_sizing() {
    // Sizing comes from the body itself, so a 4 KiB body claiming 1 byte isn't buffered.
    let (status, len, events) = reshaped("short-content-length").await;
    assert_eq!((status.as_u16(), len), (200, 4096));
    assert_eq!(only(&events), ("openapi.response_validation.skipped", "body_too_large"));
}

#[tokio::test]
async fn malformed_and_parameterised_content_types() {
    let (status, _, events) = reshaped("non-utf8-ct").await;
    assert_eq!(status, 200, "Log mode never changes the response");
    assert_eq!(only(&events).0, "openapi.response_validation.failed");
    let (_, _, events) = reshaped("ct-params").await;
    assert_eq!(only(&events), ("openapi.response_validation.passed", "body_checked"));
}

#[derive(openapi_axum::ApiResponse)]
enum Limited {
    /// Rate limited.
    #[status(200)]
    Ok {
        #[header("X-Remaining")]
        remaining: openapi_axum::HeaderText,
        #[body]
        count: u8,
    },
}

#[route(GET, "/limited")]
async fn limited() -> Limited {
    Limited::Ok { remaining: openapi_axum::HeaderText::from_static("5"), count: 1 }
}

#[route(GET, "/counted")]
/// Documents `X-Count` as an integer header.
async fn counted() -> Json<u8> {
    Json(1)
}

/// Sets `x-count` from the request's `x-set` header.
async fn set_count(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let set = req.headers().get("x-set").cloned();
    let ids: Vec<_> = req.headers().get_all("x-ids-set").iter().cloned().collect();
    let mut res = next.run(req).await;
    if let Some(v) = set {
        res.headers_mut().insert("x-count", v);
    }
    for v in ids {
        res.headers_mut().append("x-ids", v);
    }
    res
}

fn counted_router() -> axum::Router {
    ApiRouter::new()
        .routes(routes![counted, limited])
        .map_openapi(|mut o| {
            let ok =
                o.paths.paths.get_mut("/counted").unwrap().get.as_mut().unwrap().responses.responses.get_mut("200");
            if let Some(utoipa::openapi::RefOr::T(r)) = ok {
                let schema = utoipa::openapi::ObjectBuilder::new()
                    .schema_type(utoipa::openapi::schema::Type::Integer)
                    .minimum(Some(0))
                    .build();
                r.headers
                    .insert("X-Count".into(), utoipa::openapi::RefOr::T(utoipa::openapi::header::Header::new(schema)));
                let ids = utoipa::openapi::ArrayBuilder::new()
                    .items(utoipa::openapi::ObjectBuilder::new().schema_type(utoipa::openapi::schema::Type::Integer))
                    .build();
                r.headers.insert("X-Ids".into(), utoipa::openapi::RefOr::T(utoipa::openapi::header::Header::new(ids)));
            }
            o
        })
        .layer(axum::middleware::from_fn(set_count))
        .verify_responses(VerifyMode::Log)
        .split()
        .0
}

async fn count_events(router: &axum::Router, req: Request<Body>) -> Vec<(String, String, String)> {
    let router = router.clone();
    capture(async move {
        let res = router.oneshot(req).await.unwrap();
        let _ = http_body_util::BodyExt::collect(res.into_body()).await.unwrap();
    })
    .await
    .1
}

#[tokio::test]
async fn documented_header_values_are_checked() {
    let router = counted_router();
    let ok = count_events(&router, Request::get("/counted").header("x-set", "3").body(Body::empty()).unwrap()).await;
    assert_eq!(only(&ok), ("openapi.response_validation.passed", "body_checked"));
    let text =
        count_events(&router, Request::get("/counted").header("x-set", "many").body(Body::empty()).unwrap()).await;
    assert_eq!(only(&text), ("openapi.response_validation.failed", "header_mismatch"));
    assert!(!text[0].2.contains("many"), "Log mode keeps values out: {}", text[0].2);
    let negative =
        count_events(&router, Request::get("/counted").header("x-set", "-1").body(Body::empty()).unwrap()).await;
    assert_eq!(only(&negative).1, "header_mismatch");
    // A typed `#[header]` is documented as a string, and passes.
    let typed = count_events(&router, Request::get("/limited").body(Body::empty()).unwrap()).await;
    assert_eq!(only(&typed), ("openapi.response_validation.passed", "body_checked"));
}

#[tokio::test]
async fn head_responses_check_status_and_headers_without_a_body() {
    let router = counted_router();
    let head = count_events(&router, Request::head("/counted").header("x-set", "3").body(Body::empty()).unwrap()).await;
    assert_eq!(only(&head), ("openapi.response_validation.skipped", "head_request"));
    let bad = count_events(&router, Request::head("/counted").header("x-set", "x").body(Body::empty()).unwrap()).await;
    assert_eq!(only(&bad), ("openapi.response_validation.failed", "header_mismatch"));
}

#[tokio::test]
async fn array_headers_are_decoded_as_comma_separated_lists() {
    let router = counted_router();
    let get = |ids: &[&str]| {
        let mut req = Request::get("/counted");
        for v in ids {
            req = req.header("x-ids-set", *v);
        }
        req.body(Body::empty()).unwrap()
    };
    let list = count_events(&router, get(&["1, 2"])).await;
    assert_eq!(only(&list), ("openapi.response_validation.passed", "body_checked"));
    // Repeated header lines join into one list.
    let lines = count_events(&router, get(&["1", "2"])).await;
    assert_eq!(only(&lines).0, "openapi.response_validation.passed");
    let bad = count_events(&router, get(&["1, x"])).await;
    assert_eq!(only(&bad), ("openapi.response_validation.failed", "header_mismatch"));
}
