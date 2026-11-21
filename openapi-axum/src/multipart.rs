//! `multipart/form-data` bodies (feature `multipart`).
//!
//! [`Multipart<T>`] reads the parts into `T`, a struct that derives `Deserialize` and
//! `ToSchema`. The published schema decides how each part is read: a binary string
//! property ([`FilePart`], `Option<FilePart>`, `Vec<FilePart>`) is a file part, and every
//! other property is UTF-8 text coerced like a form value (`"5"` → `5`, repeated parts →
//! an array). The parts are validated against the published body schema before `T` is
//! built, the same way [`Json`](axum::Json) and [`Form`](axum::Form) bodies are.

use axum::body::Bytes;
use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;
use serde::de::value::{MapDeserializer, SeqDeserializer, StrDeserializer};
use serde::de::{self, DeserializeOwned, Deserializer, IntoDeserializer, MapAccess, Visitor};
use serde_json::Value;
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::{KnownFormat, ObjectBuilder, Schema, SchemaFormat, Type};
use utoipa::{PartialSchema, ToSchema};

use crate::spec_index::{PartKind, describe_errors, multipart_instance, part_kind};
use crate::validation::{body, invalid, published};
use crate::{Rejection, RejectionKind};

/// A `multipart/form-data` request body. Use it as a handler argument like
/// [`Json`](axum::Json): `Multipart(upload): Multipart<Upload>`.
///
/// ```
/// use axum::Json;
/// use openapi_axum::{ApiResponse, FilePart, Multipart, Rejection, route};
/// use serde::{Deserialize, Serialize};
/// use utoipa::ToSchema;
///
/// # #[derive(Serialize, ToSchema)]
/// # struct Problem { message: String }
/// # #[derive(ApiResponse)]
/// # enum ApiError {
/// #     #[status(BAD_REQUEST)]
/// #     BadRequest(Problem),
/// # }
/// # impl From<Rejection> for ApiError {
/// #     fn from(r: Rejection) -> Self { Self::BadRequest(Problem { message: r.message }) }
/// # }
/// #[derive(Deserialize, ToSchema)]
/// struct Upload {
///     /// The statement CSV.
///     file: FilePart,
///     #[schema(max_length = 100)]
///     note: Option<String>,
/// }
///
/// #[route(POST, "/files")]
/// async fn upload(Multipart(upload): Multipart<Upload>) -> Result<Json<usize>, ApiError> {
///     Ok(Json(upload.file.bytes.len()))
/// }
/// ```
///
/// Rejections, with the status axum would use: a request that isn't multipart or is
/// malformed (400), a body over the route's `DefaultBodyLimit` (413), a text part
/// that isn't UTF-8 (400), and parts that break the schema (422).
#[derive(Debug, Clone)]
pub struct Multipart<T>(pub T);

/// One uploaded file. As a `ToSchema` field it's documented as a binary string, so the
/// part is read as a file.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct FilePart {
    /// The part's `filename`, as the client sent it. Treat it as untrusted.
    pub filename: Option<String>,
    /// The part's `Content-Type`, as the client sent it.
    pub content_type: Option<String>,
    pub bytes: Bytes,
}

impl std::fmt::Debug for FilePart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The contents may be user data: show only their size.
        f.debug_struct("FilePart")
            .field("filename", &self.filename)
            .field("content_type", &self.content_type)
            .field("len", &self.bytes.len())
            .finish()
    }
}

impl PartialSchema for FilePart {
    fn schema() -> RefOr<Schema> {
        ObjectBuilder::new()
            .schema_type(Type::String)
            .format(Some(SchemaFormat::KnownFormat(KnownFormat::Binary)))
            .content_media_type("application/octet-stream")
            .into()
    }
}

impl ToSchema for FilePart {}

