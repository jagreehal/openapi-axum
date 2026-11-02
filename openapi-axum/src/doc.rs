//! Building blocks the macros call to assemble an OpenAPI [`Operation`] from types.

use std::marker::PhantomData;

use axum::http::StatusCode;
use utoipa::openapi::path::{Operation, Parameter, ParameterBuilder, ParameterIn};
use utoipa::openapi::request_body::RequestBodyBuilder;
#[cfg(any(feature = "uuid", feature = "chrono"))]
use utoipa::openapi::schema::{KnownFormat, ObjectBuilder, SchemaFormat};
use utoipa::openapi::schema::{Schema, SchemaType, Type};
use utoipa::openapi::security::{SecurityRequirement, SecurityScheme};
use utoipa::openapi::{Content, Header, Ref, RefOr, Required, ResponseBuilder, Responses};
use utoipa::{IntoParams, ToSchema};

/// Collects schemas and problems while one operation is documented.
#[derive(Default)]
pub struct DocCtx {
    pub(crate) operation_id: &'static str,
    pub(crate) schemas: Vec<(String, RefOr<Schema>)>,
    pub(crate) security_schemes: Vec<(String, SecurityScheme)>,
    /// scheme -> scopes for this operation's (single, AND-ed) security requirement.
    security_scopes: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    pub(crate) errors: Vec<String>,
}

impl DocCtx {
    pub(crate) fn new(operation_id: &'static str) -> Self {
        Self { operation_id, ..Default::default() }
    }

    /// Record a contract problem; the router panics with all of them at registration.
    pub fn error(&mut self, msg: impl Into<String>) {
        self.errors.push(format!("{}: {}", self.operation_id, msg.into()));
    }

    /// Schema for `T`: named object/enum types become `$ref`s to `components.schemas`,
    /// primitives and arrays stay inline.
    pub fn schema<T: ToSchema>(&mut self) -> RefOr<Schema> {
        T::schemas(&mut self.schemas);
        let schema = T::schema();
        let inline = match &schema {
            RefOr::Ref(_) => true,
            RefOr::T(Schema::Array(_)) => true,
            RefOr::T(Schema::Object(o)) => match &o.schema_type {
                // Named struct/enum: has properties or enum values. Plain maps stay inline.
                SchemaType::Type(Type::Object) => o.properties.is_empty() && o.enum_values.is_none(),
                _ => o.enum_values.is_none(),
            },
            RefOr::T(_) => false, // oneOf / allOf / anyOf: named composites
        };
        if inline {
            return schema;
        }
        let name = T::name().into_owned();
        self.schemas.push((name.clone(), schema));
        RefOr::Ref(Ref::from_schema_name(name))
    }

    fn insert(&mut self, responses: &mut Responses, status: u16, response: utoipa::openapi::Response) {
        let key = status.to_string();
        if responses.responses.contains_key(&key) {
            self.error(format!(
                "status {status} is documented twice; OpenAPI allows one response per status, so merge them into one variant"
            ));
            return;
        }
        responses.responses.insert(key, RefOr::T(response));
    }

    fn description(status: u16, description: Option<&str>) -> String {
        description.map(str::to_owned).unwrap_or_else(|| {
            StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or("Response").to_owned()
        })
    }

    /// A response: optional JSON body schema plus response headers (name, schema).
    pub fn response(
        &mut self,
        responses: &mut Responses,
        status: u16,
        description: Option<&str>,
        body: Option<RefOr<Schema>>,
        headers: Vec<(&str, RefOr<Schema>)>,
    ) {
        let mut r = ResponseBuilder::new().description(Self::description(status, description));
        if let Some(schema) = body {
            r = r.content("application/json", Content::new(Some(schema)));
        }
        for (name, schema) in headers {
            r = r.header(name, Header::new(schema));
        }
        self.insert(responses, status, r.build());
    }

