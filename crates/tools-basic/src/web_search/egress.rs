use reqwest::Url;
use signalbox_model_runtime::CredentialReference;

/// Non-secret name of the daemon-held Brave Search credential.
pub const BRAVE_SEARCH_CREDENTIAL_REFERENCE: &str = "brave-search-primary";

pub(super) const BRAVE_SEARCH_ORIGIN: &str = "https://api.search.brave.com";

pub(super) const BRAVE_SEARCH_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";

pub(super) const BRAVE_SUBSCRIPTION_TOKEN_HEADER: &str = "x-subscription-token";

/// Configured web-search provider.
///
/// The enum is deliberately explicit and non-exhaustive: adding a provider is
/// a new variant plus its exhaustive endpoint/authentication mapping. There is
/// no provider selection or fallback at execution time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WebSearchProvider {
    /// Brave Search Web API.
    Brave,
}

impl WebSearchProvider {
    pub(super) fn endpoint(self) -> ProviderEndpoint {
        match self {
            Self::Brave => ProviderEndpoint {
                origin: BRAVE_SEARCH_ORIGIN,
                url: BRAVE_SEARCH_ENDPOINT,
                credential_header: BRAVE_SUBSCRIPTION_TOKEN_HEADER,
                credential_reference: BRAVE_SEARCH_CREDENTIAL_REFERENCE,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ProviderEndpoint {
    pub(super) origin: &'static str,
    pub(super) url: &'static str,
    pub(super) credential_header: &'static str,
    pub(super) credential_reference: &'static str,
}

/// Immutable deployment configuration for one explicitly selected provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebSearchConfiguration {
    pub(super) provider: WebSearchProvider,
    pub(super) egress_policy: WebSearchEgressPolicy,
    pub(super) credential_reference: CredentialReference,
}

impl WebSearchConfiguration {
    /// Configures exactly one provider and its fixed egress/credential mapping.
    pub fn new(provider: WebSearchProvider) -> Self {
        let endpoint = provider.endpoint();
        Self {
            provider,
            egress_policy: WebSearchEgressPolicy { provider },
            credential_reference: CredentialReference::new(endpoint.credential_reference),
        }
    }

    /// The provider selected by deployment configuration.
    pub const fn provider(&self) -> WebSearchProvider {
        self.provider
    }

    /// The exact-origin egress policy derived from the selected provider.
    pub const fn egress_policy(&self) -> &WebSearchEgressPolicy {
        &self.egress_policy
    }

    /// The non-secret provider credential reference resolved per request.
    pub const fn credential_reference(&self) -> &CredentialReference {
        &self.credential_reference
    }
}

/// Exact API-origin policy derived from the configured provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WebSearchEgressPolicy {
    pub(super) provider: WebSearchProvider,
}

impl WebSearchEgressPolicy {
    /// The sole admitted API origin.
    pub fn allowed_origin(&self) -> &'static str {
        self.provider.endpoint().origin
    }

    pub(super) fn admits(&self, url: &Url) -> bool {
        let endpoint = self.provider.endpoint();
        let Ok(origin) = Url::parse(endpoint.origin) else {
            return false;
        };
        url.scheme() == origin.scheme()
            && url.host_str() == origin.host_str()
            && url.port_or_known_default() == origin.port_or_known_default()
    }
}