impl<S: Send + Sync, T: DeserializeOwned + ToSchema + 'static> FromRequest<S> for Multipart<T> {
    type Rejection = Rejection;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let kind = RejectionKind::Multipart;
        // The published body, so `map_openapi` edits apply; `T`'s own schema for a
        // handler that isn't registered through `ApiRouter`.
        let published = published(req.extensions(), kind)?;
        let published = published.as_ref().and_then(|(index, key)| {
            index.body(&key.0, &key.1, "multipart/form-data").map(|body| (index.root(), body))
        });
        let fallback;
        let (root, schema, validator) = match published {
            Some((root, body)) => (root, &body.schema, &body.validator),
            None => {
                fallback = body::schema_for::<T>();
                (&*fallback.1, &fallback.1["__body"], &*fallback.0)
            }
        };

        let mut multipart = axum::extract::Multipart::from_request(req, state).await?;
        let mut text = Vec::new();
        let mut files: Vec<(String, Vec<FilePart>)> = Vec::new();
        while let Some(field) = multipart.next_field().await? {
            let Some(name) = field.name().map(str::to_owned) else { continue };
            if part_kind(root, schema, &name) == PartKind::Text {
                let bytes = field.bytes().await?;
                let value = String::from_utf8(bytes.into()).map_err(|_| Rejection {
                    kind,
                    status: StatusCode::BAD_REQUEST,
                    message: format!("field `{name}`: a text part must be UTF-8"),
                })?;
                text.push((name, value));
                continue;
            }
            let part = FilePart {
                filename: field.file_name().map(str::to_owned),
                content_type: field.content_type().map(str::to_owned),
                bytes: field.bytes().await?,
            };
            match files.iter_mut().find(|(n, _)| *n == name) {
                Some((_, parts)) => parts.push(part),
                None => files.push((name, vec![part])),
            }
        }

        let counts: Vec<(String, usize)> = files.iter().map(|(n, parts)| (n.clone(), parts.len())).collect();
        let instance = multipart_instance(root, schema, &text, &counts);
        let errors = describe_errors(validator, &instance, |f| format!("field `{f}`"));
        if !errors.is_empty() {
            return Err(invalid(kind, errors.join("; ")));
        }

        let mut fields: Vec<(String, Part)> = Vec::new();
        let Value::Object(values) = instance else { unreachable!("multipart_instance returns an object") };
        for (name, value) in values {
            let part = match files.iter().position(|(n, _)| *n == name) {
                Some(i) => {
                    let (_, mut parts) = files.swap_remove(i);
                    match part_kind(root, schema, &name) {
                        PartKind::File if parts.len() == 1 => Part::File(parts.pop().expect("one part")),
                        _ => Part::Files(parts),
                    }
                }
                None => Part::Text(value),
            };
            fields.push((name, part));
        }
        T::deserialize(MapDeserializer::<_, serde_json::Error>::new(fields.into_iter()))
            .map(Multipart)
            .map_err(|e| invalid(kind, e.to_string()))
    }
}

/// One property's parts, as a serde deserializer.
enum Part {
    Text(Value),
    File(FilePart),
    Files(Vec<FilePart>),
}

impl<'de> IntoDeserializer<'de, serde_json::Error> for Part {
    type Deserializer = Self;
    fn into_deserializer(self) -> Self {
        self
    }
}

macro_rules! forward_text {
    ($($method:ident $(($($arg:ident: $ty:ty),*))?;)*) => {$(
        fn $method<V: Visitor<'de>>(self, $($($arg: $ty,)*)? visitor: V) -> Result<V::Value, Self::Error> {
            match self {
                Self::Text(value) => value.$method($($($arg,)*)? visitor),
                other => other.deserialize_any(visitor),
            }
        }
    )*};
}

