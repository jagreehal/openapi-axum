//! Validators compiled once from the **final published document**, so runtime checks
//! enforce exactly what clients read: query/path params (`ParamsGuard`), and, with
//! `verify_responses`, every outgoing response.

use std::collections::HashMap;

use axum::http::{Method, StatusCode};
use serde_json::{Map, Value, json};

use crate::RejectionKind;

struct Params {
    /// (name, location, schema). Query and path params may share a name (`/items/{id}?id=`),
    /// so everything below is keyed by location *and* name.
    fields: Vec<(String, RejectionKind, Value, ArrayStyle)>,
    query: Option<jsonschema::Validator>,
    path: Option<jsonschema::Validator>,
}

/// A published request body for one content type.
pub(crate) struct BodySpec {
    pub(crate) schema: Value,
    pub(crate) validator: jsonschema::Validator,
}

struct ResponseSpec {
    /// documented content types -> body validator
    content: Vec<(String, jsonschema::Validator)>,
}

struct OperationSpec {
    params: Option<Params>,
    /// content-type essence -> published body schema
    bodies: HashMap<String, BodySpec>,
    responses: HashMap<String, ResponseSpec>,
}

/// Compiled view of an OpenAPI document. Built by `ApiRouter::split`.
pub struct SpecIndex {
    root: Value,
    ops: HashMap<(Method, String), OperationSpec>,
}

fn resolve<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    match schema.get("$ref").and_then(Value::as_str).and_then(|r| root.pointer(r.trim_start_matches('#'))) {
        Some(target) => resolve(root, target),
        None => schema,
    }
}

/// How an array parameter's values are written on the wire (OpenAPI `style`/`explode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArrayStyle {
    /// One `name=value` per item (`form` + `explode`, the query/cookie default).
    Repeated,
    /// One value, items joined (`simple`, `form` without explode, space/pipe delimited).
    Delimited(char),
}

/// The array serialization a parameter object declares, with OpenAPI's defaults.
pub(crate) fn array_style(param: &Value) -> ArrayStyle {
    let location = param["in"].as_str().unwrap_or("query");
    let default_style = if matches!(location, "query" | "cookie") { "form" } else { "simple" };
    let style = param["style"].as_str().unwrap_or(default_style);
    let explode = param["explode"].as_bool().unwrap_or(style == "form");
    match style {
        "form" if explode => ArrayStyle::Repeated,
        "spaceDelimited" => ArrayStyle::Delimited(' '),
        "pipeDelimited" => ArrayStyle::Delimited('|'),
        "deepObject" => ArrayStyle::Repeated,
        _ => ArrayStyle::Delimited(','),
    }
}

/// The non-null type a parameter value should be coerced to.
fn primary_type<'a>(root: &'a Value, schema: &'a Value) -> (&'a str, &'a Value) {
    let schema = resolve(root, schema);
    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(Value::Array(options)) = schema.get(key)
            && let Some(non_null) = options.iter().find(|o| resolve(root, o).get("type") != Some(&json!("null")))
        {
            return primary_type(root, non_null);
        }
    }
    let ty = match schema.get("type") {
        Some(Value::String(t)) => t.as_str(),
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).find(|t| *t != "null").unwrap_or("string"),
        _ => "string",
    };
    (ty, schema)
}

/// Query/path/form values arrive as strings; turn them into the JSON the schema
/// describes (`"5"` -> `5` for integers, repeated keys -> arrays) before validating.
/// Values that don't parse stay strings, so validation reports a type error.
pub(crate) fn coerce(root: &Value, schema: &Value, raw: &[&str]) -> Value {
    coerce_styled(root, schema, raw, ArrayStyle::Repeated)
}