    /// A response with a JSON body of `T`.
    pub fn json_response<T: ToSchema>(&mut self, responses: &mut Responses, status: u16, description: Option<&str>) {
        let schema = self.schema::<T>();
        self.response(responses, status, description, Some(schema), Vec::new());
    }

    /// A response with no body.
    pub fn empty_response(&mut self, responses: &mut Responses, status: u16, description: Option<&str>) {
        self.response(responses, status, description, None, Vec::new());
    }

    /// A `text/plain` response.
    pub fn text_response(&mut self, responses: &mut Responses, status: u16, description: Option<&str>) {
        let schema = self.schema::<String>();
        let r = ResponseBuilder::new()
            .description(Self::description(status, description))
            .content("text/plain; charset=utf-8", Content::new(Some(schema)))
            .build();
        self.insert(responses, status, r);
    }

    /// Register `T` as a component if it's a named type (used for `Vec<T>` etc. so
    /// array items can become `$ref`s).
    pub fn register<T: ToSchema>(&mut self) {
        let _ = self.schema::<T>();
    }

    /// A single named parameter in any location.
    pub fn param<T: ToSchema>(
        &mut self,
        op: &mut Operation,
        name: &str,
        location: ParameterIn,
        required: bool,
        description: Option<&str>,
    ) {
        let schema = self.schema::<T>();
        let p: Parameter = ParameterBuilder::new()
            .name(name)
            .parameter_in(location)
            .required(if required { Required::True } else { Required::False })
            .description(description)
            .schema(Some(schema))
            .build();
        op.parameters.get_or_insert_default().push(RefOr::T(p));
    }

    /// A request header parameter (for `ExtractorDoc` impls).
    pub fn header_param<T: ToSchema>(
        &mut self,
        op: &mut Operation,
        name: &str,
        required: bool,
        description: Option<&str>,
    ) {
        self.param::<T>(op, name, ParameterIn::Header, required, description);
    }

    /// A request cookie parameter (for `ExtractorDoc` impls).
    pub fn cookie_param<T: ToSchema>(
        &mut self,
        op: &mut Operation,
        name: &str,
        required: bool,
        description: Option<&str>,
    ) {
        self.param::<T>(op, name, ParameterIn::Cookie, required, description);
    }

    /// Require a security scheme for this operation and register the scheme in
    /// `components.securitySchemes` (for `ExtractorDoc` impls on auth extractors).
    /// Several auth extractors on one handler are combined into one requirement: all must pass.
    pub fn security(&mut self, op: &mut Operation, name: &str, scopes: &[&str], scheme: SecurityScheme) {
        // Every auth extractor on a handler must succeed, so they're one requirement
        // (AND), not alternatives (OR). The same scheme twice unions its scopes
        // (`read` + `write` → both), rather than the last call winning.
        self.security_scopes.entry(name.to_owned()).or_default().extend(scopes.iter().map(|s| (*s).to_owned()));
        let mut schemes = self.security_scopes.iter();
        let (first, first_scopes) = schemes.next().expect("just inserted");
        let requirement = schemes
            .fold(SecurityRequirement::new(first, first_scopes.iter().cloned()), |req, (n, sc)| {
                req.add(n, sc.iter().cloned())
            });
        let reqs = op.security.get_or_insert_default();
        match reqs.first_mut() {
            Some(req) => *req = requirement,
            None => reqs.push(requirement),
        }
        self.security_schemes.push((name.to_owned(), scheme));
    }

    /// Parameters from the properties of a `ToSchema` struct, so `Query<T>` / `Path<T>`
    /// work without also deriving `IntoParams`.
    pub fn params_from_object<T: ToSchema>(&mut self, op: &mut Operation, location: ParameterIn) {
        T::schemas(&mut self.schemas);
        let Some((properties, required)) = object_fields(&self.schemas, &T::schema()) else {
            self.error(format!("`{}` must be a struct with named fields to be used as parameters", T::name()));
            return;
        };
        for (name, schema) in properties {
            let required = location == ParameterIn::Path || required.contains(&name);
            let p: Parameter = ParameterBuilder::new()
                .name(name)
                .parameter_in(location.clone())
                .required(if required { Required::True } else { Required::False })
                .schema(Some(schema))
                .build();
            op.parameters.get_or_insert_default().push(RefOr::T(p));
        }
    }

