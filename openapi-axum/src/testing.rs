//! Check a router against its own published document (feature `testing`).
//!
//! [`assert_conforms`] builds a request for every documented operation **using only the
//! document** (path/query/header/cookie params, request body), sends it to the real
//! router, and checks the response against the document. This is how hand-written
//! [`ExtractorDoc`](crate::ExtractorDoc) impls and middleware get verified rather than
//! trusted: if an extractor needs a header its docs don't mention, or a layer returns
//! an undocumented status, the docs-only request exposes it.
//!
//! ```
//! # async fn run() {
//! # use openapi_axum::{ApiRouter, route, routes};
//! # #[route(GET, "/health")] async fn health() -> &'static str { "ok" }
//! let (router, openapi) = ApiRouter::new().routes(routes![health]).split();
//! let report = openapi_axum::testing::assert_conforms(&router, &openapi).await;
//! assert_eq!(report.len(), 1);
//! # }
//! ```

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Map, Value, json};
use tower::ServiceExt;
use utoipa::openapi::OpenApi;

use crate::spec_index::{ArrayStyle, SpecIndex, array_style};

fn resolve<'a>(spec: &'a Value, schema: &'a Value) -> &'a Value {
    match schema.get("$ref").and_then(Value::as_str).and_then(|r| spec.pointer(r.trim_start_matches('#'))) {
        Some(target) => resolve(spec, target),
        None => schema,
    }
}

/// Does `value` satisfy `schema` (resolving refs against `spec`)?
fn valid(spec: &Value, schema: &Value, value: &Value) -> bool {
    crate::spec_index::compile(spec, schema).is_valid(value)
}

/// A minimal value satisfying `schema`: the schema's own `example` if it has one,
/// otherwise generated (required fields only, bounds, formats, enums respected).
/// Always schema-valid when the schema can be satisfied by the generator.
pub fn example(spec: &Value, schema: &Value) -> Value {
    example_at(spec, schema, 0)
}

fn example_at(spec: &Value, schema: &Value, depth: usize) -> Value {
    candidates_at(spec, schema, 1, depth).into_iter().next().unwrap_or(Value::Null)
}

/// Past this nesting, generation stops: recursive schemas (`Node { next: Option<Node> }`)
/// get their optional fields left out, and a required cycle gives up (`null`).
const MAX_DEPTH: usize = 8;

/// Up to `n` **distinct, schema-valid** values for `schema`, most natural first.
/// Fewer are returned when the schema doesn't allow (or the generator can't find) more.
pub fn candidates(spec: &Value, schema: &Value, n: usize) -> Vec<Value> {
    candidates_at(spec, schema, n, 0)
}

fn candidates_at(spec: &Value, schema: &Value, n: usize, depth: usize) -> Vec<Value> {
    if depth > MAX_DEPTH {
        return Vec::new();
    }
    let mut out: Vec<Value> = Vec::new();
    for c in raw_candidates(spec, schema, n, depth) {
        if out.len() == n {
            break;
        }
        if !out.contains(&c) && valid(spec, schema, &c) {
            out.push(c);
        }
    }
    out
}