/// [`coerce`] for a parameter whose arrays may be delimited (`ids=1,2`) rather than repeated.
pub(crate) fn coerce_styled(root: &Value, schema: &Value, raw: &[&str], style: ArrayStyle) -> Value {
    let (ty, schema) = primary_type(root, schema);
    if ty == "array" {
        let items = schema.get("items").cloned().unwrap_or(json!({}));
        let values: Vec<&str> = match style {
            ArrayStyle::Repeated => raw.to_vec(),
            ArrayStyle::Delimited(sep) => match raw.last() {
                Some(&"") => Vec::new(),
                Some(s) => s.split(sep).collect(),
                None => Vec::new(),
            },
        };
        return Value::Array(values.iter().map(|r| coerce(root, &items, &[r])).collect());
    }
    let Some(s) = raw.last() else { return Value::Null };
    match ty {
        "integer" => {
            s.parse::<i64>().map(Value::from).or_else(|_| s.parse::<u64>().map(Value::from)).unwrap_or(json!(s))
        }
        "number" => {
            s.parse::<f64>().ok().and_then(|n| serde_json::Number::from_f64(n).map(Value::Number)).unwrap_or(json!(s))
        }
        "boolean" => match *s {
            "true" => json!(true),
            "false" => json!(false),
            other => json!(other),
        },
        _ => json!(s),
    }
}

/// The schema of property `name`, looking through `$ref` and composition
/// (`#[serde(flatten)]` produces `allOf`).
fn find_property<'a>(root: &'a Value, schema: &'a Value, name: &str) -> Option<&'a Value> {
    let schema = resolve(root, schema);
    if let Some(p) = schema.get("properties").and_then(|p| p.get(name)) {
        return Some(p);
    }
    ["allOf", "oneOf", "anyOf"]
        .iter()
        .filter_map(|k| schema.get(*k).and_then(Value::as_array))
        .flatten()
        .find_map(|part| find_property(root, part, name))
}

/// Coerce urlencoded pairs into a JSON object according to an object schema.
pub(crate) fn coerce_object(root: &Value, object_schema: &Value, pairs: &[(String, String)]) -> Value {
    let mut grouped: Vec<(&str, Vec<&str>)> = Vec::new();
    for (k, v) in pairs {
        match grouped.iter_mut().find(|(name, _)| name == k) {
            Some((_, vals)) => vals.push(v),
            None => grouped.push((k, vec![v])),
        }
    }
    let mut out = Map::new();
    for (k, vals) in grouped {
        let value = match find_property(root, object_schema, k) {
            Some(prop) => coerce(root, prop, &vals),
            None => json!(vals.last().copied().unwrap_or_default()),
        };
        out.insert(k.to_owned(), value);
    }
    Value::Object(out)
}

/// `"application/json; charset=utf-8"` -> `"application/json"`.
pub(crate) fn essence(content_type: &str) -> String {
    content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase()
}

pub(crate) fn compile(root: &Value, schema: &Value) -> jsonschema::Validator {
    let mut wrapper = json!({ "allOf": [schema] });
    wrapper["components"] = root.get("components").cloned().unwrap_or(json!({}));
    jsonschema::options()
        .should_validate_formats(true)
        .build(&wrapper)
        .unwrap_or_else(|e| panic!("published schema is not valid JSON Schema: {e}"))
}

/// `"/limit" + error` -> "query parameter `limit`: error".
pub(crate) fn describe_errors(
    validator: &jsonschema::Validator,
    instance: &Value,
    label: impl Fn(&str) -> String,
) -> Vec<String> {
    validator
        .iter_errors(instance)
        .map(|e| {
            let path = e.instance_path().to_string();
            let field = path.trim_start_matches('/').split('/').next().unwrap_or_default().to_owned();
            if field.is_empty() { e.to_string() } else { format!("{}: {e}", label(&field)) }
        })
        .collect()
}

impl SpecIndex {
    pub fn new(openapi: &utoipa::openapi::OpenApi) -> Self {
        let root = serde_json::to_value(openapi).expect("OpenAPI serializes");
        let mut ops = HashMap::new();
        for (path, item) in root["paths"].as_object().into_iter().flatten() {
            for (m, op) in item.as_object().into_iter().flatten() {
                let Ok(method) = m.to_uppercase().parse::<Method>() else { continue };
                ops.insert((method, path.clone()), Self::operation(&root, op));
            }
        }
        Self { root, ops }
    }

