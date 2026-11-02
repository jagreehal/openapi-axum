//! Requests are validated against the **same schemas that are published**: JSON and
//! form bodies against their body schema, query and path values against the
//! documented parameter schemas. A constraint in the docs is a constraint the server
//! enforces, with no second set of validation attributes.

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::StatusCode;
use axum::http::request::Parts;
use serde::de::DeserializeOwned;
use utoipa::ToSchema;

use crate::{Rejection, RejectionKind};

fn invalid(kind: RejectionKind, message: String) -> Rejection {
    Rejection { kind, status: StatusCode::UNPROCESSABLE_ENTITY, message }
}

/// The published document for this request's operation, when the router came from
/// `ApiRouter::split`. Bodies are validated against *that* (so `map_openapi` edits are
/// enforced too); handlers mounted some other way fall back to `T`'s own schema.
#[cfg(feature = "validation")]
fn published(req: &Request) -> Option<(std::sync::Arc<crate::spec_index::SpecIndex>, axum::http::Method, String)> {
    let index = req.extensions().get::<std::sync::Arc<crate::spec_index::SpecIndex>>()?.clone();
    let path = req.extensions().get::<axum::extract::MatchedPath>()?.as_str().to_owned();
    Some((index, req.method().clone(), path))
}

/// The extractor `#[route]` uses for `Json<T>` arguments. Hands your handler an `axum::Json<T>`.
#[doc(hidden)]
pub struct ValidJson<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned + ToSchema + 'static> FromRequest<S> for ValidJson<T> {
    type Rejection = Rejection;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        #[cfg(feature = "validation")]
        let published = published(&req);
        let axum::Json(value) = axum::Json::<serde_json::Value>::from_request(req, state).await?;
        #[cfg(feature = "validation")]
        {
            let errors = match &published {
                Some((index, method, path)) => match index.body(method, path, "application/json") {
                    Some(body) => {
                        crate::spec_index::describe_errors(&body.validator, &value, |f| format!("field `{f}`"))
                    }
                    None => Vec::new(),
                },
                None => {
                    let (validator, _) = body::schema_for::<T>();
                    crate::spec_index::describe_errors(&validator, &value, |f| format!("field `{f}`"))
                }
            };
            if !errors.is_empty() {
                return Err(invalid(RejectionKind::Json, errors.join("; ")));
            }
        }
        serde_json::from_value(value).map(ValidJson).map_err(|e| invalid(RejectionKind::Json, e.to_string()))
    }
}

/// The extractor `#[route]` uses for `Form<T>` bodies (POST/PUT/...). Hands your
/// handler an `axum::Form<T>`, deserialized exactly as axum would.
#[doc(hidden)]
pub struct ValidForm<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned + ToSchema + 'static> FromRequest<S> for ValidForm<T> {
    type Rejection = Rejection;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        #[cfg(feature = "validation")]
        let published = published(&req);
        let axum::Form(pairs) = axum::Form::<Vec<(String, String)>>::from_request(req, state).await?;
        #[cfg(feature = "validation")]
        let coerced = {
            let form = "application/x-www-form-urlencoded";
            let (validator, instance) = match &published {
                Some((index, method, path)) => match index.body(method, path, form) {
                    Some(body) => {
                        (Some(&body.validator), crate::spec_index::coerce_object(index.root(), &body.schema, &pairs))
                    }
                    None => (None, serde_json::Value::Null),
                },
                None => {
                    let (validator, root) = body::schema_for::<T>();
                    let instance = crate::spec_index::coerce_object(&root, &root["__body"], &pairs);
                    let errors = crate::spec_index::describe_errors(&validator, &instance, |f| format!("field `{f}`"));
                    if !errors.is_empty() {
                        return Err(invalid(RejectionKind::Form, errors.join("; ")));
                    }
                    (None, instance)
                }
            };
            if let Some(validator) = validator {
                let errors = crate::spec_index::describe_errors(validator, &instance, |f| format!("field `{f}`"));
                if !errors.is_empty() {
                    return Err(invalid(RejectionKind::Form, errors.join("; ")));
                }
            }
            instance
        };
        // Deserialize from the original pairs so semantics match `axum::Form` exactly...
        let encoded = serde_urlencoded::to_string(&pairs).map_err(|e| invalid(RejectionKind::Form, e.to_string()))?;
        let err = match serde_urlencoded::from_str::<T>(&encoded) {
            Ok(v) => return Ok(Self(v)),
            Err(e) => e,
        };
        // ...falling back to the schema-coerced value where serde_urlencoded can't
        // (`#[serde(flatten)]` buffers values as strings, so `u32` fields fail).
        #[cfg(feature = "validation")]
        if let Ok(v) = serde_json::from_value::<T>(coerced) {
            return Ok(Self(v));
        }
        Err(invalid(RejectionKind::Form, err.to_string()))
    }
}