/// Unfiltered guesses, in preference order: documented examples, then generated.
fn raw_candidates(spec: &Value, schema: &Value, n: usize, depth: usize) -> Vec<Value> {
    let schema = resolve(spec, schema);
    // Variants multiply with nesting: below a few levels, one natural value is enough.
    let n = if depth >= 3 { 1 } else { n };
    let next = depth + 1;
    let mut out: Vec<Value> = Vec::new();
    // Documented examples first: the author knows what a valid value looks like.
    out.extend(schema.get("example").cloned());
    out.extend(schema.get("examples").and_then(Value::as_array).into_iter().flatten().cloned());
    if let Some(c) = schema.get("const") {
        out.push(c.clone());
        return out;
    }
    if let Some(Value::Array(values)) = schema.get("enum") {
        out.extend(values.iter().cloned());
        return out;
    }
    // allOf: every part applies (`#[serde(flatten)]`), so merge their examples.
    if let Some(Value::Array(parts)) = schema.get("allOf") {
        let mut merged = Map::new();
        for part in parts {
            match example_at(spec, part, next) {
                Value::Object(fields) => merged.extend(fields),
                other if parts.len() == 1 => return vec![other],
                _ => {}
            }
        }
        out.push(Value::Object(merged));
        return out;
    }
    // oneOf/anyOf: any option will do; non-null options first.
    for key in ["oneOf", "anyOf"] {
        if let Some(Value::Array(options)) = schema.get(key) {
            let (non_null, null): (Vec<&Value>, Vec<&Value>) =
                options.iter().partition(|o| resolve(spec, o).get("type") != Some(&json!("null")));
            for option in non_null.into_iter().chain(null) {
                out.extend(candidates_at(spec, option, n, next));
            }
            return out;
        }
    }
    let ty = match schema.get("type") {
        Some(Value::String(t)) => t.as_str(),
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).find(|t| *t != "null").unwrap_or("null"),
        _ if schema.get("properties").is_some() => "object",
        _ => "null",
    };
    // A few more guesses than needed, since some may fail other constraints.
    let tries = n + 8;
    match ty {
        "object" => {
            let required: Vec<&str> =
                schema["required"].as_array().map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let props = schema["properties"].as_object().cloned().unwrap_or_default();
            // First the minimal object (required fields, natural values). Then variants:
            // the k-th candidate of each field, with optional fields on odd k.
            // Optional fields only appear in variants (n > 1), so the minimal object never
            // generates them: a recursive optional field can't recurse.
            let options: Vec<(String, bool, Vec<Value>)> = props
                .iter()
                .filter_map(|(k, s)| {
                    let req = required.contains(&k.as_str());
                    if !req && n == 1 {
                        return None;
                    }
                    let mut c: Vec<Value> = candidates_at(spec, s, 1, next);
                    if n > 1 {
                        c.extend(candidates_at(spec, s, n, next));
                    }
                    // Nothing satisfies it: leave it out (a required one fails validation).
                    (!c.is_empty()).then(|| (k.clone(), req, c))
                })
                .collect();
            for k in 0..n.max(1) {
                let fields: Map<String, Value> = options
                    .iter()
                    .filter(|(_, req, _)| *req || k % 2 == 1)
                    .map(|(name, _, c)| (name.clone(), c[k % c.len()].clone()))
                    .collect();
                out.push(Value::Object(fields));
            }
        }
        // The smallest *non-empty* array allowed: an empty array is valid, but a
        // required array query param serialized with repeated keys would vanish.
        "array" => {
            let min = schema.get("minItems").and_then(Value::as_u64).unwrap_or(0) as usize;
            let max = schema.get("maxItems").and_then(Value::as_u64).map_or(usize::MAX, |m| m as usize);
            let len = min.max(1).min(max);
            let items = &schema["items"];
            if schema.get("uniqueItems").and_then(Value::as_bool).unwrap_or(false) {
                // Distinct items that each satisfy the item schema.
                let distinct = candidates_at(spec, items, len, next);
                if distinct.len() >= min.max(1).min(max) {
                    out.push(Value::Array(distinct));
                }
            } else {
                out.push(Value::Array(vec![example_at(spec, items, next); len]));
            }
            if min == 0 {
                out.push(json!([]));
            }
        }
        "string" => {
            let min = schema.get("minLength").and_then(Value::as_u64).unwrap_or(0) as usize;
            let max = schema.get("maxLength").and_then(Value::as_u64).map_or(usize::MAX, |m| m as usize);
            let len = min.max(1).min(max);
            if n > 1 && schema.get("format").is_none() && max < 10_000 && max > len {
                out.push(json!("z".repeat(max)));
            }
            for i in 0..tries {
                out.push(match schema.get("format").and_then(Value::as_str) {
                    Some("uuid") => json!(format!("00000000-0000-4000-8000-{:012x}", i + 1)),
                    Some("date-time") => json!(format!("2026-01-01T00:00:{:02}Z", i % 60)),
                    Some("date") => json!(format!("2026-01-{:02}", i % 28 + 1)),
                    Some("email") => json!(format!("a{i}@example.com")),
                    _ => {
                        let letter = char::from(b'a' + (i % 26) as u8);
                        json!(std::iter::repeat_n(letter, len).collect::<String>())
                    }
                });
            }
        }
        "integer" | "number" => {
            let bound = |key: &str| schema.get(key).and_then(Value::as_f64);
            let step = bound("multipleOf").unwrap_or(1.0);
            let lo = match (bound("minimum"), bound("exclusiveMinimum")) {
                (Some(m), _) => m,
                (None, Some(m)) => m + step,
                _ => match bound("maximum") {
                    Some(max) if max < 1.0 => max,
                    _ => 1.0,
                },
            };
            let start = (lo / step).ceil() * step;
            for i in 0..tries {
                let v = start + step * i as f64;
                out.push(if ty == "integer" { json!(v as i64) } else { json!(v) });
            }
            // Also try downward from a maximum, for ranges below the default start.
            if let Some(max) = bound("maximum") {
                for i in 0..tries {
                    let v = (max / step).floor() * step - step * i as f64;
                    out.push(if ty == "integer" { json!(v as i64) } else { json!(v) });
                }
            }
        }
        "boolean" => out.extend([json!(true), json!(false)]),
        _ => out.push(Value::Null),
    }
    out
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Every documented `(path, method, operation)`.
pub fn operations(spec: &Value) -> Vec<(String, Method, Value)> {
    let mut ops = Vec::new();
    for (path, item) in spec["paths"].as_object().into_iter().flatten() {
        for (m, op) in item.as_object().into_iter().flatten() {
            if let Ok(method) = m.to_uppercase().parse::<Method>() {
                ops.push((path.clone(), method, op.clone()));
            }
        }
    }
    ops
}

/// One input an operation documents: a parameter, or the request body.
struct Input {
    /// `path`, `query`, `header`, `cookie`, or `body`.
    location: String,
    /// The parameter name, or the body's content type.
    name: String,
    required: bool,
    schema: Value,
    example: Option<Value>,
    style: ArrayStyle,
}

