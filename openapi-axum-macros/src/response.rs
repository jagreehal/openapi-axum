use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{Attribute, Data, DeriveInput, Error, Fields, Ident, Lit, LitStr, Type, WherePredicate, parse_quote};

use crate::util::{doc_lines, register_hints};

const NAMED: &[(&str, u16)] = &[
    ("OK", 200),
    ("CREATED", 201),
    ("ACCEPTED", 202),
    ("NO_CONTENT", 204),
    ("MOVED_PERMANENTLY", 301),
    ("FOUND", 302),
    ("SEE_OTHER", 303),
    ("NOT_MODIFIED", 304),
    ("TEMPORARY_REDIRECT", 307),
    ("PERMANENT_REDIRECT", 308),
    ("BAD_REQUEST", 400),
    ("UNAUTHORIZED", 401),
    ("PAYMENT_REQUIRED", 402),
    ("FORBIDDEN", 403),
    ("NOT_FOUND", 404),
    ("METHOD_NOT_ALLOWED", 405),
    ("NOT_ACCEPTABLE", 406),
    ("CONFLICT", 409),
    ("GONE", 410),
    ("PRECONDITION_FAILED", 412),
    ("PAYLOAD_TOO_LARGE", 413),
    ("UNSUPPORTED_MEDIA_TYPE", 415),
    ("UNPROCESSABLE_ENTITY", 422),
    ("TOO_MANY_REQUESTS", 429),
    ("INTERNAL_SERVER_ERROR", 500),
    ("NOT_IMPLEMENTED", 501),
    ("BAD_GATEWAY", 502),
    ("SERVICE_UNAVAILABLE", 503),
    ("GATEWAY_TIMEOUT", 504),
];

/// `#[status(404)]` or `#[status(NOT_FOUND)]`.
fn status(attrs: &[Attribute], span: proc_macro2::Span) -> syn::Result<u16> {
    let attr = attrs
        .iter()
        .find(|a| a.path().is_ident("status"))
        .ok_or_else(|| Error::new(span, "missing `#[status(..)]`, e.g. `#[status(404)]` or `#[status(NOT_FOUND)]`"))?;
    let code = match attr.parse_args::<Lit>() {
        Ok(Lit::Int(i)) => i.base10_parse::<u16>()?,
        Ok(other) => return Err(Error::new(other.span(), "expected a status code like 404")),
        Err(_) => {
            let ident: Ident = attr.parse_args()?;
            NAMED.iter().find(|(n, _)| ident == n).map(|(_, c)| *c).ok_or_else(|| {
                Error::new(ident.span(), "unknown status name; use the number instead, e.g. `#[status(418)]`")
            })?
        }
    };
    if !(100..=599).contains(&code) {
        return Err(Error::new(attr.span(), format!("{code} is not an HTTP status code")));
    }
    Ok(code)
}

/// `Option<T>` -> (T, optional = true).
fn unwrap_option(ty: &Type) -> (Type, bool) {
    if let Type::Path(tp) = ty
        && let Some(seg) = tp.path.segments.last()
        && seg.ident == "Option"
        && let syn::PathArguments::AngleBracketed(args) = &seg.arguments
        && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
    {
        return (inner.clone(), true);
    }
    (ty.clone(), false)
}

struct HeaderField {
    binding: Ident,
    name: LitStr,
    ty: Type,
    optional: bool,
}

/// One response shape: optional body plus typed headers.
struct Shape {
    /// Pattern fields, e.g. `(body)` or `{ location: __h0, todo: body }`.
    destructure: TokenStream,
    body: Option<Type>,
    headers: Vec<HeaderField>,
}

