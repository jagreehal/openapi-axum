use syn::{Attribute, Expr, ExprLit, Lit, Meta};

/// Lines of `///` doc comments, trimmed.
pub fn doc_lines(attrs: &[Attribute]) -> Vec<String> {
    attrs
        .iter()
        .filter(|a| a.path().is_ident("doc"))
        .filter_map(|a| match &a.meta {
            Meta::NameValue(nv) => match &nv.value {
                Expr::Lit(ExprLit { lit: Lit::Str(s), .. }) => Some(s.value().trim().to_owned()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// (summary, description): first line, then everything after it.
pub fn summary_description(attrs: &[Attribute]) -> (Option<String>, Option<String>) {
    let lines = doc_lines(attrs);
    let mut it = lines.into_iter().skip_while(String::is_empty);
    let summary = it.next();
    let rest: Vec<String> = it.collect();
    let description = rest.join("\n").trim().to_owned();
    (summary, (!description.is_empty()).then_some(description))
}

const COLLECTIONS: &[&str] =
    &["Vec", "VecDeque", "LinkedList", "HashSet", "BTreeSet", "IndexSet", "HashMap", "BTreeMap", "IndexMap"];
const MAPS: &[&str] = &["HashMap", "BTreeMap", "IndexMap"];

/// Element types of collections anywhere in `ty` (`Json<Vec<Pet>>` → `Pet`), so the
/// router can register them and turn inline array items into `$ref`s.
pub fn collection_elements(ty: &syn::Type, out: &mut Vec<syn::Type>) {
    use syn::{GenericArgument, PathArguments, Type};
    match ty {
        Type::Path(tp) => {
            for seg in &tp.path.segments {
                let PathArguments::AngleBracketed(args) = &seg.arguments else { continue };
                let name = seg.ident.to_string();
                let types = args.args.iter().filter_map(|a| match a {
                    GenericArgument::Type(t) => Some(t),
                    _ => None,
                });
                for (i, t) in types.enumerate() {
                    let is_element = COLLECTIONS.contains(&name.as_str()) && (!MAPS.contains(&name.as_str()) || i == 1);
                    if is_element {
                        out.push(t.clone());
                    }
                    collection_elements(t, out);
                }
            }
        }
        Type::Slice(s) => {
            out.push((*s.elem).clone());
            collection_elements(&s.elem, out);
        }
        Type::Array(a) => {
            out.push((*a.elem).clone());
            collection_elements(&a.elem, out);
        }
        Type::Reference(r) => collection_elements(&r.elem, out),
        Type::Paren(p) => collection_elements(&p.elem, out),
        Type::Tuple(t) => t.elems.iter().for_each(|e| collection_elements(e, out)),
        _ => {}
    }
}

/// `(&&PhantomData::<X>).register_schema(ctx);` for each collection element type.
pub fn register_hints(types: &[&syn::Type]) -> proc_macro2::TokenStream {
    let mut elements = Vec::new();
    for ty in types {
        collection_elements(ty, &mut elements);
    }
    quote::quote! {
        #({
            #[allow(unused_imports)]
            use ::openapi_axum::__private::{NoRegister as _, ViaRegister as _};
            (&&::core::marker::PhantomData::<#elements>).register_schema(ctx);
        })*
    }
}
