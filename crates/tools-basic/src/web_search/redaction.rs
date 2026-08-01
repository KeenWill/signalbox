use reqwest::{Url, header::HeaderValue};
use signalbox_model_runtime::{CredentialValue, redact_text};

use super::{canonicalization::*, text_decoding::*};

pub(super) const MAX_CREDENTIAL_BYTES: usize = 4 * 1024;

pub(super) struct CredentialScrubber {
    pub(super) exact: String,
    pub(super) json_escaped: String,
    pub(super) decoded_variants: Vec<String>,
}

impl CredentialScrubber {
    pub(super) fn try_new(credential: &CredentialValue) -> Option<Self> {
        if credential.expose_bytes().len() > MAX_CREDENTIAL_BYTES {
            return None;
        }
        if has_http_header_boundary_whitespace(credential.expose_bytes()) {
            return None;
        }
        HeaderValue::from_bytes(credential.expose_bytes()).ok()?;
        let exact = std::str::from_utf8(credential.expose_bytes())
            .ok()?
            .to_owned();
        if exact.is_empty() || fixed_outer_error_debug_may_contain(&exact) {
            return None;
        }
        let decoded_variants = decoded_credential_variants(&exact)?;
        let encoded = serde_json::to_string(&exact).ok()?;
        let json_escaped = encoded.get(1..encoded.len().checked_sub(1)?)?.to_owned();
        Some(Self {
            exact,
            json_escaped,
            decoded_variants,
        })
    }

    pub(super) fn redact_text(&self, text: &str) -> String {
        let generically_redacted = redact_text(text);
        let exact_redacted = generically_redacted.replace(&self.exact, "");
        let redacted = exact_redacted.replace(&self.json_escaped, "");
        if self.contains_credential(&redacted) {
            String::from("[redacted]")
        } else {
            redacted
        }
    }

    pub(super) fn contains_credential(&self, text: &str) -> bool {
        text.contains(&self.exact)
            || text.contains(&self.json_escaped)
            || unicode_normalized_contains(text, &self.exact)
            || unicode_normalized_contains(text, &self.json_escaped)
            || unicode_case_insensitive_contains(text, &self.exact)
            || unicode_case_insensitive_contains(text, &self.json_escaped)
            || self.decoded_variants.iter().any(|variant| {
                unicode_case_insensitive_contains(text, variant)
                    || encoded_contains_credential(text, variant)
            })
            || self.contains_encoded_credential(text)
    }

    pub(super) fn contains_encoded_credential(&self, text: &str) -> bool {
        encoded_contains_credential(text, &self.exact)
            || encoded_contains_credential(text, &self.json_escaped)
    }

    pub(super) fn contains_case_normalized_credential(&self, text: &str) -> bool {
        self.contains_credential(text)
    }

