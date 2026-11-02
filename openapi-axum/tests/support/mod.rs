//! Spec-driven test support shared by the integration tests.
//!
//! The idea (schemathesis-lite): never trust the docs and the router separately.
//! Build a request *from the published spec*, send it to the real router, and check
//! the response *against the spec*. Any disagreement between docs and runtime fails.
#![allow(dead_code)]

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Map, Value, json};
use tower::ServiceExt;

/// The document is valid OpenAPI 3.1 (official meta-schema) and every `$ref` resolves.
pub fn assert_valid_document(spec: &Value) {
    let meta: Value = serde_json::from_str(include_str!("../fixtures/oas-3.1-schema.json")).unwrap();
    let validator = jsonschema::validator_for(&meta).expect("meta-schema compiles");
    let errors: Vec<String> = validator.iter_errors(spec).map(|e| format!("{e} at {}", e.instance_path())).collect();
    assert!(errors.is_empty(), "not valid OpenAPI 3.1:\n{}", errors.join("\n"));
    let mut refs = Vec::new();
    collect_refs(spec, &mut refs);
    for r in refs {
        assert!(spec.pointer(r.trim_start_matches('#')).is_some(), "dangling $ref {r}");
    }
}

fn collect_refs(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            if let Some(Value::String(r)) = m.get("$ref") {
                out.push(r.clone());
            }
            m.values().for_each(|v| collect_refs(v, out));
        }
        Value::Array(a) => a.iter().for_each(|v| collect_refs(v, out)),
        _ => {}
    }
}

fn resolve<'a>(spec: &'a Value, schema: &'a Value) -> &'a Value {
    match schema.get("$ref").and_then(Value::as_str) {
        Some(r) => resolve(spec, spec.pointer(r.trim_start_matches('#')).expect("ref resolves")),
        None => schema,
    }
}

/// A minimal value that satisfies `schema` (required fields only, bounds respected).
pub fn example(spec: &Value, schema: &Value) -> Value {
    let schema = resolve(spec, schema);
    if let Some(e) = schema.get("enum").and_then(|e| e.get(0)) {
        return e.clone();
    }
    for key in ["allOf", "oneOf", "anyOf"] {
        if let Some(Value::Array(options)) = schema.get(key) {
            let pick = options.iter().find(|o| o.get("type") != Some(&json!("null"))).unwrap_or(&options[0]);
            return example(spec, pick);
        }
    }
    let ty = match schema.get("type") {
        Some(Value::String(t)) => t.as_str(),
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).find(|t| *t != "null").unwrap_or("null"),
        _ if schema.get("properties").is_some() => "object",
        _ => "null",
    };
    let num = |default: f64| schema.get("minimum").and_then(Value::as_f64).unwrap_or(default);
    match ty {
        "object" => {
            let required: Vec<&str> =
                schema["required"].as_array().map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let props = schema["properties"].as_object().cloned().unwrap_or_default();
            let fields: Map<String, Value> = props
                .iter()
                .filter(|(k, _)| required.contains(&k.as_str()))
                .map(|(k, s)| (k.clone(), example(spec, s)))
                .collect();
            Value::Object(fields)
        }
        "array" => match schema.get("minItems").and_then(Value::as_u64) {
            Some(n) if n > 0 => Value::Array(vec![example(spec, &schema["items"]); n as usize]),
            _ => json!([]),
        },
        "string" => match schema.get("format").and_then(Value::as_str) {
            Some("uuid") => json!("00000000-0000-4000-8000-000000000001"),
            Some("date-time") => json!("2026-01-01T00:00:00Z"),
            Some("date") => json!("2026-01-01"),
            _ => json!("a".repeat(schema.get("minLength").and_then(Value::as_u64).unwrap_or(1) as usize)),
        },
        "integer" => json!(num(1.0) as i64),
        "number" => json!(num(1.0)),
        "boolean" => json!(true),
        _ => Value::Null,
    }
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Every documented (path, method, operation).
pub fn operations(spec: &Value) -> Vec<(String, Method, Value)> {
    let mut ops = Vec::new();
    for (path, item) in spec["paths"].as_object().unwrap() {
        for (m, op) in item.as_object().unwrap() {
            if let Ok(method) = m.to_uppercase().parse::<Method>() {
                ops.push((path.clone(), method, op.clone()));
            }
        }
    }
    ops
}

