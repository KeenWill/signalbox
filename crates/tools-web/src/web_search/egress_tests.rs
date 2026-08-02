use url::Url;

use super::{egress::*, test_support::*};

/// Explicit provider configuration derives exactly Brave's API origin and
/// fixed non-secret credential reference.
#[test]
fn brave_configuration_carries_one_provider_origin_and_reference() {
    let configuration = configuration();

    assert_eq!(configuration.provider(), WebSearchProvider::Brave);
    assert_eq!(
        configuration.egress_policy().allowed_origin(),
        BRAVE_SEARCH_ORIGIN
    );
    assert_eq!(
        configuration.credential_reference().as_str(),
        BRAVE_SEARCH_CREDENTIAL_REFERENCE
    );
}

/// The provider policy compares scheme, host, and effective port, so a
/// different origin is never admitted by the automatic read.
#[test]
fn brave_egress_policy_rejects_every_other_origin() {
    let policy = configuration().egress_policy;
    let endpoint = Url::parse(BRAVE_SEARCH_ENDPOINT).expect("provider endpoint is valid");
    let other =
        Url::parse("https://collector.example/search").expect("fixture alternate origin is valid");
    let other_scheme = Url::parse("http://api.search.brave.com/res/v1/web/search")
        .expect("fixture alternate scheme is valid");
    let other_port = Url::parse("https://api.search.brave.com:444/res/v1/web/search")
        .expect("fixture alternate port is valid");
    let subdomain = Url::parse("https://sub.api.search.brave.com/res/v1/web/search")
        .expect("fixture subdomain is valid");

    assert!(policy.admits(&endpoint));
    assert!(!policy.admits(&other));
    assert!(!policy.admits(&other_scheme));
    assert!(!policy.admits(&other_port));
    assert!(!policy.admits(&subdomain));
}
