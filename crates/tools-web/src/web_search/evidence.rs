use signalbox_application::ToolExecutorEvidence;
use signalbox_domain::{ToolExecutionErrorDetail, ToolExecutionErrorDetailFailure, ToolResultText};

use super::{diagnostic::*, redaction::*, result::*, text_decoding::*};

pub(super) const TRUNCATION_SUFFIX: &str = " … [truncated]";

pub(super) fn canonical_json_string(
    mut value: serde_json::Value,
) -> Result<String, serde_json::Error> {
    value.sort_all_objects();
    serde_json::to_string(&value)
}

#[derive(serde::Serialize)]
pub(super) struct RenderedSearchResult {
    pub(super) title: String,
    pub(super) url: String,
    pub(super) snippet: String,
}

pub(super) fn success_evidence(
    response: WebSearchResponse,
    scrubber: &CredentialScrubber,
) -> Result<ToolExecutorEvidence, WebSearchExecutorError> {
    let truncated =
        response.results.len() > MAX_RETURNED_RESULTS || response.more_results_available();
    let results = response
        .results
        .into_iter()
        .take(MAX_RETURNED_RESULTS)
        .map(|result| {
            if scrubber.url_contains_encoded_credential(result.url.as_str()) {
                return Err(WebSearchExecutorError::EvidenceEncoding);
            }

            let title = redact_entity_escaped_component(
                result.title.as_str(),
                MAX_RESULT_TITLE_BYTES,
                scrubber,
            )?;
            if title.len() > MAX_RESULT_TITLE_BYTES || title.trim().is_empty() {
                return Err(WebSearchExecutorError::EvidenceEncoding);
            }
            let url = scrubber.redact_text(result.url.as_str());
            let parsed_url = ParsedResultUrl::try_new(&url)
                .filter(|parsed| parsed.as_str() == url)
                .ok_or(WebSearchExecutorError::EvidenceEncoding)?;
            let snippet = redact_entity_escaped_component(
                result.snippet.as_str(),
                MAX_RESULT_SNIPPET_BYTES,
                scrubber,
            )?;
            Ok(RenderedSearchResult {
                title,
                url: parsed_url.as_str().to_owned(),
                snippet,
            })
        })
        .collect::<Result<Vec<_>, WebSearchExecutorError>>()?;
    let content = canonical_json_string(serde_json::json!({
        "results": results,
        "truncated": truncated,
    }))
    .map_err(|_| WebSearchExecutorError::EvidenceEncoding)?;
    if scrubber.contains_credential(&content) {
        return Err(WebSearchExecutorError::EvidenceEncoding);
    }
    let evidence = completed_text_evidence(content)?;
    let rendered_result = format!(
        "{:?}",
        Ok::<&ToolExecutorEvidence, WebSearchExecutorError>(&evidence)
    );
    if scrubber.contains_credential(&rendered_result) {
        return Err(WebSearchExecutorError::EvidenceEncoding);
    }
    Ok(evidence)
}

pub(super) fn redact_entity_escaped_component(
    component: &str,
    maximum_bytes: usize,
    scrubber: &CredentialScrubber,
) -> Result<String, WebSearchExecutorError> {
    let raw = decode_html_character_references_to_closure(component)
        .ok_or(WebSearchExecutorError::EvidenceEncoding)?;
    let redacted = scrubber.redact_text(&raw);
    entity_escape(&redacted, maximum_bytes).ok_or(WebSearchExecutorError::EvidenceEncoding)
}

pub(super) fn decode_html_character_references_to_closure(component: &str) -> Option<String> {
    let raw_provider_text = html_escape::decode_html_entities(component).into_owned();
    let mut decoded = raw_provider_text.clone();
    for _ in 0..MAX_REVERSIBLE_DECODE_PASSES {
        let next = html_escape::decode_html_entities(&decoded).into_owned();
        if next == decoded {
            return Some(raw_provider_text);
        }
        decoded = next;
    }
    let final_pass = html_escape::decode_html_entities(&decoded);
    (final_pass.as_ref() == decoded).then_some(raw_provider_text)
}

