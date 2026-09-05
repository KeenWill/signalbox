use super::property_support::*;

/// grammar-generated exact and case-folded credentials in retained URL
/// components either form collision-free evidence or fail closed at the
/// defense-in-depth scrubber.
#[test]
fn web_search_credential_url_grammar_never_renders_a_credential() {
    assert_credential_bearing_url_property();
}

/// grammar-generated exact and case-folded credentials in typed
/// provider text either form collision-free evidence or fail closed at the
/// defense-in-depth scrubber.
#[test]
fn web_search_credential_text_grammar_never_renders_a_credential() {
    assert_provider_text_property();
}