    /// Parameters from an `IntoParams` struct. `location` is applied unless the struct
    /// sets `#[into_params(parameter_in = ..)]`, so you don't have to.
    pub fn params<T: IntoParams>(&mut self, op: &mut Operation, location: ParameterIn) {
        let params = T::into_params(|| Some(location.clone()));
        op.parameters.get_or_insert_default().extend(params.into_iter().map(RefOr::T));
    }

    /// A single named path parameter of schema `T` (e.g. `Path<Uuid>` on `/todos/{id}`).
    pub fn path_param<T: ToSchema>(&mut self, op: &mut Operation, name: &str) {
        let schema = self.schema::<T>();
        self.push_path_param(op, name, schema);
    }

    fn body<T: ToSchema>(&mut self, op: &mut Operation, content_type: &str) {
        if op.request_body.is_some() {
            self.error("more than one request body extractor");
        }
        let schema = self.schema::<T>();
        let body = RequestBodyBuilder::new()
            .content(content_type, Content::new(Some(schema)))
            .required(Some(Required::True))
            .build();
        op.request_body = Some(RefOr::T(body));
    }

    pub fn json_body<T: ToSchema>(&mut self, op: &mut Operation) {
        self.body::<T>(op, "application/json");
    }

    pub fn form_body<T: ToSchema>(&mut self, op: &mut Operation) {
        self.body::<T>(op, "application/x-www-form-urlencoded");
    }
}

/// Properties and required names of an object schema, looking through `$ref`s to
/// `known` components and `allOf` (what `#[serde(flatten)]` produces).
type NamedSchemas = Vec<(String, RefOr<Schema>)>;

fn object_fields(known: &[(String, RefOr<Schema>)], schema: &RefOr<Schema>) -> Option<(NamedSchemas, Vec<String>)> {
    match schema {
        RefOr::Ref(r) => {
            let name = r.ref_location.rsplit('/').next()?;
            let (_, target) = known.iter().find(|(n, _)| n == name)?;
            object_fields(known, target)
        }
        RefOr::T(Schema::Object(o)) if !o.properties.is_empty() => {
            Some((o.properties.iter().map(|(k, v)| (k.clone(), v.clone())).collect(), o.required.clone()))
        }
        RefOr::T(Schema::AllOf(all)) => {
            let (mut props, mut required) = (Vec::new(), Vec::new());
            for part in &all.items {
                let (p, r) = object_fields(known, part)?;
                props.extend(p);
                required.extend(r);
            }
            (!props.is_empty()).then_some((props, required))
        }
        _ => None,
    }
}

/// Schema for a single path value whose type isn't a utoipa `ToSchema` (utoipa only
/// knows `Uuid`, chrono types etc. inside its derives). Implement it for your own
/// newtypes to use them as `Path<MyId>`.
pub trait PathValue {
    fn schema() -> RefOr<Schema>;
}

#[cfg(any(feature = "uuid", feature = "chrono"))]
fn string_format(format: KnownFormat) -> RefOr<Schema> {
    ObjectBuilder::new().schema_type(Type::String).format(Some(SchemaFormat::KnownFormat(format))).into()
}

#[cfg(feature = "uuid")]
impl PathValue for uuid::Uuid {
    fn schema() -> RefOr<Schema> {
        string_format(KnownFormat::Uuid)
    }
}

#[cfg(feature = "chrono")]
impl<Tz: chrono::TimeZone> PathValue for chrono::DateTime<Tz> {
    fn schema() -> RefOr<Schema> {
        string_format(KnownFormat::DateTime)
    }
}

