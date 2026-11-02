//! Allocations and bytes allocated per request, by configuration (via `dhat`).
//! Peak heap isn't reported: dhat's peak is process-wide and can't be reset per run.
//!
//! `cargo bench --bench allocations` (add `--no-default-features` for validation off).

mod common;

use http_body_util::BodyExt;
use tower::ServiceExt;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

const REQUESTS: u64 = 2_000;

fn main() {
    let _profiler = dhat::Profiler::builder().testing().build();
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    println!("{:<8} {:<22} {:>12} {:>12}", "payload", "configuration", "allocs/req", "bytes/req");
    for (payload, path, body) in
        [("small", "/small", common::small_json()), ("nested", "/nested", common::nested_json())]
    {
        for (label, router) in common::configurations() {
            let run = |n: u64| {
                rt.block_on(async {
                    for _ in 0..n {
                        let res = router.clone().oneshot(common::post(path, &body)).await.unwrap();
                        assert!(res.status().is_success());
                        let _ = res.into_body().collect().await.unwrap().to_bytes();
                    }
                });
            };
            run(200); // warm up: compiled validators are cached after the first request
            let before = dhat::HeapStats::get();
            run(REQUESTS);
            let after = dhat::HeapStats::get();
            let allocs = (after.total_blocks - before.total_blocks) / REQUESTS;
            let bytes = (after.total_bytes - before.total_bytes) / REQUESTS;
            println!("{payload:<8} {label:<22} {allocs:>12} {bytes:>12}");
        }
    }
}
