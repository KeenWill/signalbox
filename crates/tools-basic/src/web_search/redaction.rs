use reqwest::header::HeaderValue;
use signalbox_application::ToolExecutorEvidence;
use signalbox_domain::ToolExecutionErrorDetail;
use signalbox_model_runtime::{CredentialValue, redact_text};

use super::{
    diagnostic::WebSearchExecutorError, egress::fixed_egress_diagnostic_outputs,
    result::fixed_result_diagnostic_outputs, text_decoding::*,
};
use url::Url;

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
        if exact.is_empty() || fixed_diagnostic_output_may_contain(&exact) {
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

    pub(super) fn reversible_variants(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.exact.as_str()).chain(self.decoded_variants.iter().map(String::as_str))
    }

    pub(super) fn output_collision_variants(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.exact.as_str())
            .chain(std::iter::once(self.json_escaped.as_str()))
            .chain(self.decoded_variants.iter().map(String::as_str))
    }

    pub(super) fn url_contains_encoded_credential(&self, text: &str) -> bool {
        if self.output_collision_variants().any(|variant| {
            unicode_case_insensitive_contains(text, variant)
                || encoded_contains_credential(text, variant)
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
            whatwg_host(variant)
                .is_some_and(|candidate| unicode_case_insensitive_contains(host, &candidate))
                || idna::domain_to_ascii(variant)
                    .is_ok_and(|candidate| unicode_case_insensitive_contains(host, &candidate))
                || whatwg_http_url(variant)
                    .is_some_and(|candidate| unicode_case_insensitive_contains(text, &candidate))
        }) {
            return true;
        }
        let (unicode_host, decoding) = idna::domain_to_unicode(host);
        if decoding.is_err() {
            return true;
        }
        unicode_case_insensitive_contains(&unicode_host, &self.exact)
            || unicode_case_insensitive_contains(&unicode_host, &self.json_escaped)
            || self.contains_credential(&unicode_host)
            || self.reversible_variants().any(|variant| {
                idna::domain_to_ascii(variant).is_ok_and(|ascii| {
                    let (mapped, result) = idna::domain_to_unicode(&ascii);
                    result.is_ok() && unicode_case_insensitive_contains(&unicode_host, &mapped)
                })
            })
    }
}

fn whatwg_host(value: &str) -> Option<String> {
    let candidate = Url::parse(&format!("http://{value}/")).ok()?;
    (candidate.username().is_empty()
        && candidate.password().is_none()
        && candidate.port().is_none()
        && candidate.path() == "/"
        && candidate.query().is_none()
        && candidate.fragment().is_none())
    .then(|| candidate.host_str().map(str::to_owned))?
}

fn whatwg_http_url(value: &str) -> Option<String> {
    let mut candidate = Url::parse(value).ok()?;
    if !matches!(candidate.scheme(), "http" | "https") || candidate.host_str().is_none() {
        return None;
    }
    candidate.set_username("").ok()?;
    candidate.set_password(None).ok()?;
    candidate.set_query(None);
    candidate.set_fragment(None);
    Some(candidate.to_string())
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

pub(super) fn fixed_diagnostic_output_may_contain(credential: &str) -> bool {
    text_contains_credential_variant("Err()", credential)
        || text_contains_credential_variant("Ok()", credential)
        || fixed_egress_diagnostic_outputs()
            .iter()
            .any(|output| text_contains_credential_variant(output, credential))
        || fixed_result_diagnostic_outputs()
            .iter()
            .any(|output| text_contains_credential_variant(output, credential))
        || fixed_evidence_diagnostic_outputs().is_none_or(|outputs| {
            outputs
                .iter()
                .any(|output| text_contains_credential_variant(output, credential))
        })
}

fn fixed_evidence_diagnostic_outputs() -> Option<[String; 9]> {
    const DETAIL_SHAPE_PROBE: &str = "diagnostic probe";
    let populated_detail =
        ToolExecutionErrorDetail::try_new(String::from(DETAIL_SHAPE_PROBE)).ok()?;
    let populated_failure = format!(
        "{:?}",
        ToolExecutorEvidence::KnownFailed {
            detail: Some(populated_detail)
        }
    );
    let probe_debug = format!("{:?}", String::from(DETAIL_SHAPE_PROBE));
    let empty_debug = format!("{:?}", String::new());
    let populated_failure_shape = populated_failure.replace(&probe_debug, &empty_debug);
    let populated_result = format!(
        "{:?}",
        Ok::<ToolExecutorEvidence, WebSearchExecutorError>(ToolExecutorEvidence::KnownFailed {
            detail: Some(ToolExecutionErrorDetail::try_new(String::from(DETAIL_SHAPE_PROBE)).ok()?)
        })
    );
    let populated_result_shape = populated_result.replace(&probe_debug, &empty_debug);
    if populated_failure_shape == populated_failure || populated_result_shape == populated_result {
        return None;
    }
    Some([
        format!("{:?}", ToolExecutorEvidence::CompletedText(String::new())),
        format!(
            "{:?}",
            ToolExecutorEvidence::CompletedText(String::from("\""))
        ),
        format!(
            "{:?}",
            Ok::<ToolExecutorEvidence, ()>(ToolExecutorEvidence::CompletedText(String::new()))
        ),
        format!(
            "{:?}",
            Ok::<ToolExecutorEvidence, WebSearchExecutorError>(ToolExecutorEvidence::KnownFailed {
                detail: None
            })
        ),
        format!(
            "{:?}",
            Err::<ToolExecutorEvidence, WebSearchExecutorError>(
                WebSearchExecutorError::EvidenceEncoding
            )
        ),
        format!("{:?}", ToolExecutorEvidence::KnownFailed { detail: None }),
        populated_failure_shape,
        populated_result_shape,
        format!("{:?}", ToolExecutorEvidence::Ambiguous),
    ])
}
