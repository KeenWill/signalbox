//! Derives for repeated error implementations.

mod operator_error;

use proc_macro::TokenStream;

/// Implements error formatting, explicit source links, and optional operator classification.
#[proc_macro_derive(OperatorError, attributes(error, source, operator))]
pub fn operator_error(input: TokenStream) -> TokenStream {
    operator_error::expand(syn::parse_macro_input!(input as syn::DeriveInput))
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