#[cfg(feature = "chrono")]
impl PathValue for chrono::NaiveDate {
    fn schema() -> RefOr<Schema> {
        string_format(KnownFormat::Date)
    }
}

impl DocCtx {
    fn push_path_param(&mut self, op: &mut Operation, name: &str, schema: RefOr<Schema>) {
        let p: Parameter = ParameterBuilder::new()
            .name(name)
            .parameter_in(ParameterIn::Path)
            .required(Required::True)
            .schema(Some(schema))
            .build();
        op.parameters.get_or_insert_default().push(RefOr::T(p));
    }

    /// A single named path parameter of a [`PathValue`] type.
    pub fn path_value<T: PathValue>(&mut self, op: &mut Operation, name: &str) {
        self.push_path_param(op, name, T::schema());
    }
}

fn single<'a>(ctx: &mut DocCtx, names: &'a [&'a str], ty: &str) -> Option<&'a str> {
    match names {
        [name] => Some(name),
        _ => {
            ctx.error(format!(
                "`Path<{ty}>` extracts one value but the path template has {} parameters {names:?}; use a tuple or an IntoParams struct",
                names.len()
            ));
            None
        }
    }
}

// Autoref specialization so `Path<T>` works for `IntoParams` structs (`Path<TodoId>`),
// `ToSchema` values (`Path<u32>`, `Path<MyEnum>`) and `PathValue`s (`Path<Uuid>`);
// the macro can't tell them apart. Called as `(&&&PhantomData::<T>).method()`, method
// probing tries `&&PhantomData` impls, then `&PhantomData`, then `PhantomData`.
#[doc(hidden)]
pub trait ViaParams {
    fn document_path(&self, ctx: &mut DocCtx, op: &mut Operation, names: &[&str]);
    fn document_query(&self, ctx: &mut DocCtx, op: &mut Operation);
}

impl<T: IntoParams> ViaParams for &&PhantomData<T> {
    fn document_path(&self, ctx: &mut DocCtx, op: &mut Operation, _names: &[&str]) {
        ctx.params::<T>(op, ParameterIn::Path);
    }
    fn document_query(&self, ctx: &mut DocCtx, op: &mut Operation) {
        ctx.params::<T>(op, ParameterIn::Query);
    }
}

#[doc(hidden)]
#[diagnostic::on_unimplemented(
    message = "`{Self}` can't be documented as query parameters",
    note = "derive `utoipa::ToSchema` (or `IntoParams`) on the query struct"
)]
pub trait ViaSchema {
    fn document_path(&self, ctx: &mut DocCtx, op: &mut Operation, names: &[&str]);
    fn document_query(&self, ctx: &mut DocCtx, op: &mut Operation);
}

impl<T: ToSchema> ViaSchema for &PhantomData<T> {
    fn document_path(&self, ctx: &mut DocCtx, op: &mut Operation, names: &[&str]) {
        // A struct maps its fields onto the template; anything else is one value.
        let mut known = Vec::new();
        T::schemas(&mut known);
        let is_struct = object_fields(&known, &T::schema()).is_some();
        if is_struct {
            ctx.params_from_object::<T>(op, ParameterIn::Path);
        } else if let Some(name) = single(ctx, names, &T::name()) {
            ctx.path_param::<T>(op, name);
        }
    }
    fn document_query(&self, ctx: &mut DocCtx, op: &mut Operation) {
        ctx.params_from_object::<T>(op, ParameterIn::Query);
    }
}

#[doc(hidden)]
#[diagnostic::on_unimplemented(
    message = "`{Self}` can't be documented as a path parameter",
    note = "use an `IntoParams` struct, a `ToSchema` type, enable the `uuid`/`chrono` feature, or implement `openapi_axum::PathValue`"
)]
pub trait ViaValue {
    fn document_path(&self, ctx: &mut DocCtx, op: &mut Operation, names: &[&str]);
}