    fn operation(root: &Value, op: &Value) -> OperationSpec {
        let mut fields = Vec::new();
        let mut objects: [(Map<String, Value>, Vec<Value>); 2] = Default::default(); // [query, path]
        for p in op["parameters"].as_array().into_iter().flatten() {
            let p = resolve(root, p);
            let (kind, slot) = match p["in"].as_str() {
                Some("query") => (RejectionKind::Query, 0),
                Some("path") => (RejectionKind::Path, 1),
                _ => continue, // headers/cookies belong to their extractors
            };
            let name = p["name"].as_str().unwrap_or_default().to_owned();
            let schema = p.get("schema").cloned().unwrap_or(json!({}));
            let (properties, required) = &mut objects[slot];
            if p["required"].as_bool().unwrap_or(false) {
                required.push(json!(name));
            }
            properties.insert(name.clone(), schema.clone());
            fields.push((name, kind, schema, array_style(p)));
        }
        let [(qp, qr), (pp, pr)] = objects;
        let object = |properties: Map<String, Value>, required: Vec<Value>| {
            (!properties.is_empty())
                .then(|| compile(root, &json!({ "type": "object", "properties": properties, "required": required })))
        };
        let params = (!fields.is_empty()).then(|| Params { fields, query: object(qp, qr), path: object(pp, pr) });
        let bodies = resolve(root, &op["requestBody"])
            .get("content")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(ct, media)| {
                let schema = media.get("schema").cloned().unwrap_or(json!({}));
                (essence(ct), BodySpec { validator: compile(root, &schema), schema })
            })
            .collect();
        let mut responses = HashMap::new();
        for (status, r) in op["responses"].as_object().into_iter().flatten() {
            let r = resolve(root, r);
            let content = r["content"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(ct, media)| (ct.clone(), compile(root, media.get("schema").unwrap_or(&json!({})))))
                .collect();
            responses.insert(status.clone(), ResponseSpec { content });
        }
        OperationSpec { params, bodies, responses }
    }

    fn op(&self, method: &Method, path: &str) -> Option<&OperationSpec> {
        self.ops.get(&(method.clone(), path.to_owned())).or_else(|| {
            // axum serves HEAD from GET routes.
            (method == Method::HEAD).then(|| self.ops.get(&(Method::GET, path.to_owned()))).flatten()
        })
    }

    /// Query pairs as the JSON object the documented query params describe
    /// (`?tag=a&tag=b` -> `{"tag": ["a", "b"]}` when `tag` is an array).
    pub(crate) fn coerce_query(&self, method: &Method, path: &str, query: &[(String, String)]) -> Option<Value> {
        let params = self.op(method, path)?.params.as_ref()?;
        let mut out = Map::new();
        for (name, kind, schema, style) in &params.fields {
            let raw: Vec<&str> = query.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()).collect();
            if *kind == RejectionKind::Query && !raw.is_empty() {
                out.insert(name.clone(), coerce_styled(&self.root, schema, &raw, *style));
            }
        }
        Some(Value::Object(out))
    }

    /// Validate query and path values against the documented parameter schemas,
    /// each location against its own schema.
    pub(crate) fn check_params(
        &self,
        method: &Method,
        path: &str,
        query: &[(String, String)],
        path_params: &[(String, String)],
    ) -> Result<(), (RejectionKind, String)> {
        let Some(params) = self.op(method, path).and_then(|o| o.params.as_ref()) else { return Ok(()) };
        for (kind, validator, source, label) in [
            (RejectionKind::Path, &params.path, path_params, "path"),
            (RejectionKind::Query, &params.query, query, "query"),
        ] {
            let Some(validator) = validator else { continue };
            let mut instance = Map::new();
            for (name, _, schema, style) in params.fields.iter().filter(|(_, k, ..)| *k == kind) {
                let raw: Vec<&str> = source.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()).collect();
                if !raw.is_empty() {
                    instance.insert(name.clone(), coerce_styled(&self.root, schema, &raw, *style));
                }
            }
            let errors = describe_errors(validator, &Value::Object(instance), |f| format!("{label} parameter `{f}`"));
            if !errors.is_empty() {
                return Err((kind, errors.join("; ")));
            }
        }
        Ok(())
    }

    /// The published request body for `content_type`, if the operation documents one.
    pub(crate) fn body(&self, method: &Method, path: &str, content_type: &str) -> Option<&BodySpec> {
        self.op(method, path)?.bodies.get(&essence(content_type))
    }

    pub(crate) fn root(&self) -> &Value {
        &self.root
    }

    /// Check a response against the documented responses of its operation.
    /// `Ok(())` for routes that aren't documented (e.g. `route_undocumented`).
    pub fn check_response(
        &self,
        method: &Method,
        path: &str,
        status: StatusCode,
        content_type: Option<&str>,
        body: &[u8],
    ) -> Result<(), String> {
        let Some(op) = self.op(method, path) else { return Ok(()) };
        let label = format!("{method} {path} -> {status}");
        let class = format!("{}XX", status.as_u16() / 100);
        let documented = op
            .responses
            .get(status.as_str())
            .or_else(|| op.responses.get(&class))
            .or_else(|| op.responses.get("default"))
            .ok_or_else(|| format!("{label}: status is not documented"))?;
        // No body and no content type: nothing was sent. An empty body *with* a content
        // type (e.g. `String::new()` as text/plain) is a value, validated below.
        if body.is_empty() && content_type.is_none() {
            return if documented.content.is_empty() {
                Ok(())
            } else {
                Err(format!("{label}: documented body is missing"))
            };
        }
        if documented.content.is_empty() {
            if body.is_empty() {
                return Ok(());
            }
            return Err(format!("{label}: body returned but none is documented"));
        }
        let ct = content_type.unwrap_or_default();
        let (_, validator) = documented
            .content
            .iter()
            .find(|(doc_ct, _)| essence(doc_ct) == essence(ct))
            .ok_or_else(|| format!("{label}: content-type `{ct}` is not documented"))?;
        let instance: Value = if essence(ct).ends_with("json") {
            serde_json::from_slice(body).map_err(|e| format!("{label}: body is not valid JSON: {e}"))?
        } else {
            Value::String(String::from_utf8_lossy(body).into_owned())
        };
        let errors = describe_errors(validator, &instance, |f| format!("field `{f}`"));
        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!("{label}: body doesn't match its schema: {}", errors.join("; ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ArrayStyle, array_style, coerce_styled};
    use serde_json::json;

    #[test]
    fn array_style_follows_openapi_defaults() {
        for (param, want) in [
            (json!({ "in": "query" }), ArrayStyle::Repeated),
            (json!({ "in": "query", "explode": false }), ArrayStyle::Delimited(',')),
            (json!({ "in": "query", "style": "spaceDelimited" }), ArrayStyle::Delimited(' ')),
            (json!({ "in": "query", "style": "pipeDelimited" }), ArrayStyle::Delimited('|')),
            (json!({ "in": "cookie" }), ArrayStyle::Repeated),
            (json!({ "in": "path" }), ArrayStyle::Delimited(',')),
            (json!({ "in": "path", "explode": true }), ArrayStyle::Delimited(',')), // simple: explode only affects objects
            (json!({ "in": "header" }), ArrayStyle::Delimited(',')),
        ] {
            assert_eq!(array_style(&param), want, "{param}");
        }
    }

    #[test]
    fn delimited_values_are_split_and_coerced() {
        let schema = json!({ "type": "array", "items": { "type": "integer" } });
        let root = json!({});
        assert_eq!(coerce_styled(&root, &schema, &["1,2"], ArrayStyle::Delimited(',')), json!([1, 2]));
        assert_eq!(coerce_styled(&root, &schema, &["1|2"], ArrayStyle::Delimited('|')), json!([1, 2]));
        assert_eq!(coerce_styled(&root, &schema, &["1", "2"], ArrayStyle::Repeated), json!([1, 2]));
        assert_eq!(coerce_styled(&root, &schema, &[""], ArrayStyle::Delimited(',')), json!([]));
    }
}