/// The extractor `#[route]` uses for `Query<T>` and GET `Form<T>`. Deserializes exactly
/// like axum first; if that fails (axum's `Query` can't parse repeated keys like
/// `?tag=a&tag=b`, which the docs promise for array params), it deserializes the value
/// coerced from the published parameter schemas instead.
#[doc(hidden)]
pub struct ValidQuery<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for ValidQuery<T> {
    type Rejection = Rejection;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let raw = parts.uri.query().unwrap_or_default();
        let err = match serde_urlencoded::from_str::<T>(raw) {
            Ok(v) => return Ok(Self(v)),
            Err(e) => e,
        };
        #[cfg(feature = "validation")]
        {
            use axum::extract::MatchedPath;
            let index = parts.extensions.get::<std::sync::Arc<crate::spec_index::SpecIndex>>();
            let path = parts.extensions.get::<MatchedPath>().map(MatchedPath::as_str);
            let pairs: Vec<(String, String)> = serde_urlencoded::from_str(raw).unwrap_or_default();
            if let (Some(index), Some(path)) = (index, path)
                && let Some(coerced) = index.coerce_query(&parts.method, path, &pairs)
                && let Ok(v) = serde_json::from_value::<T>(coerced)
            {
                return Ok(Self(v));
            }
        }
        Err(Rejection {
            kind: RejectionKind::Query,
            status: StatusCode::BAD_REQUEST,
            message: format!("Failed to deserialize query string: {err}"),
        })
    }
}

/// Validates query and path values against the documented parameter schemas of the
/// matched operation, before the typed `Query`/`Path` extractors run. `#[route]` adds
/// it to handlers that take `Path`, `Query` or a GET `Form`.
#[doc(hidden)]
pub struct ParamsGuard;

impl<S: Send + Sync> FromRequestParts<S> for ParamsGuard {
    type Rejection = Rejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        #[cfg(feature = "validation")]
        {
            use axum::extract::{MatchedPath, RawPathParams};
            // Present when the router came from `ApiRouter::split`; a handler mounted
            // some other way is not validated (there's no published document to use).
            let Some(index) = parts.extensions.get::<std::sync::Arc<crate::spec_index::SpecIndex>>().cloned() else {
                return Ok(Self);
            };
            let Some(path) = parts.extensions.get::<MatchedPath>().map(|p| p.as_str().to_owned()) else {
                return Ok(Self);
            };
            let query: Vec<(String, String)> =
                serde_urlencoded::from_str(parts.uri.query().unwrap_or_default()).unwrap_or_default();
            let raw = RawPathParams::from_request_parts(parts, state).await?;
            let path_params: Vec<(String, String)> = raw.iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
            index.check_params(&parts.method, &path, &query, &path_params).map_err(|(kind, message)| Rejection {
                kind,
                status: StatusCode::BAD_REQUEST,
                message,
            })?;
        }
        #[cfg(not(feature = "validation"))]
        let _ = (parts, state);
        Ok(Self)
    }
}

#[cfg(feature = "validation")]
mod body {
    use std::any::TypeId;
    use std::collections::HashMap;
    use std::sync::{Arc, OnceLock, PoisonError, RwLock};

    use serde_json::Value;
    use utoipa::ToSchema;

    type Compiled = (Arc<jsonschema::Validator>, Arc<Value>);

    /// One compiled validator per body type, plus a root (components + the body schema
    /// under `__body`) for resolving refs during form coercion.
    // ponytail: cached for the process lifetime, keyed by TypeId.
    pub(super) fn schema_for<T: ToSchema + 'static>() -> Compiled {
        static CACHE: OnceLock<RwLock<HashMap<TypeId, Compiled>>> = OnceLock::new();
        let cache = CACHE.get_or_init(Default::default);
        if let Some(hit) = cache.read().unwrap_or_else(PoisonError::into_inner).get(&TypeId::of::<T>()) {
            return hit.clone();
        }
        let mut schemas = Vec::new();
        T::schemas(&mut schemas);
        let components: serde_json::Map<String, Value> =
            schemas.into_iter().map(|(n, s)| (n, serde_json::to_value(s).expect("schema serializes"))).collect();
        let body = serde_json::to_value(T::schema()).expect("schema serializes");
        let root = serde_json::json!({ "components": { "schemas": components }, "__body": body });
        let validator = crate::spec_index::compile(&root, &root["__body"]);
        let compiled = (Arc::new(validator), Arc::new(root));
        cache.write().unwrap_or_else(PoisonError::into_inner).insert(TypeId::of::<T>(), compiled.clone());
        compiled
    }
}