pub(super) fn completed_text_evidence(
    content: String,
) -> Result<ToolExecutorEvidence, WebSearchExecutorError> {
    ToolResultText::try_new(content)
        .map(|content| ToolExecutorEvidence::CompletedText(content.into_string()))
        .map_err(|_| WebSearchExecutorError::EvidenceEncoding)
}

pub(super) fn known_failure_evidence(
    detail: ToolExecutionErrorDetail,
    scrubber: &CredentialScrubber,
) -> Result<ToolExecutorEvidence, WebSearchExecutorError> {
    let detail = (!scrubber.contains_credential(detail.as_str())).then_some(detail);
    let evidence = ToolExecutorEvidence::KnownFailed { detail };
    let rendered_result = format!(
        "{:?}",
        Ok::<&ToolExecutorEvidence, WebSearchExecutorError>(&evidence)
    );
    if scrubber.contains_credential(&rendered_result) {
        return Err(WebSearchExecutorError::EvidenceEncoding);
    }
    Ok(evidence)
}

pub(super) fn provider_error_detail(
    error: WebSearchProviderError,
    scrubber: &CredentialScrubber,
) -> Result<Option<ToolExecutionErrorDetail>, WebSearchExecutorError> {
    let redacted = match error.detail.as_deref() {
        Some(detail) => redact_entity_escaped_component(
            detail,
            MAX_ESCAPED_PROVIDER_ERROR_DETAIL_BYTES,
            scrubber,
        )?,
        None => String::new(),
    };
    let normalized = redacted
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let body = normalized.trim();
    let detail = if body.is_empty() {
        format!(
            "web search provider rejected the request with HTTP status {}",
            error.status
        )
    } else {
        format!(
            "web search provider rejected the request with HTTP status {}: {body}",
            error.status
        )
    };
    let bounded = detail_after_redaction(detail)?;
    if scrubber.contains_credential(bounded.as_str()) {
        return Ok(None);
    }
    Ok(Some(bounded))
}

pub(super) fn detail_after_redaction(
    detail: String,
) -> Result<ToolExecutionErrorDetail, WebSearchExecutorError> {
    let rejected = match ToolExecutionErrorDetail::try_new(detail) {
        Ok(detail) => return Ok(detail),
        Err(rejected) => rejected,
    };
    let (detail, failure) = rejected.into_parts();
    match failure {
        ToolExecutionErrorDetailFailure::TooLong { .. } => {}
        ToolExecutionErrorDetailFailure::Empty
        | ToolExecutionErrorDetailFailure::SurroundingWhitespace
        | ToolExecutionErrorDetailFailure::ContainsControl => {
            return Err(WebSearchExecutorError::EvidenceEncoding);
        }
    }

    let mut first_candidate = 1;
    let mut last_candidate = detail.len().saturating_sub(1);
    let mut admitted = None;
    while first_candidate <= last_candidate {
        let candidate_index = first_candidate + (last_candidate - first_candidate) / 2;
        let candidate_boundary = prior_char_boundary(&detail, candidate_index);
        let candidate = format!("{}{TRUNCATION_SUFFIX}", &detail[..candidate_boundary]);
        match ToolExecutionErrorDetail::try_new(candidate) {
            Ok(detail) => {
                admitted = Some(detail);
                first_candidate = candidate_index + 1;
            }
            Err(rejected) => match rejected.failure() {
                ToolExecutionErrorDetailFailure::TooLong { .. } => {
                    last_candidate = candidate_index.saturating_sub(1);
                }
                ToolExecutionErrorDetailFailure::Empty
                | ToolExecutionErrorDetailFailure::SurroundingWhitespace
                | ToolExecutionErrorDetailFailure::ContainsControl => {
                    return Err(WebSearchExecutorError::EvidenceEncoding);
                }
            },
        }
    }
    admitted.ok_or(WebSearchExecutorError::EvidenceEncoding)
}

fn prior_char_boundary(text: &str, candidate: usize) -> usize {
    let mut boundary = candidate.min(text.len());
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}
