use proc_macro2::TokenStream;
use quote::{format_ident, quote, quote_spanned};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{Error, FnArg, GenericArgument, Ident, ItemFn, LitStr, PathArguments, ReturnType, Token, Type, bracketed};

use crate::util::summary_description;

const METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "TRACE"];

struct Args {
    method: Ident,
    path: LitStr,
    tags: Vec<LitStr>,
    operation_id: Option<LitStr>,
}

impl Parse for Args {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let method: Ident = input.parse()?;
        if !METHODS.contains(&method.to_string().as_str()) {
            return Err(Error::new(method.span(), format!("expected one of {}", METHODS.join(", "))));
        }
        input.parse::<Token![,]>()?;
        let path: LitStr = input.parse()?;
        let (mut tags, mut operation_id) = (Vec::new(), None);
        while !input.is_empty() {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "tag" => tags.push(input.parse()?),
                "tags" => {
                    let content;
                    bracketed!(content in input);
                    tags.extend(content.parse_terminated(<LitStr as Parse>::parse, Token![,])?);
                }
                "operation_id" => operation_id = Some(input.parse()?),
                _ => return Err(Error::new(key.span(), "expected `tag`, `tags` or `operation_id`")),
            }
        }
        Ok(Self { method, path, tags, operation_id })
    }
}

/// `{name}` / `{*name}` params, validated.
fn template_params(path: &LitStr) -> syn::Result<Vec<String>> {
    let value = path.value();
    if !value.starts_with('/') {
        return Err(Error::new(path.span(), "path must start with `/`"));
    }
    let mut names = Vec::new();
    for seg in value.split('/') {
        if let Some(name) = seg.strip_prefix(':') {
            return Err(Error::new(path.span(), format!("axum 0.8 uses `{{{name}}}`, not `{seg}`")));
        }
        if let Some(inner) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            let name = inner.trim_start_matches('*');
            if name.is_empty() || names.iter().any(|n| n == name) {
                return Err(Error::new(path.span(), format!("bad or duplicate path param `{seg}`")));
            }
            names.push(name.to_owned());
        } else if seg.contains('{') || seg.contains('}') {
            return Err(Error::new(path.span(), format!("path params must be whole segments: `{seg}`")));
        }
    }
    Ok(names)
}

/// `Json<T>` -> ("Json", T) for axum's own extractors: a bare name (`Json<T>`) or an
/// `axum::` path. Anything else (e.g. `axum_extra::extract::Query`) is a custom
/// extractor, documented through `ExtractorDoc` and keeping its own rejection.
fn extractor(ty: &Type) -> Option<(String, &Type)> {
    let Type::Path(tp) = ty else { return None };
    let segments = &tp.path.segments;
    if tp.qself.is_some() || (segments.len() > 1 && segments[0].ident != "axum") {
        return None;
    }
    let seg = segments.last()?;
    let PathArguments::AngleBracketed(args) = &seg.arguments else { return None };
    let inner = args.args.iter().find_map(|a| match a {
        GenericArgument::Type(t) => Some(t),
        _ => None,
    })?;
    Some((seg.ident.to_string(), inner))
}

fn is_json(ty: &Type) -> bool {
    matches!(ty, Type::Path(tp) if tp.path.segments.last().is_some_and(|s| s.ident == "Json"))
}

