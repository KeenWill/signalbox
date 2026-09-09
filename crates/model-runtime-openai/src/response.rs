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

pub(crate) fn output_tool_calls(
    output: Option<&[Box<serde_json::value::RawValue>]>,
) -> ToolCallsAtLoss {
    let Some(items) = output else {
        return ToolCallsAtLoss::Unobserved;
    };
    let mut census = ToolCallsAtLoss::NoneOpened;
    for raw in items {
        match serde_json::from_str::<WireOutputItem>(raw.get()) {
            Ok(item) if item.kind == "function_call" => return ToolCallsAtLoss::Opened,
            Ok(item) if matches!(item.kind.as_str(), "message" | "reasoning") => {}
            _ => census = ToolCallsAtLoss::Unobserved,
        }
    }
    census
}

pub(crate) fn convert_tool_call(
    item: &WireOutputItem,
    response_status: &str,
) -> Result<ToolCallProposal, String> {
    if item.status.as_deref() != Some("completed")
        && !(response_status == "incomplete" && item.status.as_deref() == Some("incomplete"))
    {
        return Err("function call item status disagrees with its terminal response".to_string());
    }
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

/// Converts one terminal output item while retaining opaque reasoning bytes.
fn convert_item(
    item: &WireOutputItem,
    raw: &serde_json::value::RawValue,
    response_status: &str,
) -> Result<ConvertedItem, String> {
    match item.kind.as_str() {
        "reasoning" => {
            if let Some(status) = item.status.as_deref()
                && status != "completed"
                && !(response_status == "incomplete" && status == "incomplete")
            {
                return Err(
                    "reasoning item status disagrees with its terminal response".to_string()
                );
            }
            Ok(ConvertedItem {
                parts: item
                    .encrypted_content
                    .as_ref()
                    .map(|_| AssistantPart::ProviderReasoning {
                        item_json: raw.get().to_string(),
                    })
                    .into_iter()
                    .collect(),
                refused: false,
            })
        }
        "function_call" => Ok(ConvertedItem {
            parts: vec![AssistantPart::ToolCall(convert_tool_call(
                item,
                response_status,
            )?)],
            refused: false,
        }),
        "message" => {
            if item.status.as_deref() != Some("completed")
                && !(response_status == "incomplete"
                    && item.status.as_deref() == Some("incomplete"))
            {
                return Err(
                    "output message item status disagrees with its terminal response".to_string(),
                );
            }
            if item.role.as_deref() != Some("assistant") {
                return Err("output message must have assistant role".to_string());
            }
            let Some(parts) = &item.content else {
                return Err("output message lacks content".to_string());
            };
            let mut content = Vec::new();
            let mut refused = false;
            for part in parts {
                let text = part.text().ok_or("unrecognized output content type")?;
                refused |= matches!(part, WireContent::Refusal { .. });
                if !text.is_empty() {
                    content.push(AssistantPart::Text(text.to_string()));
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
        Ok(response) => decode_response(
            response,
            TokenUsage::unreported(),
            exchange,
            correlation,
            sink,
        ),
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
    mut response: Response,
    mut usage: TokenUsage,
    exchange: ExchangeFacts,
    correlation: &C,
    sink: &mut (dyn ObservationSink<C> + Send),
) -> TerminalEvidence {
    let reported_model = response.model.clone().map(ProviderReportedModel::new);
    let reported_usage = response.reported_usage();
    if let Ok(Some(reported)) = &reported_usage {
        usage.absorb(convert_usage(reported));
    }
    if let Some(model) = &reported_model {
        emit(
            correlation,
            sink,
            ObservationFact::ProviderModelReported(model.clone()),
        );
    }
    if reported_usage.as_ref().is_ok_and(Option::is_some) || usage != TokenUsage::unreported() {
        emit(correlation, sink, ObservationFact::UsageReported(usage));
    }
    if response.status.as_deref() == Some("failed")
        && let Some(error) = response.error.take()
    {
        return provider_error(error, exchange, reported_model, usage);
    }
    let output = response.output_items();
    let tool_calls = output_tool_calls(output.as_ref().ok().and_then(|items| items.as_deref()));
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
    if response.status.as_deref() == Some("failed") {
        return loss("failed response lacks error".to_string(), None);
    }
    if matches!(response.status.as_deref(), Some("completed" | "incomplete"))
        && response.error.is_some()
    {
        return loss("non-failed response carries an error".to_string(), None);
    }
    if response.status.as_deref() == Some("completed") && response.incomplete_details.is_some() {
        return loss(
            "completed response carries incomplete details".to_string(),
            None,
        );
    }
    if let Err(error) = reported_usage {
        let finish = response
            .status
            .as_deref()
            .map(|status| {
                map_terminal(
                    status,
                    response
                        .incomplete_details
                        .as_ref()
                        .map(|details| details.reason.as_str()),
                    tool_calls,
                )
            })
            .filter(|finish| !matches!(finish, FinishReason::Unrecognized { .. }));
        return loss(error.to_string(), finish);
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
    let output = match output {
        Ok(output) => output.unwrap_or_default(),
        Err(error) => return loss(error.to_string(), None),
    };
    let items: Result<Vec<WireOutputItem>, _> = output
        .iter()
        .map(|raw| serde_json::from_str(raw.get()))
        .collect();
    let items = match items {
        Ok(items) => items,
        Err(e) => return loss(e.to_string(), None),
    };
    let mut content = Vec::new();
    let mut refused = false;
    let mut ids = BTreeSet::new();
    let Some(status) = response.status.as_deref() else {
        return loss("response lacks status".to_string(), None);
    };
    if items
        .iter()
        .any(|item| item.id.as_deref().is_none_or(str::is_empty))
    {
        return loss(
            "response output item lacks a non-empty id".to_string(),
            None,
        );
    }
    let mut finish = map_terminal(
        status,
        response
            .incomplete_details
            .as_ref()
            .map(|d| d.reason.as_str()),
        tool_calls,
    );
    let finish_at_loss =
        (!matches!(finish, FinishReason::Unrecognized { .. })).then(|| finish.clone());
    for (item, raw) in items.iter().zip(&output) {
        let ConvertedItem {
            parts,
            refused: item_refused,
        } = match convert_item(item, raw, status) {
            Ok(result) => result,
            Err(detail) => return loss(detail, finish_at_loss.clone()),
        };
        for part in &parts {
            if let AssistantPart::ToolCall(call) = part
                && !ids.insert(call.id.as_str().to_string())
            {
                return loss(
                    "response repeats a function call_id".to_string(),
                    finish_at_loss.clone(),
                );
            }
        }
        content.extend(parts);
        refused |= item_refused;
    }
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
            reason: if response
                .incomplete_details
                .as_ref()
                .map(|details| details.reason.as_str())
                == Some("content_filter")
            {
                signalbox_model_runtime::RefusalReason::ContentPolicy
            } else {
                signalbox_model_runtime::RefusalReason::Unspecified
            },
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
            "output":[{"type":"message","id":"msg_fixture","status":"completed","role":"assistant","content":[{"type":"output_text","text":"ready"}]}],
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
    fn unknown_output_kinds_withhold_the_no_tool_claim_but_keep_known_calls_open() {
        for known_call in [false, true] {
            let mut value = response();
            value["output"] = json!([{"type":"web_search_call","id":"ws_fixture"}]);
            if known_call {
                value["output"].as_array_mut().unwrap().push(json!({
                    "type":"function_call","id":"fc_fixture","status":"completed","call_id":"call_fixture","name":"lookup","arguments":"{}"
                }));
            }
            let (TerminalEvidence::BoundaryLoss(loss), _) = decode(value) else {
                panic!("unknown output must fail closed");
            };
            assert_eq!(
                loss.tool_calls,
                if known_call {
                    ToolCallsAtLoss::Opened
                } else {
                    ToolCallsAtLoss::Unobserved
                }
            );
        }
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
    fn accepted_invalid_prompt_failure_is_an_invalid_request_without_proof() {
        let mut value = response();
        value["status"] = json!("failed");
        value["error"] = json!({"code":"invalid_prompt","message":"prompt rejected"});
        let (TerminalEvidence::ProviderError(error), _) = decode(value) else {
            panic!("accepted failure must retain definitive provider-error evidence");
        };
        assert_eq!(
            error.kind,
            signalbox_model_runtime::ProviderErrorKind::InvalidRequest
        );
        assert_eq!(error.native.error_code.as_deref(), Some("invalid_prompt"));
        assert_eq!(error.exchange.http_status, Some(200));
        assert!(!error.non_acceptance_proven);
    }

    #[test]
    fn buffered_function_call_content_respects_terminal_response_status() {
        for status in ["completed", "incomplete"] {
            for item_status in [
                None,
                Some("in_progress"),
                Some("incomplete"),
                Some("future"),
                Some("completed"),
            ] {
                let mut value = response();
                value["status"] = json!(status);
                if status == "incomplete" {
                    value["incomplete_details"] = json!({"reason":"max_output_tokens"});
                }
                let arguments = if status == "incomplete" && item_status == Some("incomplete") {
                    r#"{"query":"part"#
                } else {
                    "{}"
                };
                let mut call = json!({"type":"function_call","id":"fc_fixture","call_id":"call_fixture","name":"lookup","arguments":"{}"});
                call["arguments"] = json!(arguments);
                if let Some(item_status) = item_status {
                    call["status"] = json!(item_status);
                }
                value["output"] = json!([call]);
                let (evidence, observations) = decode(value);
                if item_status == Some("completed")
                    || (status == "incomplete" && item_status == Some("incomplete"))
                {
                    let TerminalEvidence::Completed(result) = evidence else {
                        panic!("matching function-call status must retain terminal content");
                    };
                    assert_eq!(
                        result.finish,
                        if status == "incomplete" {
                            CompletionFinish::MaxOutputTokens
                        } else {
                            CompletionFinish::ToolUse
                        }
                    );
                    assert!(
                        matches!(result.content.as_slice(), [signalbox_model_runtime::AssistantPart::ToolCall(call)]
                        if call.id.as_str() == "call_fixture" && call.arguments_json == arguments)
                    );
                    assert!(
                        observations
                            .iter()
                            .any(|o| matches!(o.fact, ObservationFact::ToolCallProposed(_)))
                    );
                } else {
                    assert!(matches!(
                        evidence,
                        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                            tool_calls: ToolCallsAtLoss::Opened,
                            ..
                        })
                    ));
                    assert!(!observations.iter().any(|o| matches!(
                        o.fact,
                        ObservationFact::FinishReported(_) | ObservationFact::ToolCallProposed(_)
                    )));
                }
            }
        }
    }

    #[test]
    fn buffered_message_content_respects_terminal_response_status() {
        for status in ["completed", "incomplete"] {
            for item_status in [
                None,
                Some("in_progress"),
                Some("incomplete"),
                Some("future"),
                Some("completed"),
            ] {
                let mut value = response();
                value["status"] = json!(status);
                if status == "incomplete" {
                    value["incomplete_details"] = json!({"reason":"max_output_tokens"});
                }
                if let Some(item_status) = item_status {
                    value["output"][0]["status"] = json!(item_status);
                } else {
                    value["output"][0].as_object_mut().unwrap().remove("status");
                }
                let (evidence, observations) = decode(value);
                if item_status == Some("completed")
                    || (status == "incomplete" && item_status == Some("incomplete"))
                {
                    let TerminalEvidence::Completed(result) = evidence else {
                        panic!("matching terminal message content must decode");
                    };
                    assert_eq!(
                        result.finish,
                        if status == "incomplete" {
                            CompletionFinish::MaxOutputTokens
                        } else {
                            CompletionFinish::EndTurn
                        }
                    );
                    assert_eq!(
                        result.content,
                        vec![AssistantPart::Text("ready".to_string())]
                    );
                } else {
                    assert!(matches!(
                        evidence,
                        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                            cause: LossCause::ResponseUnintelligible { .. },
                            ..
                        })
                    ));
                    assert!(!observations.iter().any(|o| matches!(
                        o.fact,
                        ObservationFact::FinishReported(_) | ObservationFact::ToolCallProposed(_)
                    )));
                }
            }
        }
    }

    #[test]
    fn failed_envelope_keeps_provider_error_when_output_is_not_an_array() {
        for output in [json!({}), json!("invalid"), json!(42)] {
            let mut value = response();
            value["status"] = json!("failed");
            value["error"] = json!({"code":"server_error","message":"generation failed"});
            value["output"] = output;
            let (TerminalEvidence::ProviderError(error), observations) = decode(value) else {
                panic!("malformed output must not erase the failed envelope");
            };
            assert_eq!(
                error.kind,
                signalbox_model_runtime::ProviderErrorKind::ProviderInternal
            );
            assert_eq!(error.native.error_code.as_deref(), Some("server_error"));
            assert_eq!(error.native.message.as_deref(), Some("generation failed"));
            assert_eq!(error.exchange.http_status, Some(200));
            assert!(!error.non_acceptance_proven);
            assert!(!observations.iter().any(|o| matches!(
                o.fact,
                ObservationFact::FinishReported(_) | ObservationFact::ToolCallProposed(_)
            )));
        }
    }

    #[test]
    fn completed_and_incomplete_envelopes_require_no_error_and_array_output() {
        for status in ["completed", "incomplete"] {
            for error in [Value::Null, json!({}), json!({"code":"server_error"})] {
                let mut value = response();
                value["status"] = json!(status);
                if status == "incomplete" {
                    value["incomplete_details"] = json!({"reason":"max_output_tokens"});
                }
                value["error"] = error.clone();
                let (evidence, observations) = decode(value.clone());
                if error.is_null() {
                    assert!(matches!(evidence, TerminalEvidence::Completed(_)));
                } else {
                    assert!(matches!(evidence, TerminalEvidence::BoundaryLoss(_)));
                    assert!(!observations.iter().any(|o| matches!(
                        o.fact,
                        ObservationFact::FinishReported(_) | ObservationFact::ToolCallProposed(_)
                    )));
                }
                value["output"] = json!({});
                assert!(matches!(decode(value).0, TerminalEvidence::BoundaryLoss(_)));
            }
        }
    }

    #[test]
    fn completed_responses_reject_incomplete_details_before_announcing_a_finish() {
        for tool in [false, true] {
            for details in [
                Value::Null,
                json!({"reason":"max_output_tokens"}),
                json!({"reason":"content_filter"}),
            ] {
                let mut value = response();
                if tool {
                    value["output"] = json!([{"type":"function_call","id":"fc_fixture","status":"completed","call_id":"call_fixture","name":"lookup","arguments":"{}"}]);
                }
                value["incomplete_details"] = details.clone();
                let (evidence, observations) = decode(value);
                if details.is_null() {
                    let TerminalEvidence::Completed(result) = evidence else {
                        panic!("null incomplete details are valid");
                    };
                    assert_eq!(
                        result.finish,
                        if tool {
                            CompletionFinish::ToolUse
                        } else {
                            CompletionFinish::EndTurn
                        }
                    );
                } else {
                    let TerminalEvidence::BoundaryLoss(loss) = evidence else {
                        panic!("completed response with incomplete details must fail closed");
                    };
                    assert!(matches!(
                        loss.cause,
                        LossCause::ResponseUnintelligible { .. }
                    ));
                    assert_eq!(loss.finish_reported, None);
                    assert_eq!(
                        loss.tool_calls,
                        if tool {
                            ToolCallsAtLoss::Opened
                        } else {
                            ToolCallsAtLoss::NoneOpened
                        }
                    );
                    assert!(!observations.iter().any(|o| matches!(
                        o.fact,
                        ObservationFact::FinishReported(_) | ObservationFact::ToolCallProposed(_)
                    )));
                }
            }
        }
    }

    #[test]
    fn encrypted_reasoning_items_are_retained_in_output_order() {
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
            vec![AssistantPart::ProviderReasoning {
                item_json: json!({"type":"reasoning","id":"rs_fixture","summary":[],"encrypted_content":"opaque"}).to_string(),
            }, AssistantPart::Text("ready".to_string())]
        );
        assert!(
            !observations
                .iter()
                .any(|o| matches!(o.fact, ObservationFact::ThinkingDelta { .. }))
        );
    }

    #[test]
    fn every_buffered_output_item_requires_a_nonempty_id() {
        for status in ["completed", "incomplete"] {
            for item in [
                json!({"type":"message","id":"msg_second","status":"completed","role":"assistant","content":[{"type":"output_text","text":"after"}]}),
                json!({"type":"reasoning","id":"rs_fixture","summary":[]}),
                json!({"type":"function_call","id":"fc_fixture","status":"completed","call_id":"call_fixture","name":"lookup","arguments":"{}"}),
            ] {
                let mut value = response();
                value["status"] = json!(status);
                if status == "incomplete" {
                    value["incomplete_details"] = json!({"reason":"max_output_tokens"});
                }
                value["output"].as_array_mut().unwrap().push(item.clone());
                assert!(matches!(
                    decode(value.clone()).0,
                    TerminalEvidence::Completed(_)
                ));
                for id in [None, Some(Value::Null), Some(json!(""))] {
                    let mut invalid = value.clone();
                    if let Some(id) = id {
                        invalid["output"][1]["id"] = id;
                    } else {
                        invalid["output"][1].as_object_mut().unwrap().remove("id");
                    }
                    let (TerminalEvidence::BoundaryLoss(loss), observations) = decode(invalid)
                    else {
                        panic!("missing or empty item IDs must fail closed for {status}: {item}");
                    };
                    assert!(matches!(
                        loss.cause,
                        LossCause::ResponseUnintelligible { .. }
                    ));
                    assert_eq!(
                        loss.tool_calls,
                        if item["type"] == "function_call" {
                            ToolCallsAtLoss::Opened
                        } else {
                            ToolCallsAtLoss::NoneOpened
                        }
                    );
                    assert!(!observations.iter().any(|o| matches!(
                        o.fact,
                        ObservationFact::FinishReported(_) | ObservationFact::ToolCallProposed(_)
                    )));
                }
            }
        }
    }

    #[test]
    fn interleaved_text_and_function_calls_keep_provider_order() {
        let mut value = response();
        value["output"] = json!([
            {"type":"function_call","id":"fc_fixture","status":"completed","call_id":"call_fixture","name":"lookup","arguments":"{}"},
            {"type":"message","id":"msg_fixture","status":"completed","role":"assistant","content":[{"type":"output_text","text":"after"}]}]);
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
        let call = json!({"type":"function_call","id":"fc_fixture","status":"completed","call_id":"same","name":"lookup","arguments":"{}"});
        value["output"] = json!([call, call]);
        let (TerminalEvidence::BoundaryLoss(loss), _) = decode(value) else {
            panic!("duplicate calls fail closed");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Opened);
    }

    #[test]
    fn malformed_tool_calls_are_not_fabricated_into_proposals() {
        for field in ["call_id", "name", "arguments"] {
            let mut call = json!({"type":"function_call","id":"fc_fixture","status":"completed","call_id":"call_fixture","name":"lookup","arguments":"{}"});
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
            result.reason,
            signalbox_model_runtime::RefusalReason::Unspecified
        );
        assert_eq!(
            result.content,
            vec![AssistantPart::Text("declined".to_string())]
        );
    }

    #[test]
    fn malformed_usage_preserves_buffered_terminal_facts() {
        let mut value = response();
        value["status"] = json!("incomplete");
        value["incomplete_details"] = json!({"reason":"max_output_tokens"});
        value["usage"] = json!([]);
        let (TerminalEvidence::BoundaryLoss(loss), observations) = decode(value) else {
            panic!("malformed usage must remain boundary loss");
        };
        assert_eq!(loss.finish_reported, Some(FinishReason::MaxOutputTokens));
        assert_eq!(
            loss.reported_model,
            Some(ProviderReportedModel::new("model-fixture"))
        );
        assert_eq!(loss.usage, TokenUsage::unreported());
        assert!(!observations.iter().any(|observation| matches!(
            observation.fact,
            ObservationFact::UsageReported(_) | ObservationFact::FinishReported(_)
        )));
    }

    #[test]
    fn content_filter_retains_its_typed_refusal_reason() {
        let mut value = response();
        value["status"] = json!("incomplete");
        value["incomplete_details"] = json!({"reason":"content_filter"});
        let (TerminalEvidence::Refused(result), _) = decode(value) else {
            panic!("content filtering is refusal evidence");
        };
        assert_eq!(
            result.reason,
            signalbox_model_runtime::RefusalReason::ContentPolicy
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
    #[test]
    fn buffered_item_conversion_loss_retains_only_recognized_terminal_finishes() {
        for defect in ["role", "call_id", "name", "arguments", "duplicate_call_id"] {
            for reason in [
                None,
                Some("max_output_tokens"),
                Some("content_filter"),
                Some("future"),
            ] {
                let has_tools = defect != "role";
                let mut first = json!({"type":"function_call","id":"fc_first","status":"completed","call_id":"call_first","name":"lookup","arguments":"{}"});
                let mut second = json!({"type":"function_call","id":"fc_second","status":"completed","call_id":"call_second","name":"lookup","arguments":"{}"});
                if defect == "role" {
                    first = json!({"type":"message","id":"msg_first","status":"completed","role":"assistant","content":[{"type":"output_text","text":"ready"}]});
                    second = first.clone();
                    second["id"] = json!("msg_second");
                    second["role"] = json!("user");
                } else if defect == "duplicate_call_id" {
                    second["call_id"] = first["call_id"].clone();
                } else {
                    second.as_object_mut().unwrap().remove(defect);
                }
                let status = if reason.is_some() {
                    "incomplete"
                } else {
                    "completed"
                };
                let mut value = response();
                value["status"] = json!(status);
                if let Some(reason) = reason {
                    value["incomplete_details"] = json!({"reason":reason});
                }
                value["output"] = json!([first, second]);
                let (TerminalEvidence::BoundaryLoss(loss), observations) = decode(value) else {
                    panic!("invalid output must remain boundary loss");
                };
                let expected = match reason {
                    None if has_tools => Some(FinishReason::ToolUse),
                    None => Some(FinishReason::EndTurn),
                    Some("max_output_tokens") => Some(FinishReason::MaxOutputTokens),
                    Some("content_filter") => Some(FinishReason::Refusal),
                    _ => None,
                };
                assert_eq!(loss.finish_reported, expected, "{defect} {reason:?}");
                assert_eq!(
                    loss.tool_calls,
                    if has_tools {
                        ToolCallsAtLoss::Opened
                    } else {
                        ToolCallsAtLoss::NoneOpened
                    }
                );
                assert!(matches!(
                    loss.cause,
                    LossCause::ResponseUnintelligible { .. }
                ));
                assert!(!observations.iter().any(|o| matches!(
                    o.fact,
                    ObservationFact::ToolCallProposed(_) | ObservationFact::FinishReported(_)
                )));
            }
        }
    }
    #[test]
    fn buffered_reasoning_status_must_agree_with_its_terminal_response() {
        for status in ["completed", "incomplete"] {
            for item_status in [
                None,
                Some("completed"),
                Some("incomplete"),
                Some("in_progress"),
                Some("future"),
            ] {
                for tool in [false, true] {
                    let mut reasoning = json!({"type":"reasoning","id":"rs_fixture","summary":[]});
                    if let Some(item_status) = item_status {
                        reasoning["status"] = json!(item_status);
                    }
                    let content = if tool {
                        json!({"type":"function_call","id":"fc_fixture","status":"completed","call_id":"call_fixture","name":"lookup","arguments":"{}"})
                    } else {
                        json!({"type":"message","id":"msg_fixture","status":"completed","role":"assistant","content":[{"type":"output_text","text":"ready"}]})
                    };
                    let mut value = response();
                    value["status"] = json!(status);
                    if status == "incomplete" {
                        value["incomplete_details"] = json!({"reason":"max_output_tokens"});
                    }
                    value["output"] = json!([reasoning, content]);
                    let (evidence, observations) = decode(value);
                    let finish = if status == "incomplete" {
                        FinishReason::MaxOutputTokens
                    } else if tool {
                        FinishReason::ToolUse
                    } else {
                        FinishReason::EndTurn
                    };
                    if item_status.is_none()
                        || item_status == Some("completed")
                        || (status == "incomplete" && item_status == Some("incomplete"))
                    {
                        let TerminalEvidence::Completed(result) = evidence else {
                            panic!("consistent reasoning status must permit terminal content");
                        };
                        assert_eq!(FinishReason::from(result.finish), finish);
                        assert_eq!(result.content.len(), 1);
                    } else {
                        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
                            panic!("contradictory reasoning status must fail closed");
                        };
                        assert!(matches!(
                            loss.cause,
                            LossCause::ResponseUnintelligible { .. }
                        ));
                        assert_eq!(loss.finish_reported, Some(finish));
                        assert_eq!(
                            loss.tool_calls,
                            if tool {
                                ToolCallsAtLoss::Opened
                            } else {
                                ToolCallsAtLoss::NoneOpened
                            }
                        );
                        assert!(!observations.iter().any(|o| matches!(
                            o.fact,
                            ObservationFact::FinishReported(_)
                                | ObservationFact::ToolCallProposed(_)
                        )));
                    }
                    assert!(
                        !observations
                            .iter()
                            .any(|o| matches!(o.fact, ObservationFact::ThinkingDelta { .. }))
                    );
                }
            }
        }
    }
    #[test]
    fn buffered_failed_envelopes_survive_malformed_ancillary_fields() {
        for status in ["failed", "completed", "incomplete"] {
            for field in ["id", "object", "model", "usage", "incomplete_details"] {
                for malformed in [
                    json!(42),
                    json!([]),
                    json!({"input_tokens":"invalid","reason":42}),
                ] {
                    let mut value = response();
                    value["status"] = json!(status);
                    if status == "failed" {
                        value["error"] =
                            json!({"code":"server_error","message":"generation failed"});
                    }
                    if status == "incomplete" {
                        value["incomplete_details"] = json!({"reason":"max_output_tokens"});
                    }
                    value[field] = malformed;
                    let (evidence, observations) = decode(value);
                    if status == "failed" {
                        let TerminalEvidence::ProviderError(error) = evidence else {
                            panic!("ancillary {field} must not erase the failed envelope");
                        };
                        assert_eq!(
                            error.kind,
                            signalbox_model_runtime::ProviderErrorKind::ProviderInternal
                        );
                        assert_eq!(error.native.error_code.as_deref(), Some("server_error"));
                        assert_eq!(error.native.message.as_deref(), Some("generation failed"));
                        assert_eq!(error.exchange.http_status, Some(200));
                        assert!(!error.non_acceptance_proven);
                        let expected = if field == "usage" {
                            TokenUsage::unreported()
                        } else {
                            TokenUsage {
                                input_tokens: Some(19),
                                output_tokens: Some(7),
                                cache_read_input_tokens: Some(3),
                                cache_creation_input_tokens: Some(5),
                            }
                        };
                        assert_eq!(error.usage, expected);
                        let observed_usage = observations.iter().rev().find_map(|o| match o.fact {
                            ObservationFact::UsageReported(usage) => Some(usage),
                            _ => None,
                        });
                        assert_eq!(
                            observed_usage,
                            (expected != TokenUsage::unreported()).then_some(expected)
                        );
                    } else {
                        assert!(
                            matches!(evidence, TerminalEvidence::BoundaryLoss(_)),
                            "{status} {field}"
                        );
                    }
                    assert!(!observations.iter().any(|o| matches!(
                        o.fact,
                        ObservationFact::FinishReported(_) | ObservationFact::ToolCallProposed(_)
                    )));
                }
            }
        }
    }
    #[test]
    fn failed_status_and_error_suffice_without_valid_ancillary_fields() {
        for ancillary in [
            json!({}),
            json!({
                "id":42,"object":[],"model":{},"output":{},"usage":"invalid","incomplete_details":[]
            }),
        ] {
            let mut value = ancillary;
            value["status"] = json!("failed");
            value["error"] = json!({"code":"invalid_prompt","message":"rejected prompt"});
            let (TerminalEvidence::ProviderError(error), observations) = decode(value) else {
                panic!("intact failure must take precedence over every ancillary field");
            };
            assert_eq!(
                error.kind,
                signalbox_model_runtime::ProviderErrorKind::InvalidRequest
            );
            assert_eq!(error.native.error_code.as_deref(), Some("invalid_prompt"));
            assert_eq!(error.native.message.as_deref(), Some("rejected prompt"));
            assert_eq!(error.exchange.http_status, Some(200));
            assert!(!error.non_acceptance_proven);
            assert_eq!(error.reported_model, None);
            assert_eq!(error.usage, TokenUsage::unreported());
            assert!(observations.is_empty());
        }
    }
}
