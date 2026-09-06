//! Buffered Responses decoding and shared terminal and item conversion.

use std::collections::BTreeSet;

use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CompletionEvidence, ExchangeFacts, FinishReason,
    LossCause, NativeErrorFacts, Observation, ObservationFact, ObservationSink,
    ProviderErrorEvidence, ProviderMessageId, ProviderReportedModel, RefusalEvidence,
    TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal, ToolCallsAtLoss, ToolName,
    validate_provider_json_nesting,
};

use crate::{
    status::classify_error,
    translate::is_valid_function_name,
    wire::{Response, ResponseError, WireContent, WireOutputItem, WireUsage},
};

pub(crate) fn convert_usage(wire: &WireUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: wire.input_tokens,
        output_tokens: wire.output_tokens,
        cache_creation_input_tokens: wire
            .input_tokens_details
            .as_ref()
            .and_then(|d| d.cache_write_tokens),
        cache_read_input_tokens: wire
            .input_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens),
    }
}

pub(crate) fn convert_tool_call(item: &WireOutputItem) -> Result<ToolCallProposal, String> {
    let (Some(id), Some(name), Some(arguments)) = (&item.call_id, &item.name, &item.arguments)
    else {
        return Err("function call lacks call_id, name, or arguments".to_string());
    };
    if id.is_empty() || !is_valid_function_name(name) {
        return Err("function call has an empty call_id or invalid name".to_string());
    }
    validate_provider_json_nesting(arguments.as_bytes()).map_err(|e| e.to_string())?;
    Ok(ToolCallProposal {
        id: ToolCallId::new(id.clone()),
        name: ToolName::new(name.clone()),
        arguments_json: arguments.clone(),
    })
}

struct ConvertedItem {
    parts: Vec<AssistantPart>,
    refused: bool,
}

/// Converts one complete output item; reasoning has no durable producer here.
fn convert_item(item: &WireOutputItem) -> Result<ConvertedItem, String> {
    match item.kind.as_str() {
        "reasoning" => Ok(ConvertedItem {
            parts: Vec::new(),
            refused: false,
        }),
        "function_call" => Ok(ConvertedItem {
            parts: vec![AssistantPart::ToolCall(convert_tool_call(item)?)],
            refused: false,
        }),
        "message" => {
            if item.role.as_deref() != Some("assistant") {
                return Err("output message must have assistant role".to_string());
            }
            let Some(parts) = &item.content else {
                return Err("output message lacks content".to_string());
            };
            let mut content = Vec::new();
            let mut refused = false;
            for part in parts {
                let text = match part {
                    WireContent::OutputText { text } => text,
                    WireContent::Refusal { refusal } => {
                        refused = true;
                        refusal
                    }
                    WireContent::Unknown => {
                        return Err("unrecognized output content type".to_string());
                    }
                };
                if !text.is_empty() {
                    content.push(AssistantPart::Text(text.clone()));
                }
            }
            Ok(ConvertedItem {
                parts: content,
                refused,
            })
        }
        other => Err(format!("unrecognized output item type {other:?}")),
    }
}

pub(crate) fn map_terminal(
    status: &str,
    reason: Option<&str>,
    tool_calls: ToolCallsAtLoss,
) -> FinishReason {
    match (status, reason) {
        ("completed", _) if tool_calls == ToolCallsAtLoss::Opened => FinishReason::ToolUse,
        ("completed", _) => FinishReason::EndTurn,
        ("incomplete", Some("max_output_tokens")) => FinishReason::MaxOutputTokens,
        ("incomplete", Some("content_filter")) => FinishReason::Refusal,
        ("incomplete", Some(other)) => FinishReason::Unrecognized {
            provider_token: other.to_string(),
        },
        (other, _) => FinishReason::Unrecognized {
            provider_token: other.to_string(),
        },
    }
}

pub(crate) fn provider_error(
    error: ResponseError,
    exchange: ExchangeFacts,
    reported_model: Option<ProviderReportedModel>,
    usage: TokenUsage,
) -> TerminalEvidence {
    TerminalEvidence::ProviderError(ProviderErrorEvidence {
        kind: classify_error(0, error.code.as_deref()),
        non_acceptance_proven: false,
        native: NativeErrorFacts {
            error_token: None,
            error_code: error.code,
            message: error.message,
        },
        exchange,
        reported_model,
        usage,
    })
}

