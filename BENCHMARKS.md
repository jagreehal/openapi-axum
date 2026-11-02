# Benchmarks

What openapi-axum adds to a request, measured in-process with `tower::ServiceExt::oneshot` on one thread: no network and no server. The handler echoes its JSON body back, so the numbers isolate extraction, validation and response handling.

Payloads:
- **small**: `{"title": "buy milk", "done": false}`
- **nested**: an owner with tags, plus 20 items with labels (about 1 KB)

Machine: Apple M1 Pro, rustc 1.97.1, release profile.

## Time per request

Criterion medians. The 95% confidence intervals are within 1% of each median.

| Configuration | small | nested |
|---|---|---|
| plain axum | 1.3 µs | 11.5 µs |
| openapi-axum, validation compiled out | 1.9 µs | 12.2 µs |
| request validation (default) | 2.5 µs | 18.3 µs |
| request validation + `verify_responses` | 4.0 µs | 29.3 µs |

## Allocations per request

Counted with [dhat](https://docs.rs/dhat) over 2,000 requests, after warm-up.

| Configuration | small | nested |
|---|---|---|
| plain axum | 30 allocs, 3.4 KB | 246 allocs, 12.2 KB |
| openapi-axum, validation compiled out | 46 allocs, 5.6 KB | 262 allocs, 14.4 KB |
| request validation (default) | 61 allocs, 7.3 KB | 383 allocs, 31.0 KB |
| request validation + `verify_responses` | 100 allocs, 10.4 KB | 593 allocs, 52.5 KB |

## Reading the numbers

- With validation compiled out, the remaining cost is fixed per request: the route id, the response stamp and the handler span.
- Request validation parses a JSON body into `serde_json::Value`, validates it, then converts it into your type. That's the extra cost, and it grows with the body.
- `verify_responses` buffers and validates each response body within its limit. Use it in tests and CI, and sample with `VerifyMode::Log` in production if the cost matters.
- These are microseconds of CPU per request. Network, database and handler work usually dominate, but measure your own payloads.

## Not measured here

p95/p99 latency and throughput need a load generator (such as `oha` or `wrk`) against a running server. Peak heap isn't reported because dhat's peak is process-wide and can't be reset per configuration.

## Run them

```bash
cargo bench -p openapi-axum --bench overhead                         # validation on
cargo bench -p openapi-axum --bench overhead --no-default-features   # validation compiled out
cargo bench -p openapi-axum --bench allocations                      # add --no-default-features likewise
```
