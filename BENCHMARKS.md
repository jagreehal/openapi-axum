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
| openapi-axum, validation compiled out | 1.9 µs | 12.7 µs |
| request validation (default) | 2.4 µs | 17.9 µs |
| request validation + `verify_responses` | 3.9 µs | 28.3 µs |

## Allocations per request

Counted with [dhat](https://docs.rs/dhat) over 2,000 requests, after warm-up.

| Configuration | small | nested |
|---|---|---|
| plain axum | 30 allocs, 3.4 KB | 246 allocs, 12.2 KB |
| plain axum + the same constraints checked by hand | 30 allocs, 3.4 KB | 246 allocs, 12.2 KB |
| openapi-axum, validation compiled out | 46 allocs, 5.6 KB | 262 allocs, 14.4 KB |
| request validation (default) | 58 allocs, 7.2 KB | 380 allocs, 31.0 KB |
| request validation + `verify_responses` | 94 allocs, 10.3 KB | 587 allocs, 52.3 KB |

## Where request validation's cost goes

The fair comparison is with the hand-checked row: checking these constraints in Rust after typed deserialization costs nothing measurable. Request validation costs more because it validates a generic JSON tree rather than your type. For the nested payload (`cargo bench --bench breakdown`):

| Stage | allocs | bytes |
|---|---|---|
| typed parse into the struct (what plain axum does) | 88 | 5.5 KB |
| parse into `serde_json::Value` | 175 | 19.0 KB |
| `Value`, then convert into the struct (what validation does) | 197 | 21.1 KB |
| run the compiled validator on the `Value` | 0 | 0 |

So the intermediate `serde_json::Value` accounts for most of the extra memory and time. The validator itself allocates nothing for valid input. It's compiled once, when `split()` builds the router, and reused for every request.

Removing the `Value` would mean validating a borrowed, zero-copy view of the request bytes, which jsonschema supports through its `Json` trait but openapi-axum doesn't implement yet. Validating your typed value itself would skip the tree, but the checks would then come from somewhere other than the published schema, and the published schema is the one source openapi-axum validates against.

## Reading the numbers

- With validation compiled out, the remaining cost is fixed per request: the route id, the response stamp and the handler span.
- `verify_responses` buffers and validates each response body within its limit. Use it in tests and CI, and sample with `VerifyMode::Log` in production if the cost matters.
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
cargo bench -p openapi-axum --bench breakdown                        # stage-by-stage allocations
```