fn inputs(spec: &Value, op: &Value) -> Vec<Input> {
    let mut out = Vec::new();
    for p in op["parameters"].as_array().into_iter().flatten() {
        let p = resolve(spec, p);
        let location = p["in"].as_str().unwrap_or_default().to_owned();
        out.push(Input {
            required: location == "path" || p["required"].as_bool().unwrap_or(false),
            name: p["name"].as_str().unwrap_or_default().to_owned(),
            location,
            schema: p["schema"].clone(),
            example: p.get("example").cloned(),
            style: array_style(p),
        });
    }
    // `requestBody` may be a `$ref` to components.requestBodies.
    let content =
        resolve(spec, &op["requestBody"]).get("content").and_then(Value::as_object).and_then(|c| c.iter().next());
    if let Some((ct, media)) = content {
        out.push(Input {
            location: "body".into(),
            name: ct.clone(),
            required: true,
            schema: media["schema"].clone(),
            example: media.get("example").cloned(),
            style: ArrayStyle::Repeated,
        });
    }
    out
}

impl Input {
    /// The documented example if it's valid, else a generated value.
    fn default_value(&self, spec: &Value, at: &str) -> Value {
        let value = self
            .example
            .clone()
            .filter(|e| valid(spec, &self.schema, e))
            .unwrap_or_else(|| example(spec, &self.schema));
        let what = if self.location == "body" {
            format!("`{}` body", self.name)
        } else {
            format!("{} parameter `{}`", self.location, self.name)
        };
        assert!(
            valid(spec, &self.schema, &value),
            "{at}: couldn't generate a valid value for {what} (got {value}); add an `example` to its schema"
        );
        value
    }

    /// `query parameter `limit`` or `body`.
    fn describe(&self) -> String {
        if self.location == "body" { "body".into() } else { format!("{} parameter `{}`", self.location, self.name) }
    }

    /// Can `value` be sent in this input's place and read back as the same value?
    fn representable(&self, value: &Value) -> bool {
        let scalar_ok = |v: &Value| match v {
            Value::String(s) => !(s.is_empty() && self.location == "path"),
            Value::Number(_) | Value::Bool(_) => true,
            _ => false,
        };
        match (self.location.as_str(), value) {
            ("body", _) => true,
            // An empty array vanishes from a query string; a path segment can't be empty.
            (_, Value::Array(items)) => !items.is_empty() && items.iter().all(scalar_ok),
            _ => scalar_ok(value),
        }
    }
}

/// A request for `inputs`, with `values[i]` for input `i` (`None`: leave it out).
fn request_with(path: &str, method: &Method, inputs: &[Input], values: &[Option<Value>]) -> Request<Body> {
    let mut uri = path.to_owned();
    let mut query = Vec::new();
    let mut cookies = Vec::new();
    let mut req = Request::builder().method(method.clone());
    let mut body = Body::empty();
    for (input, value) in inputs.iter().zip(values) {
        let Some(value) = value else { continue };
        let name = input.name.as_str();
        if input.location == "body" {
            req = req.header("content-type", name);
            body = if name.starts_with("application/x-www-form-urlencoded") {
                let pairs: Vec<(String, String)> =
                    value.as_object().into_iter().flatten().map(|(k, v)| (k.clone(), scalar(v))).collect();
                Body::from(serde_urlencoded::to_string(pairs).expect("form encodes"))
            } else {
                Body::from(value.to_string())
            };
            continue;
        }
        // Serialize the way the parameter declares (style/explode), the same rule
        // the runtime uses to read it back.
        let items: Vec<String> = match value {
            Value::Array(items) => items.iter().map(scalar).collect(),
            other => vec![scalar(other)],
        };
        let values: Vec<String> = match (value.is_array(), input.style) {
            (true, ArrayStyle::Delimited(sep)) => vec![items.join(&sep.to_string())],
            _ => items,
        };
        match input.location.as_str() {
            "path" => {
                let encoded: Vec<String> = values.iter().map(|v| percent_encode(v)).collect();
                uri = uri.replace(&format!("{{{name}}}"), &encoded.join(","));
            }
            "query" => query.extend(values.into_iter().map(|v| (name.to_owned(), v))),
            "header" => req = req.header(name, values.join(",")),
            "cookie" => cookies.extend(values.into_iter().map(|v| format!("{name}={v}"))),
            _ => {}
        }
    }
    assert!(!uri.contains('{'), "{method} {path}: a path capture isn't documented as a parameter");
    if !query.is_empty() {
        uri = format!("{uri}?{}", serde_urlencoded::to_string(&query).expect("query encodes"));
    }
    if !cookies.is_empty() {
        req = req.header("cookie", cookies.join("; "));
    }
    req.uri(uri).body(body).expect("valid request")
}

/// Percent-encode everything but RFC 3986 unreserved characters, for a path segment.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The values the default request sends: every required input, nothing optional.
fn default_values(spec: &Value, at: &str, inputs: &[Input]) -> Vec<Option<Value>> {
    inputs.iter().map(|i| i.required.then(|| i.default_value(spec, at))).collect()
}