/// A request built only from what the spec says this operation needs.
pub fn request_from_spec(spec: &Value, path: &str, method: &Method, op: &Value) -> Request<Body> {
    let mut uri = path.to_owned();
    let mut query = Vec::new();
    let mut req = Request::builder().method(method.clone());
    for p in op["parameters"].as_array().into_iter().flatten() {
        let required = p["required"].as_bool().unwrap_or(false);
        let value = scalar(&example(spec, &p["schema"]));
        let name = p["name"].as_str().unwrap();
        match p["in"].as_str().unwrap() {
            "path" => uri = uri.replace(&format!("{{{name}}}"), &value),
            "query" if required => query.push(format!("{name}={value}")),
            "header" if required => req = req.header(name, value),
            "cookie" if required => req = req.header("cookie", format!("{name}={value}")),
            _ => {}
        }
    }
    assert!(!uri.contains('{'), "{method} {path}: a path capture isn't documented as a parameter");
    if !query.is_empty() {
        uri = format!("{uri}?{}", query.join("&"));
    }
    let body = op["requestBody"]["content"].as_object().and_then(|c| c.iter().next());
    let body = match body {
        Some((ct, media)) => {
            let value = example(spec, &media["schema"]);
            req = req.header("content-type", ct.as_str());
            if ct.starts_with("application/x-www-form-urlencoded") {
                let pairs: Vec<String> =
                    value.as_object().unwrap().iter().map(|(k, v)| format!("{k}={}", scalar(v))).collect();
                Body::from(pairs.join("&"))
            } else {
                Body::from(value.to_string())
            }
        }
        None => Body::empty(),
    };
    req.uri(uri).body(body).unwrap()
}

/// Send a request; check its status is documented and its body matches the schema.
pub async fn send_checked(router: &axum::Router, spec: &Value, path: &str, req: Request<Body>) -> (StatusCode, Value) {
    let method = req.method().as_str().to_lowercase();
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let content_type = res.headers().get("content-type").map(|v| v.to_str().unwrap().to_owned());
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let op = &spec["paths"][path][&method];
    let responses = &op["responses"];
    let documented = responses.get(status.as_str()).unwrap_or_else(|| {
        panic!("{method} {path} returned undocumented {status}: {}", String::from_utf8_lossy(&bytes))
    });
    let documented = resolve(spec, documented);
    for name in documented["headers"].as_object().into_iter().flatten().map(|(k, _)| k) {
        // Typed headers are always sent unless Option; the kitchen sink only uses required ones.
        if !name.starts_with("X-Optional") {
            assert!(headers.contains_key(name.as_str()), "{method} {path} {status}: documented header {name} missing");
        }
    }
    let content = documented["content"].as_object();
    let body = if bytes.is_empty() {
        assert!(content.is_none_or(|c| c.is_empty()), "{method} {path} {status}: documented body missing");
        Value::Null
    } else {
        let content = content.unwrap_or_else(|| panic!("{method} {path} {status}: undocumented body"));
        let ct = content_type.unwrap_or_default();
        let (_, media) = content
            .iter()
            .find(|(k, _)| ct.starts_with(k.split(';').next().unwrap()))
            .unwrap_or_else(|| panic!("{method} {path} {status}: content-type {ct} not documented"));
        let body: Value = if ct.starts_with("application/json") {
            serde_json::from_slice(&bytes).unwrap()
        } else {
            Value::String(String::from_utf8(bytes.to_vec()).unwrap())
        };
        let mut root = json!({ "allOf": [media["schema"]] });
        root["components"] = spec["components"].clone();
        let validator = jsonschema::validator_for(&root).unwrap();
        let errors: Vec<String> = validator.iter_errors(&body).map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{method} {path} {status}: body doesn't match schema: {errors:?}\n{body}");
        body
    };
    (status, body)
}
