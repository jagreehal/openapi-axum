//! `verify_responses` reports each outcome as a `tracing` event (target
//! `openapi_axum::verify`) with a stable `reason`, so you can tell how much traffic
//! had its body checked, not just how much failed.

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