/// A request built only from what the document says this operation needs.
///
/// # Panics
/// If the path has a capture that isn't documented as a parameter.
pub fn request_from_spec(spec: &Value, path: &str, method: &Method, op: &Value) -> Request<Body> {
    let inputs = inputs(spec, op);
    let values = default_values(spec, &format!("{method} {path}"), &inputs);
    request_with(path, method, &inputs, &values)
}

/// Up to `n` distinct valid value sets, after the default one: the k-th candidate of
/// each input, with optional inputs sent on odd k.
fn valid_cases(spec: &Value, inputs: &[Input], defaults: &[Option<Value>], n: usize) -> Vec<Vec<Option<Value>>> {
    let options: Vec<Vec<Value>> = inputs
        .iter()
        .zip(defaults)
        .map(|(input, default)| {
            let mut c: Vec<Value> = default.iter().cloned().collect();
            for v in candidates(spec, &input.schema, n) {
                if input.representable(&v) && !c.contains(&v) {
                    c.push(v);
                }
            }
            c
        })
        .collect();
    let mut out: Vec<Vec<Option<Value>>> = vec![defaults.to_vec()];
    for k in 1..n {
        let case: Vec<Option<Value>> = inputs
            .iter()
            .zip(&options)
            .map(|(input, c)| (!c.is_empty() && (input.required || k % 2 == 1)).then(|| c[k % c.len()].clone()))
            .collect();
        if !out.contains(&case) {
            out.push(case);
        }
    }
    out.remove(0);
    out
}

fn wrong_number(schema: &Value, key: &str, delta: f64) -> Option<Value> {
    let m = schema.get(key)?.as_f64()?;
    let v = m + delta;
    Some(if schema["type"] == "integer" && v.fract() == 0.0 { json!(v as i64) } else { json!(v) })
}

/// Values that break `schema` in one way each, labelled (`below minimum`, `missing
/// required "title"`). Every one is confirmed invalid against the schema.
/// `wire`: the value travels as text (a parameter), so only same-type breaks count.
pub fn invalid_values(spec: &Value, schema: &Value, wire: bool) -> Vec<(String, Value)> {
    let mut out = raw_invalid(spec, schema, wire, 0);
    let mut seen = Vec::new();
    out.retain(|(_, v)| {
        !seen.contains(v) && !valid(spec, schema, v) && {
            seen.push(v.clone());
            true
        }
    });
    out
}

