use super::property_support::*;

/// INV-035: grammar-generated URL and credential representations either form
/// collision-free evidence or fail closed at the defense-in-depth scrubber.
#[test]
fn web_search_credential_url_grammar_never_renders_a_credential() {
    assert_credential_bearing_url_property();
}

/// INV-035: grammar-generated credential representations in typed provider
/// text either form collision-free evidence or fail closed at the scrubber.
#[test]
fn web_search_credential_text_grammar_never_renders_a_credential() {
    assert_provider_text_property();
}
