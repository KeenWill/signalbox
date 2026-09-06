use std::collections::BTreeSet;

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{
    Data, DeriveInput, GenericArgument, Ident, LitStr, Member, PathArguments, Type, ext::IdentExt,
    spanned::Spanned,
};

#[derive(Clone, Copy)]
enum Mode {
    Ref,
    Copy,
    Clone,
    Str,
    Slice,
    Inner,
    OptRef,
    Unbox,
}

fn argument<'a>(ty: &'a Type, container: &str) -> Option<&'a Type> {
    let Type::Path(path) = ty else { return None };
    let segment = path.path.segments.last()?;
    if segment.ident != container {
        return None;
    }
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    arguments.args.iter().find_map(|argument| match argument {
        GenericArgument::Type(ty) => Some(ty),
        _ => None,
    })
}

pub(super) fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input,
            "Accessors requires a struct",
        ));
    };
    let mut methods = Vec::new();
    let mut names = BTreeSet::new();
    for (index, field) in data.fields.iter().enumerate() {
        let attrs = field
            .attrs
            .iter()
            .filter(|attr| attr.path().is_ident("get"))
            .collect::<Vec<_>>();
        if attrs.is_empty() {
            continue;
        }
        let mut mode = None;
        let mut name = field.ident.clone();
        let mut into = false;
        for attr in attrs {
            if matches!(attr.meta, syn::Meta::Path(_)) {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("as") {
                    let literal: LitStr = meta.value()?.parse()?;
                    let mut renamed: Ident = syn::parse_str(&literal.value())
                        .map_err(|error| syn::Error::new_spanned(&literal, error))?;
                    renamed.set_span(literal.span());
                    name = Some(renamed);
                } else if meta.path.is_ident("into") {
                    into = true;
                } else {
                    let selected = meta
                        .path
                        .get_ident()
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    let selected = match selected.as_str() {
                        "copy" => Mode::Copy,
                        "clone" => Mode::Clone,
                        "str" => Mode::Str,
                        "slice" => Mode::Slice,
                        "inner" => Mode::Inner,
                        "opt_ref" => Mode::OptRef,
                        "unbox" => Mode::Unbox,
                        _ => return Err(meta.error("unknown get mode")),
                    };
                    if mode.replace(selected).is_some() {
                        return Err(meta.error("only one get mode is allowed"));
                    }
                }
                Ok(())
            })?;
        }
        let name = name.ok_or_else(|| {
            syn::Error::new_spanned(field, "tuple field accessor requires as = \"name\"")
        })?;
        if !names.insert(name.unraw().to_string()) {
            return Err(syn::Error::new_spanned(&name, "duplicate accessor name"));
        }
        let member = field
            .ident
            .clone()
            .map(Member::Named)
            .unwrap_or_else(|| Member::Unnamed(syn::Index::from(index)));
        let ty = &field.ty;
        let docs = field
            .attrs
            .iter()
            .filter(|attr| attr.path().is_ident("doc"))
            .collect::<Vec<_>>();
        let method = match mode.unwrap_or(Mode::Ref) {
            Mode::Ref => quote!(pub const fn #name(&self) -> &#ty { &self.#member }),
            Mode::Copy => quote!(pub const fn #name(self) -> #ty { self.#member }),
            Mode::Clone => {
                quote!(pub fn #name(&self) -> #ty where #ty: ::std::clone::Clone { ::std::clone::Clone::clone(&self.#member) })
            }
            Mode::Str => {
                quote!(pub fn #name(&self) -> &str { ::std::convert::AsRef::<str>::as_ref(&self.#member) })
            }
            Mode::Slice => {
                let element = match ty {
                    Type::Slice(slice) => Some(slice.elem.as_ref()),
                    Type::Array(array) => Some(array.elem.as_ref()),
                    _ => argument(ty, "Vec").or_else(|| {
                        let Type::Slice(slice) =
                            argument(ty, "Box").or_else(|| argument(ty, "Arc"))?
                        else {
                            return None;
                        };
                        Some(slice.elem.as_ref())
                    }),
                }
                .ok_or_else(|| {
                    syn::Error::new_spanned(
                        ty,
                        "slice requires a slice, array, Vec, Box<[T]>, or Arc<[T]> field",
                    )
                })?;
                quote!(pub fn #name(&self) -> &[#element] { &self.#member })
            }
            Mode::Inner => {
                let primitive = if let Type::Path(path) = ty {
                    path.path.segments.last().and_then(|segment| {
                        segment
                            .ident
                            .to_string()
                            .strip_prefix("NonZero")
                            .map(str::to_lowercase)
                    })
                } else {
                    None
                };
                let primitive = primitive
                    .filter(|name| {
                        [
                            "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64",
                            "i128", "isize",
                        ]
                        .contains(&name.as_str())
                    })
                    .ok_or_else(|| {
                        syn::Error::new_spanned(ty, "inner requires a NonZero integer field")
                    })?;
                let primitive = Ident::new(&primitive, ty.span());
                quote!(pub const fn #name(self) -> #primitive { self.#member.get() })
            }
            Mode::OptRef => {
                let inner = argument(ty, "Option").ok_or_else(|| {
                    syn::Error::new_spanned(ty, "opt_ref requires an Option field")
                })?;
                quote!(pub fn #name(&self) -> ::std::option::Option<&#inner> { self.#member.as_ref() })
            }
            Mode::Unbox => {
                let inner = argument(ty, "Box")
                    .ok_or_else(|| syn::Error::new_spanned(ty, "unbox requires a Box field"))?;
                quote!(pub fn #name(&self) -> &#inner { &self.#member })
            }
        };
        methods.push(quote!(#(#docs)* #method));
        if into {
            let into_name = format_ident!("into_{}", name.unraw(), span = name.span());
            if !names.insert(into_name.to_string()) {
                return Err(syn::Error::new_spanned(&name, "duplicate accessor name"));
            }
            methods.push(quote!(#(#docs)* pub fn #into_name(self) -> #ty { self.#member }));
        }
    }
    let name = &input.ident;
    let (implementation, generics, clause) = input.generics.split_for_impl();
    Ok(quote!(impl #implementation #name #generics #clause { #(#methods)* }))
}
