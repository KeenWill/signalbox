//! Context admission for terminal tool text; executor evidence remains unchanged.

use super::{ToolLoopCorruption, ToolLoopRepositoryError};
use crate::model_execution::ToolContinuationUsageLimitCatalog;
use rust_decimal::Decimal;
use signalbox_domain::{
    ModelCallId, ProviderModelIdentity, ResolvedProviderTarget, ToolExecutionErrorDetail,
};
use sqlx::{PgConnection, Row};

pub(super) async fn result_byte_limit(
    connection: &mut PgConnection,
    producing_call: ModelCallId,
    limits: &ToolContinuationUsageLimitCatalog,
) -> Result<Option<i64>, ToolLoopRepositoryError> {
    let row = sqlx::query(
        "SELECT call.resolved_provider_model_identity_id,
                call.prepared_context_window_tokens, call.prepared_max_output_tokens,
                call.usage_input_tokens, call.usage_output_tokens,
                call.usage_input_includes_cache_tokens,
                call.usage_cache_read_input_tokens, call.usage_cache_creation_input_tokens,
                call.retained_input_tokens, call.retained_output_tokens,
                call.prepared_provider_compaction_replay,
                NOT EXISTS (
                    SELECT 1 FROM semantic_transcript_entry compacted
                     WHERE compacted.producing_model_call_id = call.model_call_id
                       AND compacted.payload_kind = 'provider_compaction'
                       AND compacted.assistant_text_value::jsonb ->> 'content' IS NOT NULL
                ) AS input_is_retained,
                (SELECT count(*) FROM tool_request request
                  WHERE request.producing_model_call_id = call.model_call_id) AS result_count,
                (SELECT COALESCE(sum(octet_length(jsonb_build_object(
                    'position', $2::numeric,
                    'source_session_id', request.session_id,
                    'entry_id', request.request_id,
                    'type', 'tool_execution_result',
                    'tool_request_id', request.request_id,
                    'tool_attempt_id', request.request_id,
                    'content', ''
                )::text)), 0)::bigint FROM tool_request request
                  WHERE request.producing_model_call_id = call.model_call_id) AS framing_bytes,
                octet_length(jsonb_build_object('error', jsonb_build_object(
                    'kind', 'preauthorization_rejected',
                    'detail', $3::text
                ))::text)::bigint AS minimum_failure_content_bytes
           FROM model_call call WHERE call.model_call_id = $1",
    )
    .bind(producing_call.into_uuid())
    // Result entry/attempt identities are assigned later; all UUID encodings
    // have the request ID's width. Reserve the widest physical position and
    // the empty result envelope for every persisted request, including
    // inadmissible proposals, without charging source response payloads.
    .bind(Decimal::from(u64::MAX))
    .bind(truncation_marker(0, usize::MAX))
    .fetch_one(&mut *connection)
    .await?;
    let target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
        row.try_get("resolved_provider_model_identity_id")?,
    ));
    let Some(limit) = limits
        .iter()
        .find_map(|((candidate, _), limit)| (*candidate == target).then_some(limit))
    else {
        return Ok(None);
    };
    let number = |field: &'static str| -> Result<Option<u64>, ToolLoopRepositoryError> {
        row.try_get::<Option<Decimal>, _>(field)?
            .map(|value| {
                if !value.fract().is_zero() || value.is_sign_negative() {
                    return Err(ToolLoopCorruption::Inconsistent("tool result budget").into());
                }
                u64::try_from(value)
                    .map_err(|_| ToolLoopCorruption::Inconsistent("tool result budget").into())
            })
            .transpose()
    };
    let (Some(window), Some(output)) = (
        number("prepared_context_window_tokens")?,
        number("prepared_max_output_tokens")?,
    ) else {
        return Ok(None);
    };
    let mut input = number("usage_input_tokens")?.unwrap_or(0);
    if !row.try_get::<bool, _>("usage_input_includes_cache_tokens")? {
        input = input
            .saturating_add(number("usage_cache_read_input_tokens")?.unwrap_or(0))
            .saturating_add(number("usage_cache_creation_input_tokens")?.unwrap_or(0));
    }
    let mut previous_output = number("usage_output_tokens")?.unwrap_or(0);
    if row.try_get::<Option<bool>, _>("prepared_provider_compaction_replay")? == Some(true) {
        input = number("retained_input_tokens")?.unwrap_or(
            if row.try_get::<bool, _>("input_is_retained")? {
                input
            } else {
                0
            },
        );
        previous_output = number("retained_output_tokens")?.unwrap_or(previous_output);
    }
    let count = u64::try_from(row.try_get::<i64, _>("result_count")?)
        .map_err(|_| ToolLoopCorruption::Inconsistent("tool result count"))?;
    let framing = u64::try_from(row.try_get::<i64, _>("framing_bytes")?)
        .map_err(|_| ToolLoopCorruption::Inconsistent("tool result framing"))?;
    let minimum_failure_content =
        u64::try_from(row.try_get::<i64, _>("minimum_failure_content_bytes")?)
            .map_err(|_| ToolLoopCorruption::Inconsistent("tool failure content"))?;
    let safe_prefix = window
        .saturating_sub(output)
        .saturating_sub(limit.compaction_prompt_bytes());
    // This finite allowance uses the cap or default, not an unknown future count.
    // A larger response that exhausts headroom takes the compaction path.
    let framing_per_result = framing
        .checked_div(count)
        .ok_or(ToolLoopCorruption::Inconsistent("empty tool result batch"))?;
    let next_batch = limit
        .max_tool_requests()
        .unwrap_or(signalbox_application::ToolProposalLimits::DEFAULT_MAX_REQUESTS)
        .saturating_mul(framing_per_result.saturating_add(minimum_failure_content));
    let headroom = window
        .saturating_sub(output.saturating_add(output))
        .saturating_sub(next_batch)
        .saturating_sub(input)
        .saturating_sub(previous_output);
    let per_result = safe_prefix
        .min(headroom)
        .saturating_sub(framing)
        .checked_div(count)
        .ok_or(ToolLoopCorruption::Inconsistent("empty tool result batch"))?;
    Ok(Some(i64::try_from(per_result).map_err(|_| {
        ToolLoopCorruption::Inconsistent("tool result byte limit")
    })?))
}

