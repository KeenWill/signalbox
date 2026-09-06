//! Derives for repeated error implementations and field accessors.

mod accessors;
mod operator_error;

use proc_macro::TokenStream;

/// Implements error formatting, explicit source links, and optional operator classification.
#[proc_macro_derive(OperatorError, attributes(error, source, operator))]
pub fn operator_error(input: TokenStream) -> TokenStream {
    operator_error::expand(syn::parse_macro_input!(input as syn::DeriveInput))
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Implements the accessors selected by field-level get attributes.
#[proc_macro_derive(Accessors, attributes(get))]
pub fn accessors(input: TokenStream) -> TokenStream {
    accessors::expand(syn::parse_macro_input!(input as syn::DeriveInput))
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