impl<T: PathValue> ViaValue for PhantomData<T> {
    fn document_path(&self, ctx: &mut DocCtx, op: &mut Operation, names: &[&str]) {
        if let Some(name) = single(ctx, names, std::any::type_name::<T>()) {
            ctx.path_value::<T>(op, name);
        }
    }
}

/// utoipa's `Vec<T>` inlines `T` in full and doesn't register it. Once every route is
/// known, swap inline array items that are identical to a registered component for a
/// `$ref`, so generated clients keep the model name. Purely structural: no name guessing.
pub(crate) fn reference_components(openapi: &mut utoipa::openapi::OpenApi) {
    use utoipa::openapi::schema::ArrayItems;
    let Some(components) = &openapi.components else { return };
    let known: Vec<(String, RefOr<Schema>)> = components.schemas.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let fix = |schema: &mut RefOr<Schema>| {
        if let RefOr::T(Schema::Array(array)) = schema
            && let ArrayItems::RefOrSchema(items) = &mut array.items
            && let Some((name, _)) = known.iter().find(|(_, k)| k == items.as_ref())
        {
            **items = RefOr::Ref(Ref::from_schema_name(name.clone()));
        }
    };
    for item in openapi.paths.paths.values_mut() {
        let ops = [&mut item.get, &mut item.put, &mut item.post, &mut item.delete, &mut item.patch, &mut item.head];
        for op in ops.into_iter().flatten() {
            let bodies = op.request_body.iter_mut().filter_map(|b| match b {
                RefOr::T(b) => Some(&mut b.content),
                RefOr::Ref(_) => None,
            });
            for content in bodies {
                content.values_mut().for_each(|c| {
                    if let RefOr::T(c) = c {
                        c.schema.iter_mut().for_each(fix)
                    }
                });
            }
            for r in op.responses.responses.values_mut() {
                if let RefOr::T(r) = r {
                    r.content.values_mut().for_each(|c| {
                        if let RefOr::T(c) = c {
                            c.schema.iter_mut().for_each(fix)
                        }
                    });
                }
            }
        }
    }
}

/// Document a custom extractor (auth, headers, cookies...). Implement it for your
/// extractor and every `#[route]` handler that takes it is documented automatically:
/// the docs follow the extractor the handler *actually* uses.
///
/// ```
/// # use openapi_axum::{DocCtx, ExtractorDoc};
/// # use utoipa::openapi::{path::Operation, security::{HttpAuthScheme, HttpBuilder, SecurityScheme}};
/// struct Bearer(String);
///
/// impl ExtractorDoc for Bearer {
///     fn document(ctx: &mut DocCtx, op: &mut Operation) {
///         let scheme = SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build());
///         ctx.security(op, "bearer", &[], scheme);
///         ctx.empty_response(&mut op.responses, 401, Some("Missing or invalid token"));
///     }
/// }
/// ```
pub trait ExtractorDoc {
    fn document(ctx: &mut DocCtx, op: &mut Operation);
}

#[doc(hidden)]
pub trait ViaExtractorDoc {
    fn document_extractor(&self, ctx: &mut DocCtx, op: &mut Operation);
}

impl<T: ExtractorDoc> ViaExtractorDoc for &PhantomData<T> {
    fn document_extractor(&self, ctx: &mut DocCtx, op: &mut Operation) {
        T::document(ctx, op);
    }
}

#[doc(hidden)]
pub trait NoExtractorDoc {
    fn document_extractor(&self, _ctx: &mut DocCtx, _op: &mut Operation) {}
}

impl<T> NoExtractorDoc for PhantomData<T> {}

/// Registers element types seen in signatures (`Vec<Pet>` → `Pet`) when they're `ToSchema`.
#[doc(hidden)]
pub trait ViaRegister {
    fn register_schema(&self, ctx: &mut DocCtx);
}