impl<'de> Deserializer<'de> for Part {
    type Error = serde_json::Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self {
            Self::Text(value) => value.deserialize_any(visitor),
            Self::File(part) => visitor.visit_map(FileAccess { part, next: 0 }),
            Self::Files(parts) => {
                let mut seq = SeqDeserializer::<_, serde_json::Error>::new(parts.into_iter().map(Self::File));
                let value = visitor.visit_seq(&mut seq)?;
                seq.end()?;
                Ok(value)
            }
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self {
            Self::Text(value) => value.deserialize_option(visitor),
            other => visitor.visit_some(other),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self {
            Self::Text(value) => value.deserialize_newtype_struct(name, visitor),
            other => visitor.visit_newtype_struct(other),
        }
    }

    forward_text! {
        deserialize_bool; deserialize_i8; deserialize_i16; deserialize_i32; deserialize_i64;
        deserialize_i128; deserialize_u8; deserialize_u16; deserialize_u32; deserialize_u64;
        deserialize_u128; deserialize_f32; deserialize_f64; deserialize_char; deserialize_str;
        deserialize_string; deserialize_bytes; deserialize_byte_buf; deserialize_unit;
        deserialize_unit_struct(name: &'static str);
        deserialize_seq; deserialize_tuple(len: usize);
        deserialize_tuple_struct(name: &'static str, len: usize);
        deserialize_map;
        deserialize_struct(name: &'static str, fields: &'static [&'static str]);
        deserialize_enum(name: &'static str, variants: &'static [&'static str]);
        deserialize_identifier; deserialize_ignored_any;
    }
}

/// A [`FilePart`] as a map of its three fields, read back by its `Deserialize` impl.
struct FileAccess {
    part: FilePart,
    next: usize,
}

const FILE_FIELDS: [&str; 3] = ["filename", "content_type", "bytes"];

impl<'de> MapAccess<'de> for FileAccess {
    type Error = serde_json::Error;

    fn next_key_seed<K: de::DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error> {
        let Some(key) = FILE_FIELDS.get(self.next) else { return Ok(None) };
        seed.deserialize(StrDeserializer::<Self::Error>::new(key)).map(Some)
    }

    fn next_value_seed<V: de::DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Self::Error> {
        self.next += 1;
        let text = |s: &mut Option<String>| s.take().map_or(Value::Null, Value::String);
        match self.next {
            1 => seed.deserialize(text(&mut self.part.filename)),
            2 => seed.deserialize(text(&mut self.part.content_type)),
            _ => seed.deserialize(BytesDeserializer(std::mem::take(&mut self.part.bytes))),
        }
    }
}

struct BytesDeserializer(Bytes);

impl<'de> Deserializer<'de> for BytesDeserializer {
    type Error = serde_json::Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_byte_buf(self.0.into())
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct seq tuple tuple_struct map struct enum
        identifier ignored_any
    }
}

impl<'de> serde::Deserialize<'de> for FilePart {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FileVisitor;

        impl<'de> Visitor<'de> for FileVisitor {
            type Value = FilePart;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a file part of a multipart body")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<FilePart, A::Error> {
                let mut part = FilePart::default();
                let mut bytes = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "filename" => part.filename = map.next_value()?,
                        "content_type" => part.content_type = map.next_value()?,
                        "bytes" => bytes = Some(map.next_value::<ByteBuf>()?.0),
                        _ => {
                            map.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }
                part.bytes = bytes.ok_or_else(|| de::Error::missing_field("bytes"))?;
                Ok(part)
            }
        }

        deserializer.deserialize_map(FileVisitor)
    }
}

/// The contents of a file part.
struct ByteBuf(Bytes);

impl<'de> serde::Deserialize<'de> for ByteBuf {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BytesVisitor;

        impl Visitor<'_> for BytesVisitor {
            type Value = ByteBuf;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("the bytes of a file part")
            }

            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<ByteBuf, E> {
                Ok(ByteBuf(Bytes::copy_from_slice(v)))
            }

            fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<ByteBuf, E> {
                Ok(ByteBuf(Bytes::from(v)))
            }
        }

        deserializer.deserialize_byte_buf(BytesVisitor)
    }
}