fn truncation_marker(retained: usize, dropped: usize) -> String {
    format!("\n[tool result truncated: retained {retained} bytes; dropped {dropped} bytes]")
}

pub(super) fn context_error_detail(text: &str, limit: usize) -> String {
    // Error details remain control-free and within their existing domain bound.
    context_text(text, limit.min(ToolExecutionErrorDetail::MAX_UTF8_BYTES))
        .replace('\n', " ")
        .trim_start_matches(' ')
        .to_owned()
}

/// The budget includes JSON string escaping and the truncation marker.
/// A marker that cannot fit is retained for dedicated compaction.
pub(super) fn context_text(text: &str, limit: usize) -> String {
    fn encoded_bytes(text: &str) -> usize {
        // A UTF-8 string always serializes; counting avoids allocating a JSON copy.
        text.bytes().fold(2usize, |bytes, byte| {
            bytes.saturating_add(match byte {
                b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 8 | 12 => 2,
                0..=31 => 6,
                _ => 1,
            })
        })
    }
    if encoded_bytes(text) <= limit {
        return text.to_owned();
    }
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    // Marker growth is bounded by the decimal byte counts. Shrinking the prefix
    // by the exact excess keeps every iteration moving toward a fitting result.
    loop {
        let suffix = truncation_marker(end, text.len() - end);
        let bytes =
            encoded_bytes(&text[..end]).saturating_add(encoded_bytes(&suffix).saturating_sub(2));
        if bytes <= limit || end == 0 {
            return format!("{}{suffix}", &text[..end]);
        }
        end = end.saturating_sub(bytes - limit);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::context_text;

    #[test]
    fn context_prefix_reports_exact_utf8_counts_within_escaped_budget() {
        let source = "𠜎\t\n\"".repeat(100);
        let limit = 160;
        let bounded = context_text(&source, limit);
        let marker_start = bounded.rfind("\n[tool result truncated:").expect("marker");
        let (prefix, marker) = bounded.split_at(marker_start);
        assert_eq!(prefix, &source[..prefix.len()]);
        assert_eq!(
            marker,
            format!(
                "\n[tool result truncated: retained {} bytes; dropped {} bytes]",
                prefix.len(),
                source.len() - prefix.len() + 1,
            )
        );
        assert!(serde_json::to_vec(&bounded).expect("text encodes").len() <= limit);
        assert_eq!(context_text(&bounded, limit), bounded);
    }

    #[test]
    fn context_error_detail_keeps_admitted_unicode_whitespace_exact() {
        let source = "\u{2003}failure detail";
        signalbox_domain::ToolExecutionErrorDetail::try_new(source.to_owned())
            .expect("non-POSIX whitespace is admitted");
        assert_eq!(
            super::context_error_detail(source, source.len() + 2),
            source
        );
    }

    #[test]
    fn context_prefix_keeps_a_small_result_exact() {
        let source = "unchanged 界 result";
        assert_eq!(context_text(source, source.len() + 2), source);
    }
}