impl<T: ToSchema> ViaRegister for &PhantomData<T> {
    fn register_schema(&self, ctx: &mut DocCtx) {
        ctx.register::<T>();
    }
}

#[doc(hidden)]
pub trait NoRegister {
    fn register_schema(&self, _ctx: &mut DocCtx) {}
}

impl<T> NoRegister for PhantomData<T> {}

/// `IntoParams` doesn't expose its fields' types, so schemas they `$ref` (e.g. an enum
/// field) can't be registered from it. If the params struct also derives `ToSchema`,
/// its nested schemas are registered here; otherwise `split()` reports the dangling ref.
#[doc(hidden)]
pub trait ViaNested {
    fn register_nested(&self, ctx: &mut DocCtx);
}

impl<T: ToSchema> ViaNested for &PhantomData<T> {
    fn register_nested(&self, ctx: &mut DocCtx) {
        T::schemas(&mut ctx.schemas);
    }
}

#[doc(hidden)]
pub trait NoNested {
    fn register_nested(&self, _ctx: &mut DocCtx) {}
}

impl<T> NoNested for PhantomData<T> {}

/// Every `$ref` in the document that doesn't resolve, with where it's used.
///
/// Only reference-bearing positions are checked. Example/default/const/enum values and
/// `x-` extensions are user data, so a literal `{"$ref": ...}` inside them isn't a
/// reference. Name maps (`properties`, `paths`, `content`...) are walked by value so a
/// property that happens to be called `default` is still checked.
pub(crate) fn dangling_refs(openapi: &utoipa::openapi::OpenApi) -> Vec<(String, String)> {
    dangling_refs_in(&serde_json::to_value(openapi).expect("OpenAPI serializes"))
}

