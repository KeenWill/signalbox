use signalbox_application::ToolExecutorEvidence;
use signalbox_domain::{ToolExecutionErrorDetail, ToolExecutionErrorDetailFailure, ToolResultText};

use super::{diagnostic::*, redaction::*, result::*};

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
        .detail
        .as_deref()
        .map(|detail| scrubber.redact_text(detail))
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
    let bounded = detail_after_redaction(detail)?;
    if scrubber.contains_case_normalized_credential(bounded.as_str()) {
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
    if !matches!(failure, ToolExecutionErrorDetailFailure::TooLong { .. }) {
        return Err(WebSearchExecutorError::EvidenceEncoding);
    }

    let boundaries = detail
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(detail.len()))
        .collect::<Vec<_>>();
    let mut first_candidate = 1;
    let mut last_candidate = boundaries.len().saturating_sub(1);
    let mut admitted = None;
    while first_candidate <= last_candidate {
        let candidate_index = first_candidate + (last_candidate - first_candidate) / 2;
        let candidate = format!(
            "{}{TRUNCATION_SUFFIX}",
            &detail[..boundaries[candidate_index]]
        );
        match ToolExecutionErrorDetail::try_new(candidate) {
            Ok(detail) => {
                admitted = Some(detail);
                first_candidate = candidate_index + 1;
            }
            Err(rejected)
                if matches!(
                    rejected.failure(),
                    ToolExecutionErrorDetailFailure::TooLong { .. }
                ) =>
            {
                last_candidate = candidate_index.saturating_sub(1);
            }
            Err(_) => return Err(WebSearchExecutorError::EvidenceEncoding),
        }
    }
    admitted.ok_or(WebSearchExecutorError::EvidenceEncoding)
}