fn shape(fields: &Fields, code: u16) -> syn::Result<Shape> {
    let no_body = |span| Error::new(span, format!("status {code} can't have a body"));
    match fields {
        Fields::Unit => Ok(Shape { destructure: quote!(), body: None, headers: Vec::new() }),
        Fields::Unnamed(f) if f.unnamed.len() == 1 => {
            if matches!(code, 100..=199 | 204 | 304) {
                return Err(no_body(fields.span()));
            }
            Ok(Shape { destructure: quote!((body)), body: Some(f.unnamed[0].ty.clone()), headers: Vec::new() })
        }
        Fields::Named(named) => {
            let (mut body, mut headers, mut parts) = (None, Vec::new(), Vec::new());
            for (i, field) in named.named.iter().enumerate() {
                let ident = field.ident.clone().expect("named field");
                if field.attrs.iter().any(|a| a.path().is_ident("body")) {
                    if body.is_some() {
                        return Err(Error::new(ident.span(), "only one `#[body]` field"));
                    }
                    if matches!(code, 100..=199 | 204 | 304) {
                        return Err(no_body(ident.span()));
                    }
                    body = Some(field.ty.clone());
                    parts.push(quote!(#ident: body));
                } else if let Some(attr) = field.attrs.iter().find(|a| a.path().is_ident("header")) {
                    let name: LitStr = attr.parse_args()?;
                    let binding = format_ident!("__h{i}");
                    let (ty, optional) = unwrap_option(&field.ty);
                    parts.push(quote!(#ident: #binding));
                    headers.push(HeaderField { binding, name, ty, optional });
                } else {
                    return Err(Error::new(
                        ident.span(),
                        "mark each field `#[body]` or `#[header(\"Name\")]` so it's both sent and documented",
                    ));
                }
            }
            Ok(Shape { destructure: quote!({ #(#parts),* }), body, headers })
        }
        Fields::Unnamed(_) => Err(Error::new(
            fields.span(),
            "use a unit variant (no body), one unnamed body field `NotFound(ErrorBody)`, or named `#[body]`/`#[header]` fields",
        )),
    }
}

struct Case {
    pattern: TokenStream,
    code: u16,
    shape: Shape,
    description: Option<String>,
}

pub fn expand(item: TokenStream) -> syn::Result<TokenStream> {
    let input: DeriveInput = syn::parse2(item)?;
    let name = &input.ident;
    let describe = |attrs: &[Attribute]| {
        let d = doc_lines(attrs).join(" ").trim().to_owned();
        (!d.is_empty()).then_some(d)
    };

    let cases: Vec<Case> = match &input.data {
        Data::Enum(e) => {
            let mut seen = std::collections::BTreeMap::new();
            let mut cases = Vec::new();
            for v in &e.variants {
                let code = status(&v.attrs, v.ident.span())?;
                if let Some(prev) = seen.insert(code, v.ident.clone()) {
                    return Err(Error::new(
                        v.ident.span(),
                        format!("status {code} is already used by `{prev}`; OpenAPI allows one response per status"),
                    ));
                }
                let shape = shape(&v.fields, code)?;
                let (ident, d) = (&v.ident, &shape.destructure);
                cases.push(Case { pattern: quote!(Self::#ident #d), code, shape, description: describe(&v.attrs) });
            }
            cases
        }
        Data::Struct(s) => {
            let code = status(&input.attrs, name.span())?;
            let shape = shape(&s.fields, code)?;
            let d = &shape.destructure;
            vec![Case { pattern: quote!(Self #d), code, shape, description: describe(&input.attrs) }]
        }
        Data::Union(_) => return Err(Error::new(name.span(), "unions aren't supported")),
    };

    let private = quote!(::openapi_axum::__private);

    // Generic support: every body/header type gets the bounds it needs.
    let mut generics = input.generics.clone();
    {
        let where_clause = generics.make_where_clause();
        for c in &cases {
            if let Some(b) = &c.shape.body {
                let p: WherePredicate = parse_quote!(#b: #private::serde::Serialize + #private::utoipa::ToSchema);
                where_clause.predicates.push(p);
            }
            for h in &c.shape.headers {
                let ty = &h.ty;
                let p: WherePredicate = parse_quote!(#ty: ::core::fmt::Display + #private::utoipa::ToSchema);
                where_clause.predicates.push(p);
            }
        }
    }
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let arms = cases.iter().map(|c| {
        let (pattern, code) = (&c.pattern, c.code);
        let status = quote!(#private::axum::http::StatusCode::from_u16(#code).expect("validated at compile time"));
        let base = match c.shape.body {
            Some(_) => quote!((#status, #private::axum::Json(body)).into_response()),
            None => quote!(#status.into_response()),
        };
        let set_headers = c.shape.headers.iter().map(|h| {
            let (b, n) = (&h.binding, &h.name);
            if h.optional {
                quote!(if let ::core::option::Option::Some(v) = &#b { #private::set_header(&mut res, #n, v); })
            } else {
                quote!(#private::set_header(&mut res, #n, &#b);)
            }
        });
        quote!(#pattern => {
            #[allow(unused_mut)]
            let mut res = #base;
            #(#set_headers)*
            res
        })
    });

    let docs = cases.iter().map(|c| {
        let code = c.code;
        let description = match &c.description {
            Some(d) => quote!(::core::option::Option::Some(#d)),
            None => quote!(::core::option::Option::None),
        };
        let body = match &c.shape.body {
            Some(ty) => quote!(::core::option::Option::Some(ctx.schema::<#ty>())),
            None => quote!(::core::option::Option::None),
        };
        let headers = c.shape.headers.iter().map(|h| {
            let (n, ty) = (&h.name, &h.ty);
            quote!((#n, ctx.schema::<#ty>()))
        });
        let hint_types: Vec<&Type> = c.shape.body.iter().collect();
        let hints = register_hints(&hint_types);
        quote! {
            #hints
            let body = #body;
            let headers = ::std::vec![#(#headers),*];
            ctx.response(responses, #code, #description, body, headers);
        }
    });

    Ok(quote! {
        impl #impl_generics #private::axum::response::IntoResponse for #name #ty_generics #where_clause {
            fn into_response(self) -> #private::axum::response::Response {
                use #private::axum::response::IntoResponse as _;
                match self { #(#arms),* }
            }
        }

        impl #impl_generics ::openapi_axum::ApiResponse for #name #ty_generics #where_clause {
            fn document(ctx: &mut ::openapi_axum::DocCtx, responses: &mut #private::utoipa::openapi::Responses) {
                #(#docs)*
            }
        }
    })
}