fn dangling_refs_in(root: &serde_json::Value) -> Vec<(String, String)> {
    use serde_json::Value;

    /// Keywords whose values are literal data, never references.
    const LITERAL: &[&str] = &["example", "default", "const", "enum", "value", "dataValue", "serializedValue"];
    /// Keywords whose values are maps from user-chosen names to objects.
    const NAME_MAPS: &[&str] = &[
        "paths",
        "webhooks",
        "properties",
        "patternProperties",
        "dependentSchemas",
        "$defs",
        "definitions",
        "schemas",
        "responses",
        "parameters",
        "requestBodies",
        "headers",
        "content",
        "encoding",
        "securitySchemes",
        "links",
        "callbacks",
        "pathItems",
        "mediaTypes",
    ];

    struct Walk<'a> {
        root: &'a Value,
        at: Vec<String>,
        out: Vec<(String, String)>,
    }

    impl Walk<'_> {
        /// `v` is an OpenAPI/JSON Schema object (or array of them): its keys are keywords.
        fn object(&mut self, v: &Value) {
            match v {
                Value::Array(items) => {
                    self.each(items.iter().enumerate().map(|(i, v)| (i.to_string(), v)), Self::object)
                }
                Value::Object(m) => {
                    if let Some(Value::String(r)) = m.get("$ref")
                        && r.starts_with('#')
                        && self.root.pointer(&r[1..]).is_none()
                    {
                        self.out.push((r.clone(), self.at.join(" ")));
                    }
                    for (k, child) in m {
                        if k.starts_with("x-") || LITERAL.contains(&k.as_str()) {
                            continue;
                        }
                        self.at.push(k.clone());
                        match (k.as_str(), child) {
                            // Schema `examples` is an array of literal values; elsewhere it's a
                            // name -> Example object map (whose `$ref` is real, `value` is data).
                            ("examples", Value::Array(_)) => {}
                            ("examples", _) => self.names(child),
                            (k, _) if NAME_MAPS.contains(&k) => self.names(child),
                            _ => self.object(child),
                        }
                        self.at.pop();
                    }
                }
                _ => {}
            }
        }

        /// `v` maps user-chosen names to objects: keys are names, values are objects.
        fn names(&mut self, v: &Value) {
            match v {
                Value::Object(m) => self.each(m.iter().map(|(k, v)| (k.clone(), v)), Self::object),
                other => self.object(other),
            }
        }

        fn each<'v>(&mut self, items: impl Iterator<Item = (String, &'v Value)>, f: fn(&mut Self, &Value)) {
            for (k, v) in items {
                self.at.push(k);
                f(self, v);
                self.at.pop();
            }
        }
    }

    let mut walk = Walk { root, at: Vec::new(), out: Vec::new() };
    walk.object(root);
    walk.out
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    fn dangling_check(spec: Value) -> Result<(), String> {
        let found = super::dangling_refs_in(&spec);
        if found.is_empty() { Ok(()) } else { Err(format!("{found:?}")) }
    }

    fn base_doc(extra_components: Value, path_item: Value) -> Value {
        json!({
            "openapi": "3.1.0",
            "info": { "title": "t", "version": "1" },
            "paths": { "/x": path_item },
            "components": extra_components,
        })
    }

    #[test]
    fn literal_refs_in_examples_defaults_and_extensions_are_ignored() {
        let literal = json!({ "data": { "$ref": "#/user-supplied-value" } });
        let schema = json!({
            "type": "object",
            "example": literal,
            "examples": [literal],
            "default": literal,
            "const": literal,
            "enum": [literal],
            "x-anything": literal,
            "properties": { "a": { "type": "string", "example": literal } },
        });
        let doc = base_doc(
            json!({
                "schemas": { "Thing": schema },
                "examples": { "ex": { "value": literal } },
            }),
            json!({ "get": {
                "parameters": [{ "name": "q", "in": "query", "schema": { "type": "string" }, "example": literal,
                                 "examples": { "one": { "value": literal } } }],
                "responses": { "200": { "description": "ok", "content": { "application/json": {
                    "schema": { "$ref": "#/components/schemas/Thing" },
                    "example": literal,
                    "examples": { "one": { "summary": "s", "value": literal }, "shared": { "$ref": "#/components/examples/ex" } },
                } } } },
                "x-extension": literal,
            } }),
        );
        assert_eq!(dangling_check(doc), Ok(()));
    }

    #[test]
    fn real_refs_are_still_checked_everywhere() {
        // A property literally named `default` / `example` is a schema, not a literal.
        for (label, components, path_item) in [
            (
                "property named default",
                json!({ "schemas": { "T": { "type": "object", "properties": { "default": { "$ref": "#/components/schemas/Gone" } } } } }),
                json!({ "get": { "responses": { "200": { "description": "ok" } } } }),
            ),
            (
                "property named example",
                json!({ "schemas": { "T": { "type": "object", "properties": { "example": { "$ref": "#/components/schemas/Gone" } } } } }),
                json!({ "get": { "responses": { "200": { "description": "ok" } } } }),
            ),
            (
                "example object reference",
                json!({}),
                json!({ "get": { "responses": { "200": { "description": "ok", "content": { "application/json": {
                    "examples": { "shared": { "$ref": "#/components/schemas/Gone" } } } } } } } }),
            ),
            (
                "items",
                json!({}),
                json!({ "get": { "responses": { "200": { "description": "ok", "content": { "application/json": {
                    "schema": { "type": "array", "items": { "$ref": "#/components/schemas/Gone" } } } } } } } }),
            ),
            (
                "allOf",
                json!({ "schemas": { "T": { "allOf": [{ "$ref": "#/components/schemas/Gone" }] } } }),
                json!({ "get": { "responses": { "200": { "description": "ok" } } } }),
            ),
            (
                "response reference",
                json!({}),
                json!({ "get": { "responses": { "404": { "$ref": "#/components/responses/Gone" } } } }),
            ),
        ] {
            let err = dangling_check(base_doc(components, path_item)).expect_err(label);
            assert!(err.contains("/Gone"), "{label}: {err}");
        }
    }
}
