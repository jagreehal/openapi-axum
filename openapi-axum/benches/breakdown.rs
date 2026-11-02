//! Where request validation's cost goes, stage by stage, for the nested payload.
//! Allocations are counted with dhat. Timings are left out: dhat slows down code that
//! allocates, so only the criterion `overhead` bench is a fair timing.
//!
//! `cargo bench -p openapi-axum --bench breakdown`

mod common;

use serde_json::Value;
use utoipa::{PartialSchema, ToSchema};

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn measure(label: &str, n: u64, mut f: impl FnMut()) {
    for _ in 0..50 {
        f();
    }
    let before = dhat::HeapStats::get();
    for _ in 0..n {
        f();
    }
    let after = dhat::HeapStats::get();
    let allocs = (after.total_blocks - before.total_blocks) / n;
    let bytes = (after.total_bytes - before.total_bytes) / n;
    println!("{label:<48} {allocs:>6} allocs {bytes:>7} B");
}

fn main() {
    let _profiler = dhat::Profiler::builder().testing().build();
    let body = common::nested_json();
    let bytes = body.as_bytes();

    let mut schemas = Vec::new();
    <common::Nested as ToSchema>::schemas(&mut schemas);
    let components: serde_json::Map<String, Value> =
        schemas.into_iter().map(|(n, s)| (n, serde_json::to_value(s).unwrap())).collect();
    let root = serde_json::json!({
        "allOf": [serde_json::to_value(<common::Nested as PartialSchema>::schema()).unwrap()],
        "components": { "schemas": components },
    });
    let validator = jsonschema::options().should_validate_formats(true).build(&root).unwrap();
    let value: Value = serde_json::from_slice(bytes).unwrap();
    let n = 5_000;

    measure("typed parse, from_slice::<Nested> (plain axum)", n, || {
        let _: common::Nested = serde_json::from_slice(bytes).unwrap();
    });
    measure("from_slice::<Value>", n, || {
        let _: Value = serde_json::from_slice(bytes).unwrap();
    });
    measure("from_slice::<Value> + from_value::<Nested>", n, || {
        let v: Value = serde_json::from_slice(bytes).unwrap();
        let _: common::Nested = serde_json::from_value(v).unwrap();
    });
    measure("is_valid(&value)", n, || assert!(validator.is_valid(&value)));
    measure("iter_errors(&value).collect(), valid input", n, || {
        assert!(validator.iter_errors(&value).next().is_none());
    });
}