fn raw_invalid(spec: &Value, schema: &Value, wire: bool, depth: usize) -> Vec<(String, Value)> {
    let s = resolve(spec, schema);
    let mut out: Vec<(String, Value)> = Vec::new();
    let mut push = |label: &str, v: Value| out.push((label.to_owned(), v));
    let ty = match s.get("type") {
        Some(Value::String(t)) => t.clone(),
        _ if s.get("properties").is_some() || s.get("allOf").is_some() => "object".into(),
        _ => String::new(),
    };
    let step = if ty == "integer" { 1.0 } else { 0.5 };
    for (key, delta, label) in [
        ("minimum", -step, "below minimum"),
        ("exclusiveMinimum", 0.0, "at exclusiveMinimum"),
        ("maximum", step, "above maximum"),
        ("exclusiveMaximum", 0.0, "at exclusiveMaximum"),
    ] {
        if let Some(v) = wrong_number(s, key, delta) {
            push(label, v);
        }
    }
    if let Some(k) = s.get("multipleOf").and_then(Value::as_f64) {
        push("not a multipleOf", json!(k * 1.5));
    }
    if let Some(n) = s.get("minLength").and_then(Value::as_u64).filter(|n| *n > 0) {
        push("shorter than minLength", json!("a".repeat(n as usize - 1)));
    }
    if let Some(n) = s.get("maxLength").and_then(Value::as_u64).filter(|n| *n < 100_000) {
        push("longer than maxLength", json!("a".repeat(n as usize + 1)));
    }
    if let Some(f) = s.get("format").and_then(Value::as_str) {
        push(&format!("not a valid {f}"), json!("not-valid"));
    }
    if s.get("pattern").is_some() {
        push("not matching pattern", json!("\u{1}!"));
    }
    if s.get("enum").is_some() || s.get("const").is_some() {
        push("not an allowed value", json!("__not_allowed__"));
        push("not an allowed value", json!(987_654_321));
    }
    if ty == "array" {
        let item = example(spec, &s["items"]);
        if let Some(n) = s.get("minItems").and_then(Value::as_u64).filter(|n| *n > 0) {
            push("fewer than minItems", Value::Array(vec![item.clone(); n as usize - 1]));
        }
        if let Some(n) = s.get("maxItems").and_then(Value::as_u64).filter(|n| *n < 1000) {
            push("more than maxItems", Value::Array(vec![item.clone(); n as usize + 1]));
        }
        if s["uniqueItems"] == true {
            push("duplicate items", json!([item.clone(), item]));
        }
        if depth < 3 {
            for (label, v) in raw_invalid(spec, &s["items"], wire, depth + 1).into_iter().take(4) {
                out.push((format!("item: {label}"), json!([v])));
            }
        }
    }
    if ty == "object" && !wire {
        // `allOf` (`#[serde(flatten)]`): every part's fields and requirements apply.
        let parts: Vec<&Value> = match s.get("allOf").and_then(Value::as_array) {
            Some(parts) => parts.iter().map(|p| resolve(spec, p)).collect(),
            None => vec![s],
        };
        let base = match example(spec, s) {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        for part in parts {
            for r in part["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                let mut m = base.clone();
                m.remove(r);
                out.push((format!("missing required `{r}`"), Value::Object(m)));
            }
            for (name, prop) in part["properties"].as_object().into_iter().flatten() {
                if depth >= 3 {
                    break;
                }
                for (label, v) in raw_invalid(spec, prop, false, depth + 1).into_iter().take(6) {
                    let mut m = base.clone();
                    m.insert(name.clone(), v);
                    out.push((format!("`{name}`: {label}"), Value::Object(m)));
                }
            }
            if part["additionalProperties"] == false {
                let mut m = base.clone();
                m.insert("__unexpected".into(), json!(1));
                out.push(("unexpected field".into(), Value::Object(m)));
            }
        }
    }
    // Wrong types last: the keyword breaks above say more about the constraints.
    if wire {
        if matches!(ty.as_str(), "integer" | "number" | "boolean") {
            let article = if ty == "integer" { "an" } else { "a" };
            out.push((format!("not {article} {ty}"), json!("x")));
        }
    } else {
        if ty == "integer" {
            out.push(("not an integer".into(), json!(1.5)));
        }
        let probes = [
            ("null", "null", Value::Null),
            ("boolean", "a boolean", json!(true)),
            ("number", "a number", json!(12345)),
            ("string", "a string", json!("x")),
            ("array", "an array", json!([])),
            ("object", "an object", json!({})),
        ];
        let same = |t: &str| t == ty || (t == "number" && ty == "integer");
        for (t, label, v) in probes {
            if !same(t) {
                out.push((format!("wrong type: {label}"), v));
            }
        }
    }
    out
}

/// Send `req` to `router` and check the response against the document.
///
/// # Panics
/// On an undocumented status, content type, header value or body shape.
pub async fn send_checked(router: &Router, openapi: &OpenApi, path: &str, req: Request<Body>) -> (StatusCode, Value) {
    let index = SpecIndex::new(openapi);
    send_with(&index, router, path, req).await.unwrap_or_else(|e| panic!("{e}"))
}

/// Send `req` and check the response: status, content type, documented headers, body.
async fn send_with(
    index: &SpecIndex,
    router: &Router,
    path: &str,
    req: Request<Body>,
) -> Result<(StatusCode, Value), String> {
    let method = req.method().clone();
    let res = router.clone().oneshot(req).await.expect("router is infallible");
    let status = res.status();
    let content_type = res.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.expect("body collects").to_bytes();
    // A HEAD response describes the GET response without its body: check the rest.
    let body = (method != Method::HEAD).then_some(&bytes[..]);
    let checked = index
        .check_headers(&method, path, status, &headers, false)
        .and_then(|()| index.check_response_with(&method, path, status, content_type.as_deref(), body, false))
        .map_err(|v| v.message);
    if let Err(violation) = checked {
        return Err(format!("{violation}\n  body: {}", String::from_utf8_lossy(&bytes)));
    }
    let body = serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
    Ok((status, body))
}

/// One documented security scheme's credential placement.
enum Placement {
    Header(String, String),
    Query(String, String),
    Cookie(String, String),
}

/// Where a scheme's credential goes, per its definition in `components.securitySchemes`.
fn placement(spec: &Value, scheme: &str, credential: Option<&str>) -> Option<Placement> {
    let def = &spec["components"]["securitySchemes"][scheme];
    let value = |dummy: &str| credential.unwrap_or(dummy).to_owned();
    match def["type"].as_str()? {
        "apiKey" => {
            let name = def["name"].as_str()?.to_owned();
            match def["in"].as_str()? {
                "header" => Some(Placement::Header(name, value("a"))),
                "query" => Some(Placement::Query(name, value("a"))),
                "cookie" => Some(Placement::Cookie(name, value("a"))),
                _ => None,
            }
        }
        "http" if def["scheme"].as_str()?.eq_ignore_ascii_case("basic") => {
            Some(Placement::Header("authorization".into(), format!("Basic {}", value("YTph"))))
        }
        // http bearer, oauth2, openIdConnect: a bearer token.
        _ => Some(Placement::Header("authorization".into(), format!("Bearer {}", value("a")))),
    }
}

/// Checks a router against its own published document. Build with [`Conformance::new`],
/// or use [`assert_conforms`] for the defaults.
///
/// For every documented operation it sends a request built **only from the document**:
/// documented params and body, plus credentials for the operation's documented security
/// requirement (placed where the scheme says). It then requires:
/// - not rejected as malformed (400/415/422): the docs describe a valid request,
/// - not rejected as unauthenticated (401/403): the docs describe how to authenticate,
/// - the response matches the document (status, content type, header values, body).
///
/// Then it generates more requests from the schemas ([`cases`](Self::cases)):
/// - **valid variants** (other values, boundary values, optional params and fields
///   included) must pass the same rules,
/// - **invalid variants** (one path/query param or JSON body broken at a time: out of
///   range, too long, wrong type, missing required field, ...) must be rejected with
///   400/415/422 (or 404 for a path param), and that rejection must match the document.
///
/// Every case is deterministic, so a failure reproduces on every run. [`run`](Self::run)
/// returns a [`Report`] of what each operation exercised and skipped.
///
/// Credentials can only be supplied for *documented* schemes, so an extractor that needs
/// an undocumented header can't be made to pass.
pub struct Conformance<'a> {
    router: &'a Router,
    openapi: &'a OpenApi,
    credentials: Vec<(String, String)>,
    expected: Vec<(String, StatusCode)>,
    cases: usize,
    fixtures: Vec<(Option<String>, String, Value)>,
    bodies: Vec<(String, Value)>,
    before_each: Option<Hook<'a>>,
}

