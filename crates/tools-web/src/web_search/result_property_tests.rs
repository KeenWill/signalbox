use super::property_support::*;

/// grammar-generated credentials in URL user information, query
/// components, and fragments are absent from parsed result output without
/// relying on credential scrubbing.
#[test]
fn web_search_structural_url_grammar_never_renders_discarded_components() {
    assert_structural_url_property();
}