pub fn expand(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let args: Args = syn::parse2(attr)?;
    let mut f: ItemFn = syn::parse2(item)?;

    if f.sig.asyncness.is_none() {
        return Err(Error::new(f.sig.fn_token.span(), "#[route] handlers must be `async fn`"));
    }
    if !f.sig.generics.params.is_empty() {
        return Err(Error::new(f.sig.generics.span(), "#[route] handlers can't be generic"));
    }
    let ret = match &f.sig.output {
        ReturnType::Type(_, ty) if matches!(**ty, Type::ImplTrait(_)) => {
            return Err(Error::new(
                ty.span(),
                "return a concrete type (e.g. `Result<Json<T>, ApiError>`); the docs are derived from it",
            ));
        }
        ReturnType::Type(_, ty) => (**ty).clone(),
        ReturnType::Default => {
            return Err(Error::new(f.sig.span(), "add a return type; the response docs are derived from it"));
        }
    };

    let names = template_params(&args.path)?;
    let krate = quote!(::openapi_axum);
    let private = quote!(#krate::__private);

    let mut params = Vec::new(); // wrapper params
    let mut unwraps = Vec::new(); // rejection handling in the wrapper
    let mut call_args = Vec::new();
    let mut docs = Vec::new(); // operation documentation statements
    let mut bodies = 0;
    let mut guarded = false; // any Path/Query/GET-Form: validate params against the published doc
    let mut query_args: Vec<usize> = Vec::new(); // Query / GET Form: parse via ValidQuery

    for (i, input) in f.sig.inputs.iter_mut().enumerate() {
        let FnArg::Typed(pt) = input else {
            return Err(Error::new(input.span(), "#[route] handlers can't take `self`"));
        };
        let raw = pt.attrs.iter().any(|a| a.path().is_ident("raw"));
        pt.attrs.retain(|a| !a.path().is_ident("raw"));
        let ty = (*pt.ty).clone();
        let arg = format_ident!("__arg{i}");
        call_args.push(arg.clone());

        let rejection = match extractor(&ty) {
            Some((kind, inner)) => match kind.as_str() {
                // axum reads `Form<T>` from the query string on GET/HEAD.
                "Form" if matches!(args.method.to_string().as_str(), "GET" | "HEAD") => {
                    docs.push(quote_spanned! {inner.span()=> {
                        #[allow(unused_imports)]
                        use #private::{ViaParams as _, ViaSchema as _};
                        (&&&::core::marker::PhantomData::<#inner>).document_query(ctx, &mut op);
                        #[allow(unused_imports)]
                        use #private::{NoNested as _, ViaNested as _};
                        (&&::core::marker::PhantomData::<#inner>).register_nested(ctx);
                    }});
                    query_args.push(i);
                    Some((format_ident!("FormRejection"), false, inner.clone()))
                }
                "Json" | "Form" => {
                    bodies += 1;
                    if bodies > 1 {
                        return Err(Error::new(ty.span(), "only one body extractor (Json/Form) per handler"));
                    }
                    let method = if kind == "Json" { quote!(json_body) } else { quote!(form_body) };
                    docs.push(quote_spanned!(ty.span()=> ctx.#method::<#inner>(&mut op);));
                    Some((format_ident!("{kind}Rejection"), true, inner.clone()))
                }
                "Query" => {
                    docs.push(quote_spanned! {inner.span()=> {
                        #[allow(unused_imports)]
                        use #private::{ViaParams as _, ViaSchema as _};
                        (&&&::core::marker::PhantomData::<#inner>).document_query(ctx, &mut op);
                        #[allow(unused_imports)]
                        use #private::{NoNested as _, ViaNested as _};
                        (&&::core::marker::PhantomData::<#inner>).register_nested(ctx);
                    }});
                    query_args.push(i);
                    Some((format_ident!("QueryRejection"), false, inner.clone()))
                }
                "Path" => {
                    if let Type::Tuple(tuple) = inner {
                        // Compile-time check: tuple arity must match the template.
                        if tuple.elems.len() != names.len() {
                            return Err(Error::new(
                                inner.span(),
                                format!(
                                    "path `{}` has {} param(s) {names:?} but this tuple extracts {}",
                                    args.path.value(),
                                    names.len(),
                                    tuple.elems.len()
                                ),
                            ));
                        }
                        for (elem, name) in tuple.elems.iter().zip(&names) {
                            docs.push(quote_spanned! {elem.span()=> {
                                #[allow(unused_imports)]
                                use #private::{ViaParams as _, ViaSchema as _, ViaValue as _};
                                (&&&::core::marker::PhantomData::<#elem>).document_path(ctx, &mut op, &[#name]);
                            }});
                        }
                    } else {
                        docs.push(quote_spanned! {inner.span()=> {
                            #[allow(unused_imports)]
                            use #private::{ViaParams as _, ViaSchema as _, ViaValue as _};
                            (&&&::core::marker::PhantomData::<#inner>).document_path(ctx, &mut op, &[#(#names),*]);
                            #[allow(unused_imports)]
                            use #private::{NoNested as _, ViaNested as _};
                            (&&::core::marker::PhantomData::<#inner>).register_nested(ctx);
                        }});
                    }
                    Some((format_ident!("PathRejection"), false, inner.clone()))
                }
                _ => None,
            },
            None => None,
        };
        if rejection.is_none() {
            // Custom extractor (auth, headers...): documented if it implements ExtractorDoc.
            docs.push(quote_spanned! {ty.span()=> {
                #[allow(unused_imports)]
                use #private::{NoExtractorDoc as _, ViaExtractorDoc as _};
                (&&::core::marker::PhantomData::<#ty>).document_extractor(ctx, &mut op);
            }});
        }

        let reject = quote!(return #private::reject::<#ret>(::core::convert::Into::into(e)));
        match rejection.filter(|_| !raw) {
            // Json bodies go through ValidJson: validated against the published schema.
            Some((_, true, inner)) if is_json(&ty) => {
                params.push(quote!(#arg: ::core::result::Result<#private::ValidJson<#inner>, #krate::Rejection>));
                unwraps.push(quote_spanned! {ty.span()=>
                    let #arg = match #arg {
                        ::core::result::Result::Ok(v) => #private::axum::Json(v.0),
                        ::core::result::Result::Err(e) => #reject,
                    };
                });
            }
            // Form bodies (non-GET) go through ValidForm, same idea.
            Some((_, true, inner)) => {
                params.push(quote!(#arg: ::core::result::Result<#private::ValidForm<#inner>, #krate::Rejection>));
                unwraps.push(quote_spanned! {ty.span()=>
                    let #arg = match #arg {
                        ::core::result::Result::Ok(v) => #private::axum::Form(v.0),
                        ::core::result::Result::Err(e) => #reject,
                    };
                });
            }
            // Query / GET Form: ValidQuery parses like axum, plus repeated keys for array params.
            Some((_, false, inner)) if query_args.contains(&i) => {
                guarded = true;
                let wrap = match extractor(&ty).map(|(k, _)| k).as_deref() {
                    Some("Form") => quote!(#private::axum::Form),
                    _ => quote!(#private::axum::extract::Query),
                };
                params.push(quote!(#arg: ::core::result::Result<#private::ValidQuery<#inner>, #krate::Rejection>));
                unwraps.push(quote_spanned! {ty.span()=>
                    let #arg = match #arg {
                        ::core::result::Result::Ok(v) => #wrap(v.0),
                        ::core::result::Result::Err(e) => #reject,
                    };
                });
            }
            Some((rej, false, _)) => {
                guarded = true;
                params.push(quote!(#arg: ::core::result::Result<#ty, #private::axum::extract::rejection::#rej>));
                unwraps.push(quote_spanned! {ty.span()=>
                    let #arg = match #arg {
                        ::core::result::Result::Ok(v) => v,
                        ::core::result::Result::Err(e) => #reject,
                    };
                });
            }
            None => params.push(quote!(#arg: #ty)),
        }
    }

    // Validate query/path values against the published parameter schemas first.
    if guarded {
        params.insert(0, quote!(__params: ::core::result::Result<#private::ParamsGuard, #krate::Rejection>));
        unwraps.insert(
            0,
            quote! {
                if let ::core::result::Result::Err(e) = __params {
                    return #private::reject::<#ret>(e);
                }
            },
        );
    }

    let arg_types: Vec<Type> = f
        .sig
        .inputs
        .iter()
        .filter_map(|a| match a {
            FnArg::Typed(pt) => Some((*pt.ty).clone()),
            FnArg::Receiver(_) => None,
        })
        .collect();
    let mut hint_types: Vec<&Type> = arg_types.iter().collect();
    hint_types.push(&ret);
    let hints = crate::util::register_hints(&hint_types);

    let name = &f.sig.ident;
    let vis = &f.vis;
    let method = &args.method;
    let path = &args.path;
    let operation_id = args.operation_id.map_or_else(|| name.to_string(), |l| l.value());
    let (summary, description) = summary_description(&f.attrs);
    let summary = summary.map(|s| quote!(op.summary = ::core::option::Option::Some(<::std::string::String as ::core::convert::From<&str>>::from(#s));));
    let description = description.map(|d| quote!(op.description = ::core::option::Option::Some(<::std::string::String as ::core::convert::From<&str>>::from(#d));));
    let tags = &args.tags;
    let tags = (!tags.is_empty()).then(|| quote!(op.tags = ::core::option::Option::Some(::std::vec![#(<::std::string::String as ::core::convert::From<&str>>::from(#tags)),*]);));
    let deprecated =
        f.attrs.iter().any(|a| a.path().is_ident("deprecated")).then(
            || quote!(op.deprecated = ::core::option::Option::Some(#private::utoipa::openapi::Deprecated::True);),
        );

    Ok(quote! {
        #f

        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        #vis struct #name {}

        #[allow(deprecated)]
        impl #name {
            /// The axum handler registered by `routes!`: maps extractor rejections into
            /// the return type, then calls the fn above.
            #[doc(hidden)]
            pub async fn handler(#(#params),*) -> #ret {
                #(#unwraps)*
                #private::instrument(#operation_id, #path, #name(#(#call_args),*)).await
            }
        }

        impl #private::Route for #name {
            const METHOD: #private::Method = #private::Method::#method;
            const PATH: &'static str = #path;
            const OPERATION_ID: &'static str = #operation_id;

            fn operation(ctx: &mut #krate::DocCtx) -> #private::utoipa::openapi::path::Operation {
                #[allow(unused_imports)]
                use #private::utoipa::openapi::path::ParameterIn;
                let mut op = <#private::utoipa::openapi::path::Operation as ::core::default::Default>::default();
                op.operation_id = ::core::option::Option::Some(<::std::string::String as ::core::convert::From<&str>>::from(#operation_id));
                #summary
                #description
                #tags
                #deprecated
                #(#docs)*
                #hints
                <#ret as #krate::ApiResponse>::document(ctx, &mut op.responses);
                op
            }
        }
    })
}