type Hook<'a> =
    Box<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> + Send + Sync + 'a>;

impl<'a> Conformance<'a> {
    pub fn new(router: &'a Router, openapi: &'a OpenApi) -> Self {
        Self {
            router,
            openapi,
            credentials: Vec::new(),
            expected: Vec::new(),
            cases: 8,
            fixtures: Vec::new(),
            bodies: Vec::new(),
            before_each: None,
        }
    }

    /// A real credential for a documented security scheme (e.g. a test token for
    /// `"bearer"`). Without one, a placeholder is sent, which works for extractors that
    /// only check presence.
    #[must_use]
    pub fn credential(mut self, scheme: &str, value: &str) -> Self {
        self.credentials.push((scheme.to_owned(), value.to_owned()));
        self
    }

    /// Use `value` for every parameter named `name` (path, query, header or cookie),
    /// such as an id that exists in your test data. Valid variants keep it; invalid
    /// variants still try values that break its schema.
    ///
    /// # Panics
    /// In [`run`](Self::run), if `value` doesn't match the parameter's schema.
    #[must_use]
    pub fn fixture(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.fixtures.push((None, name.to_owned(), value.into()));
        self
    }

    /// [`fixture`](Self::fixture) for one operation (`"GET /todos/{id}"`). Takes
    /// precedence over a fixture for every operation.
    #[must_use]
    pub fn fixture_for(mut self, operation: &str, name: &str, value: impl Into<Value>) -> Self {
        self.fixtures.push((Some(operation.to_owned()), name.to_owned(), value.into()));
        self
    }

    /// The request body for the docs-built request of `operation`, in place of a
    /// generated one.
    ///
    /// # Panics
    /// In [`run`](Self::run), if `value` doesn't match the body schema.
    #[must_use]
    pub fn body_for(mut self, operation: &str, value: impl Into<Value>) -> Self {
        self.bodies.push((operation.to_owned(), value.into()));
        self
    }

    /// Run `hook` before every request, to reset or seed state so each request sees the
    /// same data.
    #[must_use]
    pub fn before_each<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'a,
        Fut: std::future::Future<Output = ()> + Send + 'a,
    {
        self.before_each = Some(Box::new(move || Box::pin(hook())));
        self
    }

    /// Require operation `"GET /path"` to answer the docs-built request with `status`
    /// exactly (e.g. an endpoint that is *meant* to be unauthenticated-hostile). Its
    /// generated cases are skipped.
    #[must_use]
    pub fn expect(mut self, operation: &str, status: StatusCode) -> Self {
        self.expected.push((operation.to_owned(), status));
        self
    }

    /// Generated requests per operation: up to `n` valid variants, and up to `n`
    /// invalid values per input. Default 8; `0` sends only the docs-built request.
    #[must_use]
    pub fn cases(mut self, n: usize) -> Self {
        self.cases = n;
        self
    }

    /// Add the credentials the operation's first documented requirement needs.
    fn authorize(&self, spec: &Value, op: &Value, key: &str, req: &mut Request<Body>) -> Vec<String> {
        let schemes: Vec<String> =
            op["security"].get(0).and_then(Value::as_object).map(|r| r.keys().cloned().collect()).unwrap_or_default();
        let mut cookies = Vec::new();
        for scheme in &schemes {
            let credential = self.credentials.iter().find(|(s, _)| s == scheme).map(|(_, v)| v.as_str());
            match placement(spec, scheme, credential) {
                Some(Placement::Header(name, value)) => {
                    req.headers_mut().insert(
                        axum::http::HeaderName::try_from(name).expect("header name"),
                        value.parse().expect("header value"),
                    );
                }
                Some(Placement::Query(name, value)) => {
                    let uri = req.uri().to_string();
                    let sep = if uri.contains('?') { '&' } else { '?' };
                    let pair = serde_urlencoded::to_string([(name, value)]).expect("encodes");
                    *req.uri_mut() = format!("{uri}{sep}{pair}").parse().expect("uri");
                }
                Some(Placement::Cookie(name, value)) => cookies.push(format!("{name}={value}")),
                None => panic!("{key}: security scheme `{scheme}` isn't defined in components.securitySchemes"),
            }
        }
        if !cookies.is_empty() {
            let existing = req.headers().get("cookie").and_then(|v| v.to_str().ok()).map(str::to_owned);
            let all = existing.into_iter().chain(cookies).collect::<Vec<_>>().join("; ");
            req.headers_mut().insert("cookie", all.parse().expect("cookie"));
        }
        schemes
    }

