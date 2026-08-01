use signalbox_application::ToolExecutorEvidence;
use signalbox_domain::{ToolExecutionErrorDetail, ToolResultText};

use super::{diagnostic::*, redaction::*, result::*};

pub(super) const MAX_ERROR_DETAIL_BYTES: usize = 4 * 1024;

pub(super) const TRUNCATION_SUFFIX: &str = " … [truncated]";

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

            let title = scrubber.redact_text(result.title.as_str());
            if title.len() > MAX_RESULT_TITLE_BYTES || title.trim().is_empty() {
                return Err(WebSearchExecutorError::EvidenceEncoding);
            }
            let url = scrubber.redact_text(result.url.as_str());
            let parsed_url = ParsedResultUrl::try_new(&url)
                .filter(|parsed| parsed.as_str() == url)
                .ok_or(WebSearchExecutorError::EvidenceEncoding)?;
            let snippet = scrubber.redact_text(result.snippet.as_str());
            if snippet.len() > MAX_RESULT_SNIPPET_BYTES {
                return Err(WebSearchExecutorError::EvidenceEncoding);
            }
            Ok(RenderedSearchResult {
                title,
                url: parsed_url.as_str().to_owned(),
                snippet,
            })
        })
        .collect::<Result<Vec<_>, WebSearchExecutorError>>()?;
    let content = serde_json::to_string(&serde_json::json!({
        "results": results,
        "truncated": truncated,
    }))
    .map_err(|_| WebSearchExecutorError::EvidenceEncoding)?;
    if scrubber.contains_case_normalized_credential(&content) {
        return Err(WebSearchExecutorError::EvidenceEncoding);
    }
    completed_text_evidence(content)
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
    let detail = (!scrubber.contains_case_normalized_credential(detail.as_str())).then_some(detail);
    Ok(ToolExecutorEvidence::KnownFailed { detail })
}

pub(super) fn provider_error_detail(
    error: WebSearchProviderError,
    scrubber: &CredentialScrubber,
) -> Result<Option<ToolExecutionErrorDetail>, WebSearchExecutorError> {
    let redacted = error
        .message
        .as_deref()
        .map(|message| scrubber.redact_text(message))
        .unwrap_or_default();
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
    let bounded = truncate_after_redaction(detail);
    if scrubber.contains_case_normalized_credential(&bounded) {
        return Ok(None);
    }
    ToolExecutionErrorDetail::try_new(bounded)
        .map(Some)
        .map_err(|_| WebSearchExecutorError::EvidenceEncoding)
}

pub(super) fn truncate_after_redaction(detail: String) -> String {
    if detail.len() <= MAX_ERROR_DETAIL_BYTES {
        return detail;
    }
    let retained_bytes = MAX_ERROR_DETAIL_BYTES.saturating_sub(TRUNCATION_SUFFIX.len());
    let mut end = retained_bytes;
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &detail[..end], TRUNCATION_SUFFIX)
}
