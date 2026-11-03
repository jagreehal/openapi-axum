# Benchmarks

What openapi-axum adds to a request, measured in-process with `tower::ServiceExt::oneshot` on one thread: no network and no server. The handler echoes its JSON body back, so the numbers isolate extraction, validation and response handling.

Payloads:
- **small**: `{"title": "buy milk", "done": false}`
- **nested**: an owner with tags, plus 20 items with labels (about 1 KB)

Both carry the same constraints in their schemas: titles of 1 to 200 characters, a non-empty owner name, at most 100 items.

Machine: Apple M1 Pro, rustc 1.97.1, release profile.

## Time per request

Criterion medians. The 95% confidence intervals are within 1% of each median.

| Configuration | small | nested |
|---|---|---|
| plain axum | 1.3 µs | 11.8 µs |
| plain axum + the same constraints checked by hand | 1.3 µs | 11.6 µs |
| openapi-axum, validation compiled out (docs only) | 1.4 µs | 11.8 µs |
| request validation (default) | 2.4 µs | 17.9 µs |
| request validation + `verify_responses` | 3.9 µs | 28.3 µs |

## Allocations per request

Counted with [dhat](https://docs.rs/dhat) over 2,000 requests, after warm-up.

| Configuration | small | nested |
|---|---|---|
| plain axum | 30 allocs, 3.4 KB | 246 allocs, 12.2 KB |
| plain axum + the same constraints checked by hand | 30 allocs, 3.4 KB | 246 allocs, 12.2 KB |
| openapi-axum, validation compiled out (docs only) | 30 allocs, 3.9 KB | 246 allocs, 12.7 KB |
| request validation (default) | 58 allocs, 7.2 KB | 380 allocs, 31.0 KB |
| request validation + `verify_responses` | 94 allocs, 10.3 KB | 587 allocs, 52.3 KB |

## Where request validation's cost goes

The fair comparison is with the hand-checked row: checking these constraints in Rust after typed deserialization costs nothing measurable. Request validation costs more because it validates a generic JSON tree rather than your type.

Each stage for the nested payload: time from criterion under the normal allocator (`cargo bench --bench stages`), allocations from dhat (`cargo bench --bench breakdown`):

| Stage | time | allocs | bytes |
|---|---|---|---|
| typed parse into the struct (what plain axum does) | 4.3 µs | 88 | 5.5 KB |
| parse into `serde_json::Value` | 7.2 µs | 175 | 19.0 KB |
| run the compiled validator on the `Value` | 1.4 µs | 0 | 0 |
| convert the `Value` into the struct | 2.4 µs | 22 | 2.1 KB |

Validation's path (parse into `Value`, validate, convert) takes 11.0 µs against 4.3 µs for the typed parse: 6.7 µs more, in line with the 6.1 µs measured end to end. Of that, the `Value` round trip takes 5.3 µs (79%) and the schema check 1.4 µs (21%). The validator allocates nothing for valid input. It's compiled once, when `split()` builds the router, and reused for every request.

Removing the `Value` would mean validating a borrowed, zero-copy view of the request bytes, which jsonschema supports through its `Json` trait but openapi-axum doesn't implement yet. Validating your typed value itself would skip the tree, but the checks would then come from somewhere other than the published schema, and the published schema is the one source openapi-axum validates against.

## Reading the numbers

- With validation compiled out, a route is your handler and nothing else: openapi-axum adds no layer, extension or allocation per request. The 0.5 KB is a larger allocation axum already makes, since `#[route]` wraps your handler to route extractor rejections into your error type. The `tracing` feature adds a span per handler, which costs nothing measurable here with no subscriber installed.
- `verify_responses` buffers and validates each response body within its limit. Use it in tests and CI. In production, `verify_sample_every(n)` checks one body in every `n` and `VerifyMode::Log` records failures without failing requests.
- These are microseconds of CPU per request. Network, database and handler work tend to outweigh them, but measure your own payloads.

## Not measured here

- p95/p99 latency and throughput need a load generator (such as `oha` or `wrk`) against a running server.
- The extra allocations' effect under concurrent load (allocator contention, memory pressure) hasn't been measured.
- Peak heap isn't reported, because dhat's peak is process-wide and can't be reset per configuration.

## Run them

```bash
cargo bench -p openapi-axum --bench overhead                         # validation on
cargo bench -p openapi-axum --bench overhead --no-default-features   # validation compiled out
cargo bench -p openapi-axum --bench allocations                      # add --no-default-features likewise
cargo bench -p openapi-axum --bench stages                           # stage-by-stage time
cargo bench -p openapi-axum --bench breakdown                        # stage-by-stage allocations
```