pub(crate) fn decode_buffered_response<C: Clone>(
    body: &[u8],
    exchange: ExchangeFacts,
    correlation: &C,
    sink: &mut (dyn ObservationSink<C> + Send),
) -> TerminalEvidence {
    let response = validate_provider_json_nesting(body)
        .map_err(|e| e.to_string())
        .and_then(|()| serde_json::from_slice::<Response>(body).map_err(|e| e.to_string()));
    match response {
        Ok(response) => decode_response(response, exchange, correlation, sink),
        Err(detail) => TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::ResponseUnintelligible { detail },
            exchange,
            reported_model: None,
            finish_reported: None,
            tool_calls: ToolCallsAtLoss::Unobserved,
            usage: TokenUsage::unreported(),
        }),
    }
}

pub(crate) fn decode_response<C: Clone>(
    response: Response,
    exchange: ExchangeFacts,
    correlation: &C,
    sink: &mut (dyn ObservationSink<C> + Send),
) -> TerminalEvidence {
    let reported_model = response.model.clone().map(ProviderReportedModel::new);
    let usage = response
        .usage
        .as_ref()
        .map(convert_usage)
        .unwrap_or_default();
    let items: Result<Vec<WireOutputItem>, _> = response
        .output
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|raw| serde_json::from_str(raw.get()))
        .collect();
    let tool_calls = match &items {
        Ok(items) if items.iter().any(|item| item.kind == "function_call") => {
            ToolCallsAtLoss::Opened
        }
        Ok(_) if response.output.is_some() => ToolCallsAtLoss::NoneOpened,
        _ => ToolCallsAtLoss::Unobserved,
    };
    let loss = |detail: String, finish_reported: Option<FinishReason>| {
        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::ResponseUnintelligible { detail },
            exchange: exchange.clone(),
            reported_model: reported_model.clone(),
            finish_reported,
            tool_calls,
            usage,
        })
    };
    if let Some(model) = &reported_model {
        emit(
            correlation,
            sink,
            ObservationFact::ProviderModelReported(model.clone()),
        );
    }
    if response.usage.is_some() {
        emit(correlation, sink, ObservationFact::UsageReported(usage));
    }
    if response.status.as_deref() == Some("failed") {
        return match response.error {
            Some(error) => provider_error(error, exchange, reported_model, usage),
            None => loss("failed response lacks error".to_string(), None),
        };
    }
    if response.object.as_deref() != Some("response")
        || response.id.as_deref().is_none_or(str::is_empty)
        || response.model.as_deref().is_none_or(str::is_empty)
        || response.output.is_none()
    {
        return loss(
            "response lacks its object, id, model, or output".to_string(),
            None,
        );
    }
    let items = match items {
        Ok(items) => items,
        Err(e) => return loss(e.to_string(), None),
    };
    let mut content = Vec::new();
    let mut refused = false;
    let mut ids = BTreeSet::new();
    for item in &items {
        let ConvertedItem {
            parts,
            refused: item_refused,
        } = match convert_item(item) {
            Ok(result) => result,
            Err(detail) => return loss(detail, None),
        };
        for part in &parts {
            if let AssistantPart::ToolCall(call) = part
                && !ids.insert(call.id.as_str().to_string())
            {
                return loss("response repeats a function call_id".to_string(), None);
            }
        }
        content.extend(parts);
        refused |= item_refused;
    }
    let Some(status) = response.status.as_deref() else {
        return loss("response lacks status".to_string(), None);
    };
    let mut finish = map_terminal(
        status,
        response
            .incomplete_details
            .as_ref()
            .map(|d| d.reason.as_str()),
        tool_calls,
    );
    if matches!(finish, FinishReason::Unrecognized { .. }) {
        return loss(
            "unrecognized response terminal status or incomplete reason".to_string(),
            Some(finish),
        );
    }
    if refused {
        finish = FinishReason::Refusal;
    }
    for part in &content {
        if let AssistantPart::ToolCall(call) = part {
            emit(
                correlation,
                sink,
                ObservationFact::ToolCallProposed(call.clone()),
            );
        }
    }
    emit(
        correlation,
        sink,
        ObservationFact::FinishReported(finish.clone()),
    );
    let message_id = response.id.map(ProviderMessageId::new);
    match finish.completion_finish() {
        Some(finish) => TerminalEvidence::Completed(CompletionEvidence {
            exchange,
            message_id,
            reported_model,
            finish,
            content,
            usage,
        }),
        None => TerminalEvidence::Refused(RefusalEvidence {
            exchange,
            message_id,
            reported_model,
            content,
            usage,
            retained_input_tokens: None,
            retained_output_tokens: None,
        }),
    }
}

