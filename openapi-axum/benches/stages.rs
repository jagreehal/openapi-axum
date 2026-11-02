//! Time per stage of request validation, for the nested payload, under the normal
//! allocator (criterion). Pairs with `breakdown`, which counts the same stages'
//! allocations with dhat.
//!
//! `cargo bench -p openapi-axum --bench stages`

mod common;

use criterion::{Criterion, criterion_group, criterion_main};
use serde_json::Value;
use std::hint::black_box;
use utoipa::{PartialSchema, ToSchema};

fn bench(c: &mut Criterion) {
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

    let mut g = c.benchmark_group("stages");
    g.bench_function("1 typed parse (plain axum)", |b| {
        b.iter(|| serde_json::from_slice::<common::Nested>(black_box(bytes)).unwrap())
    });
    g.bench_function("2 parse into Value", |b| b.iter(|| serde_json::from_slice::<Value>(black_box(bytes)).unwrap()));
    g.bench_function("3 validate Value", |b| b.iter(|| assert!(validator.is_valid(black_box(&value)))));
    g.bench_function("4 Value into typed", |b| {
        b.iter_batched(
            || value.clone(),
            |v| serde_json::from_value::<common::Nested>(v).unwrap(),
            criterion::BatchSize::SmallInput,
        )
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