    #[allow(clippy::too_many_arguments)]
    async fn send(
        &self,
        index: &SpecIndex,
        spec: &Value,
        path: &str,
        op: &Value,
        key: &str,
        case: &str,
        mut req: Request<Body>,
    ) -> Sent {
        if let Some(hook) = &self.before_each {
            hook().await;
        }
        let schemes = self.authorize(spec, op, key, &mut req);
        let (status, body) = send_with(index, self.router, path, req).await.unwrap_or_else(|e| panic!("{case}: {e}"));
        Sent { status, body, schemes }
    }

    /// The docs-built values, with fixtures and supplied bodies in place.
    fn base_values(&self, spec: &Value, key: &str, inputs: &[Input]) -> (Vec<Option<Value>>, Vec<bool>) {
        let mut fixed = vec![false; inputs.len()];
        let values = inputs
            .iter()
            .enumerate()
            .map(|(i, input)| {
                let supplied = if input.location == "body" {
                    self.bodies.iter().find(|(op, _)| op == key).map(|(_, v)| v.clone())
                } else {
                    let for_op = self.fixtures.iter().find(|(op, n, _)| op.as_deref() == Some(key) && *n == input.name);
                    let any = self.fixtures.iter().find(|(op, n, _)| op.is_none() && *n == input.name);
                    for_op.or(any).map(|(_, _, v)| v.clone())
                };
                match supplied {
                    Some(v) => {
                        assert!(
                            valid(spec, &input.schema, &v),
                            "{key}: the value supplied for {} doesn't match its schema: {v}",
                            input.describe()
                        );
                        fixed[i] = true;
                        Some(v)
                    }
                    None => input.required.then(|| input.default_value(spec, key)),
                }
            })
            .collect();
        (values, fixed)
    }

    /// Run the checks and report what each operation exercised.
    ///
    /// # Panics
    /// On the first request that violates one of the rules above.
    pub async fn run(self) -> Report {
        let spec = serde_json::to_value(self.openapi).expect("OpenAPI serializes");
        let index = SpecIndex::new(self.openapi);
        let mut report = Report { operations: Vec::new() };
        for (path, method, op) in operations(&spec) {
            let key = format!("{method} {path}");
            let ins = inputs(&spec, &op);
            let (defaults, fixed) = self.base_values(&spec, &key, &ins);
            let sent =
                self.send(&index, &spec, &path, &op, &key, &key, request_with(&path, &method, &ins, &defaults)).await;
            let expected = self.expected.iter().find(|(k, _)| *k == key).map(|(_, s)| *s);
            if let Some(want) = expected {
                assert_eq!(sent.status, want, "{key}: expected {want}, got {}: {}", sent.status, sent.body);
            } else {
                sent.accepted(&key);
            }
            let mut entry = OperationReport {
                operation: key.clone(),
                status: sent.status,
                valid_cases: 0,
                invalid_cases: 0,
                statuses: std::iter::once(sent.status.as_u16()).collect(),
                not_exercised: Vec::new(),
                skipped: Vec::new(),
            };
            if expected.is_some() {
                entry.skipped.push("generated cases: `.expect` set for this operation".into());
            } else if self.cases == 0 {
                entry.skipped.push("generated cases: `.cases(0)`".into());
            } else {
                let cases = valid_cases(&spec, &ins, &defaults, self.cases + 1);
                for (i, input) in ins.iter().enumerate() {
                    if fixed[i] {
                        continue;
                    }
                    let varies = cases.iter().any(|c| c[i].is_some() && c[i] != defaults[i]);
                    if !varies && resolve(&spec, &input.schema).get("pattern").is_some() {
                        entry.skipped.push(format!(
                            "valid variants: {} uses its example only (a `pattern` can't be generated)",
                            input.describe()
                        ));
                    }
                }
                for mut values in cases {
                    for (i, f) in fixed.iter().enumerate() {
                        if *f {
                            values[i].clone_from(&defaults[i]);
                        }
                    }
                    let case = format!("{key} (generated valid request: {})", describe(&ins, &values));
                    let req = request_with(&path, &method, &ins, &values);
                    let sent = self.send(&index, &spec, &path, &op, &key, &case, req).await;
                    sent.accepted(&case);
                    entry.valid_cases += 1;
                    entry.statuses.insert(sent.status.as_u16());
                }
                for (i, input) in ins.iter().enumerate() {
                    // Headers and cookies belong to their extractors; forms lose JSON types.
                    let checked = match input.location.as_str() {
                        "path" | "query" => true,
                        "body" => input.name.contains("json"),
                        _ => false,
                    };
                    if !checked {
                        entry.skipped.push(format!("invalid values: {} isn't generated", input.describe()));
                        continue;
                    }
                    let wire = input.location != "body";
                    let bad: Vec<(String, Value)> = invalid_values(&spec, &input.schema, wire)
                        .into_iter()
                        .filter(|(_, v)| input.representable(v))
                        .take(self.cases)
                        .collect();
                    if bad.is_empty() {
                        entry.skipped.push(format!("invalid values: {} has no constraint to break", input.describe()));
                    }
                    for (label, value) in bad {
                        let mut values = defaults.clone();
                        values[i] = Some(value.clone());
                        let case =
                            format!("{key} (generated invalid request, {}: {label}, value {value})", input.describe());
                        let req = request_with(&path, &method, &ins, &values);
                        let sent = self.send(&index, &spec, &path, &op, &key, &case, req).await;
                        let rejected = matches!(sent.status.as_u16(), 400 | 415 | 422)
                            || (input.location == "path" && sent.status == StatusCode::NOT_FOUND);
                        assert!(
                            rejected,
                            "{case}: answered {}: {}\n  the docs say this request is invalid, but the handler accepted it: \
                             it doesn't enforce what the docs promise",
                            sent.status, sent.body
                        );
                        entry.invalid_cases += 1;
                        entry.statuses.insert(sent.status.as_u16());
                    }
                }
            }
            for status in op["responses"].as_object().into_iter().flatten().map(|(s, _)| s) {
                let seen = entry.statuses.iter().any(|code| {
                    let code = code.to_string();
                    *status == code || (status.ends_with("XX") && status[..1] == code[..1])
                });
                if !seen && status != "default" {
                    entry.not_exercised.push(status.clone());
                }
            }
            report.operations.push(entry);
        }
        report
    }
}