pub(crate) fn emit<C: Clone>(
    correlation: &C,
    sink: &mut (dyn ObservationSink<C> + Send),
    fact: ObservationFact,
) {
    sink.observe(Observation {
        correlation: correlation.clone(),
        fact,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use signalbox_model_runtime::{CompletionFinish, PROVIDER_JSON_NESTING_LIMIT};

    fn response() -> Value {
        json!({"id":"resp_fixture", "object":"response", "model":"model-fixture", "status":"completed",
            "output":[{"type":"message","id":"msg_fixture","role":"assistant","content":[{"type":"output_text","text":"ready"}]}],
            "usage":{"input_tokens":19,"output_tokens":7,"input_tokens_details":{"cached_tokens":3,"cache_write_tokens":5}}})
    }
    fn decode(value: Value) -> (TerminalEvidence, Vec<Observation<String>>) {
        let mut observations = Vec::new();
        let evidence = decode_buffered_response(
            value.to_string().as_bytes(),
            ExchangeFacts {
                http_status: Some(200),
                ..ExchangeFacts::default()
            },
            &"fixture".to_string(),
            &mut observations,
        );
        (evidence, observations)
    }

    #[test]
    fn response_identity_and_all_four_usage_axes_are_retained() {
        let (TerminalEvidence::Completed(result), _) = decode(response()) else {
            panic!("complete response decodes");
        };
        assert_eq!(
            result.message_id,
            Some(ProviderMessageId::new("resp_fixture"))
        );
        assert_eq!(
            result.usage,
            TokenUsage {
                input_tokens: Some(19),
                output_tokens: Some(7),
                cache_creation_input_tokens: Some(5),
                cache_read_input_tokens: Some(3)
            }
        );
        assert_eq!(
            result.content,
            vec![AssistantPart::Text("ready".to_string())]
        );
    }

    #[test]
    fn incomplete_reason_determines_the_terminal_disposition() {
        for (reason, expected) in [
            ("max_output_tokens", FinishReason::MaxOutputTokens),
            ("content_filter", FinishReason::Refusal),
            (
                "max_messages",
                FinishReason::Unrecognized {
                    provider_token: "max_messages".to_string(),
                },
            ),
            (
                "steered",
                FinishReason::Unrecognized {
                    provider_token: "steered".to_string(),
                },
            ),
            (
                "future",
                FinishReason::Unrecognized {
                    provider_token: "future".to_string(),
                },
            ),
        ] {
            let mut value = response();
            value["status"] = json!("incomplete");
            value["incomplete_details"] = json!({"reason":reason});
            let (evidence, _) = decode(value);
            match evidence {
                TerminalEvidence::Completed(result) => {
                    assert_eq!(FinishReason::from(result.finish), expected, "{reason}")
                }
                TerminalEvidence::Refused(_) => {
                    assert_eq!(expected, FinishReason::Refusal, "{reason}")
                }
                TerminalEvidence::BoundaryLoss(loss) => {
                    assert_eq!(loss.finish_reported, Some(expected), "{reason}")
                }
                other => panic!("unexpected terminal for {reason}: {other:?}"),
            }
        }
    }

    #[test]
    fn accepted_failed_response_never_proves_non_acceptance() {
        let mut value = response();
        value["status"] = json!("failed");
        value["error"] = json!({"code":"rate_limit_exceeded","message":"later failure"});
        let (TerminalEvidence::ProviderError(error), _) = decode(value) else {
            panic!("accepted failure is definitive provider error");
        };
        assert_eq!(
            error.kind,
            signalbox_model_runtime::ProviderErrorKind::RateLimited
        );
        assert!(!error.non_acceptance_proven);
        assert_eq!(error.native.error_token, None);
        assert_eq!(
            error.native.error_code.as_deref(),
            Some("rate_limit_exceeded")
        );
    }

    #[test]
    fn reasoning_items_are_recognized_and_dropped_without_continuity() {
        let mut value = response();
        value["output"].as_array_mut().unwrap().insert(
            0,
            json!({"type":"reasoning","id":"rs_fixture","summary":[],"encrypted_content":"opaque"}),
        );
        let (TerminalEvidence::Completed(result), observations) = decode(value) else {
            panic!("reasoning and text response decodes");
        };
        assert_eq!(
            result.content,
            vec![AssistantPart::Text("ready".to_string())]
        );
        assert!(
            !observations
                .iter()
                .any(|o| matches!(o.fact, ObservationFact::ThinkingDelta { .. }))
        );
    }

    #[test]
    fn interleaved_text_and_function_calls_keep_provider_order() {
        let mut value = response();
        value["output"] = json!([
            {"type":"function_call","id":"fc_fixture","call_id":"call_fixture","name":"lookup","arguments":"{}"},
            {"type":"message","id":"msg_fixture","role":"assistant","content":[{"type":"output_text","text":"after"}]}]);
        let (TerminalEvidence::Completed(result), _) = decode(value) else {
            panic!("ordered response decodes");
        };
        assert_eq!(result.finish, CompletionFinish::ToolUse);
        assert!(
            matches!(&result.content[0], AssistantPart::ToolCall(call) if call.id.as_str()=="call_fixture")
        );
        assert_eq!(result.content[1], AssistantPart::Text("after".to_string()));
    }

    #[test]
    fn duplicate_function_call_ids_are_boundary_loss_with_opened_tools() {
        let mut value = response();
        let call =
            json!({"type":"function_call","call_id":"same","name":"lookup","arguments":"{}"});
        value["output"] = json!([call, call]);
        let (TerminalEvidence::BoundaryLoss(loss), _) = decode(value) else {
            panic!("duplicate calls fail closed");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Opened);
    }

    #[test]
    fn malformed_tool_calls_are_not_fabricated_into_proposals() {
        for field in ["call_id", "name", "arguments"] {
            let mut call = json!({"type":"function_call","call_id":"call_fixture","name":"lookup","arguments":"{}"});
            call.as_object_mut().unwrap().remove(field);
            let mut value = response();
            value["output"] = json!([call]);
            let (evidence, observations) = decode(value);
            assert!(
                matches!(evidence, TerminalEvidence::BoundaryLoss(_)),
                "{field}"
            );
            assert!(
                !observations
                    .iter()
                    .any(|o| matches!(o.fact, ObservationFact::ToolCallProposed(_))),
                "{field}"
            );
        }
    }

    #[test]
    fn malformed_success_envelopes_are_boundary_loss() {
        for field in ["object", "id", "model", "status", "output"] {
            let mut value = response();
            value.as_object_mut().unwrap().remove(field);
            assert!(
                matches!(decode(value).0, TerminalEvidence::BoundaryLoss(_)),
                "{field}"
            );
        }
    }

    #[test]
    fn unrequested_output_kinds_fail_closed() {
        let mut value = response();
        value["output"] =
            json!([{"type":"compaction","id":"cmp_fixture","encrypted_content":"opaque"}]);
        assert!(matches!(decode(value).0, TerminalEvidence::BoundaryLoss(_)));
    }

    #[test]
    fn absent_usage_is_not_announced_or_fabricated() {
        let mut value = response();
        value.as_object_mut().unwrap().remove("usage");
        let (TerminalEvidence::Completed(result), observations) = decode(value) else {
            panic!("buffered usage may be unreported");
        };
        assert_eq!(result.usage, TokenUsage::unreported());
        assert!(
            !observations
                .iter()
                .any(|o| matches!(o.fact, ObservationFact::UsageReported(_)))
        );
    }

    #[test]
    fn refusal_content_is_never_success() {
        let mut value = response();
        value["output"][0]["content"] = json!([{"type":"refusal","refusal":"declined"}]);
        let (TerminalEvidence::Refused(result), _) = decode(value) else {
            panic!("refusal is distinct from completion");
        };
        assert_eq!(
            result.content,
            vec![AssistantPart::Text("declined".to_string())]
        );
    }

    #[test]
    fn overdeep_unknown_material_is_rejected_before_decoding() {
        let body = format!(
            "{{\"unknown\":{}0{}}}",
            "[".repeat(PROVIDER_JSON_NESTING_LIMIT),
            "]".repeat(PROVIDER_JSON_NESTING_LIMIT)
        );
        let evidence = decode_buffered_response(
            body.as_bytes(),
            ExchangeFacts::default(),
            &(),
            &mut Vec::new(),
        );
        assert!(matches!(
            evidence,
            TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                tool_calls: ToolCallsAtLoss::Unobserved,
                ..
            })
        ));
    }
}
