//! Per-request time: plain axum vs openapi-axum configurations, small and nested JSON.
//!
//! `cargo bench --bench overhead` (validation on) and
//! `cargo bench --bench overhead --no-default-features` (validation compiled out).

mod common;

use criterion::{Criterion, criterion_group, criterion_main};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn bench(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    for (payload, path, body) in
        [("small", "/small", common::small_json()), ("nested", "/nested", common::nested_json())]
    {
        let mut group = c.benchmark_group(payload);
        for (label, router) in common::configurations() {
            group.bench_function(label, |b| {
                b.to_async(&rt).iter(|| {
                    let router = router.clone();
                    let req = common::post(path, &body);
                    async move {
                        let res = router.oneshot(req).await.unwrap();
                        assert!(res.status().is_success());
                        res.into_body().collect().await.unwrap().to_bytes()
                    }
                });
            });
        }
        group.finish();
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