/// What a [`Conformance`] run exercised. `Display` prints one line per operation, with
/// the documented statuses no request reached and the inputs it couldn't vary.
#[derive(Debug, Clone)]
pub struct Report {
    pub operations: Vec<OperationReport>,
}

/// One operation's part of a [`Report`].
#[derive(Debug, Clone)]
pub struct OperationReport {
    /// `"GET /todos/{id}"`.
    pub operation: String,
    /// The docs-built request's status.
    pub status: StatusCode,
    pub valid_cases: usize,
    pub invalid_cases: usize,
    /// Every status any request got.
    pub statuses: std::collections::BTreeSet<u16>,
    /// Documented statuses no request got (such as a `404` the test data never hits).
    pub not_exercised: Vec<String>,
    /// Inputs and cases the run didn't generate, with the reason.
    pub skipped: Vec<String>,
}

impl Report {
    pub fn len(&self) -> usize {
        self.operations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// The report for one operation (`"GET /todos/{id}"`).
    pub fn get(&self, operation: &str) -> Option<&OperationReport> {
        self.operations.iter().find(|o| o.operation == operation)
    }

    /// `(operation, status)` for each docs-built request.
    pub fn statuses(&self) -> Vec<(String, StatusCode)> {
        self.operations.iter().map(|o| (o.operation.clone(), o.status)).collect()
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for o in &self.operations {
            let statuses: Vec<String> = o.statuses.iter().map(u16::to_string).collect();
            write!(
                f,
                "{}: {} valid, {} invalid, statuses {}",
                o.operation,
                o.valid_cases + 1,
                o.invalid_cases,
                statuses.join(", ")
            )?;
            if !o.not_exercised.is_empty() {
                write!(f, "; not exercised: {}", o.not_exercised.join(", "))?;
            }
            writeln!(f)?;
            for s in &o.skipped {
                writeln!(f, "  skipped {s}")?;
            }
        }
        Ok(())
    }
}

struct Sent {
    status: StatusCode,
    body: Value,
    schemes: Vec<String>,
}

impl Sent {
    /// A request built from the docs must not be rejected as malformed or unauthenticated.
    fn accepted(&self, key: &str) {
        let (status, body, schemes) = (self.status, &self.body, &self.schemes);
        if matches!(status.as_u16(), 400 | 415 | 422) {
            panic!(
                "{key}: a request built from the docs was rejected ({status}): {body}\n  \
                 the docs don't describe a valid request (missing param/header, wrong body?)"
            );
        }
        if matches!(status.as_u16(), 401 | 403) {
            if schemes.is_empty() {
                panic!(
                    "{key}: answered {status} but documents no security requirement: it needs \
                     authentication the docs don't describe (document it with ctx.security in ExtractorDoc)"
                );
            }
            panic!(
                "{key}: credentials for {schemes:?} were rejected ({status}); pass real ones with \
                 `.credential(scheme, value)`, or `.expect(\"{key}\", StatusCode::...)` if that's intended"
            );
        }
    }
}

/// `query limit=5, body {...}` for a failure message.
fn describe(inputs: &[Input], values: &[Option<Value>]) -> String {
    let parts: Vec<String> = inputs
        .iter()
        .zip(values)
        .filter_map(|(i, v)| {
            let v = v.as_ref()?;
            Some(if i.location == "body" { format!("body {v}") } else { format!("{} {}={v}", i.location, i.name) })
        })
        .collect();
    if parts.is_empty() { "no inputs".into() } else { parts.join(", ") }
}

/// [`Conformance`] with defaults: placeholder credentials, no fixtures, 8 cases.
///
/// # Panics
/// See [`Conformance::run`].
pub async fn assert_conforms(router: &Router, openapi: &OpenApi) -> Report {
    Conformance::new(router, openapi).run().await
}