    pub(super) fn reversible_variants(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.exact.as_str()).chain(self.decoded_variants.iter().map(String::as_str))
    }

    pub(super) fn output_collision_variants(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.exact.as_str())
            .chain(std::iter::once(self.json_escaped.as_str()))
            .chain(self.decoded_variants.iter().map(String::as_str))
    }

    pub(super) fn url_contains_encoded_credential(&self, text: &str) -> bool {
        if self.contains_encoded_credential(text)
            || self.decoded_variants.iter().any(|variant| {
                unicode_case_insensitive_contains(text, variant)
                    || encoded_contains_credential(text, variant)
            })
        {
            return true;
        }
        if self.reversible_variants().any(|variant| {
            let slash_normalized = variant.replace('\\', "/");
            slash_normalized != variant
                && (unicode_case_insensitive_contains(text, &slash_normalized)
                    || encoded_contains_credential(text, &slash_normalized))
        }) {
            return true;
        }
        if self.reversible_variants().any(|variant| {
            url_preprocessed_credential_variant(variant).is_some_and(|normalized| {
                unicode_case_insensitive_contains(text, &normalized)
                    || encoded_contains_credential(text, &normalized)
            })
        }) {
            return true;
        }
        if self.reversible_variants().any(|variant| {
            normalize_url_path_dot_segments(variant).is_some_and(|normalized| {
                unicode_case_insensitive_contains(text, &normalized)
                    || encoded_contains_credential(text, &normalized)
            })
        }) {
            return true;
        }
        if self.reversible_variants().any(|variant| {
            canonicalized_url_port_fragment(variant).is_some_and(|normalized| {
                unicode_case_insensitive_contains(text, &normalized)
                    || encoded_contains_credential(text, &normalized)
            })
        }) {
            return true;
        }
        if self.reversible_variants().any(|variant| {
            canonicalized_complete_url(variant).is_some_and(|normalized| {
                unicode_case_insensitive_contains(text, &normalized)
                    || encoded_contains_credential(text, &normalized)
            })
        }) {
            return true;
        }
        let Ok(url) = Url::parse(text) else {
            return true;
        };
        if self
            .reversible_variants()
            .any(|variant| url.scheme().eq_ignore_ascii_case(variant))
        {
            return true;
        }
        let Some(host) = url.host_str() else {
            return false;
        };
        if self.reversible_variants().any(|variant| {
            canonicalized_url_host(variant).is_some_and(|credential_host| {
                unicode_case_insensitive_contains(host, &credential_host)
            })
        }) {
            return true;
        }
        if let Some(result_host) = parse_ip_literal(host) {
            if self
                .reversible_variants()
                .any(|variant| parse_ip_literal(variant).is_some_and(|key| key == result_host))
            {
                return true;
            }
            match result_host {
                std::net::IpAddr::V4(result_ipv4) => {
                    let result_components = result_ipv4.octets();
                    return self.reversible_variants().any(|variant| {
                        canonicalized_ipv4_component_fragments(variant).any(|fragment| {
                            result_components
                                .windows(fragment.len())
                                .any(|window| window == fragment)
                        })
                    });
                }
                std::net::IpAddr::V6(result_ipv6) => {
                    let result_components = result_ipv6.segments();
                    let result_octets = result_ipv6.octets();
                    return self.reversible_variants().any(|variant| {
                        let (mixed_components, mixed_octets) =
                            canonicalized_mixed_ipv6_ipv4_tail_fragments(variant);
                        canonicalized_ipv6_fragments(variant)
                            .into_iter()
                            .any(|fragment| {
                                result_components
                                    .windows(fragment.len())
                                    .any(|window| window == fragment)
                            })
                            || canonicalized_ipv4_tail_fragments(variant).into_iter().any(
                                |fragment| {
                                    result_octets
                                        .windows(fragment.len())
                                        .any(|window| window == fragment)
                                },
                            )
                            || mixed_components.into_iter().any(|fragment| {
                                result_components
                                    .windows(fragment.len())
                                    .any(|window| window == fragment)
                            })
                            || mixed_octets.into_iter().any(|fragment| {
                                result_octets
                                    .windows(fragment.len())
                                    .any(|window| window == fragment)
                            })
                    });
                }
            }
        }
        if self.reversible_variants().any(|variant| {
            idna::domain_to_ascii(variant)
                .is_ok_and(|credential_host| credential_host.eq_ignore_ascii_case(host))
        }) {
            return true;
        }
        let (unicode_host, decoding) = idna::domain_to_unicode(host);
        decoding.is_err()
            || unicode_case_insensitive_contains(&unicode_host, &self.exact)
            || unicode_case_insensitive_contains(&unicode_host, &self.json_escaped)
            || self.contains_credential(&unicode_host)
    }
}

pub(super) fn has_http_header_boundary_whitespace(value: &[u8]) -> bool {
    value
        .first()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
        || value
            .last()
            .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
}

pub(super) fn encoded_contains_credential(text: &str, credential: &str) -> bool {
    let mut decoded = String::from(text);
    for _ in 0..MAX_REVERSIBLE_DECODE_PASSES {
        let Some(next) = decode_reversible_text_once(&decoded) else {
            return true;
        };
        if next.change == ReversibleTextChange::Changed
            && unicode_case_insensitive_contains(&next.text, credential)
        {
            return true;
        }
        if next.change == ReversibleTextChange::Unchanged {
            return false;
        }
        decoded = next.text;
    }
    decode_reversible_text_once(&decoded)
        .is_none_or(|decoded| decoded.change == ReversibleTextChange::Changed)
}

pub(super) fn decoded_credential_variants(credential: &str) -> Option<Vec<String>> {
    let mut decoded = String::from(credential);
    let mut variants = Vec::new();
    for _ in 0..MAX_REVERSIBLE_DECODE_PASSES {
        let next = decode_reversible_text_once(&decoded)?;
        if next.change == ReversibleTextChange::Unchanged {
            return Some(variants);
        }
        variants.push(next.text.clone());
        decoded = next.text;
    }
    let decoded = decode_reversible_text_once(&decoded)?;
    (decoded.change == ReversibleTextChange::Unchanged).then_some(variants)
}

pub(super) fn text_contains_credential_variant(text: &str, credential: &str) -> bool {
    unicode_case_insensitive_contains(text, credential)
        || encoded_contains_credential(text, credential)
        || decoded_credential_variants(credential).is_none_or(|variants| {
            variants.iter().any(|variant| {
                unicode_case_insensitive_contains(text, variant)
                    || encoded_contains_credential(text, variant)
            })
        })
}

pub(super) fn fixed_outer_error_debug_may_contain(credential: &str) -> bool {
    text_contains_credential_variant("Err()", credential)
}
