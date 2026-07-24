//! Buffered-response decoding and shared response-fact mapping.

use std::collections::BTreeSet;

use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CompletionEvidence, ExchangeFacts, FinishReason,
    LossCause, Observation, ObservationFact, ObservationSink, ProviderReportedModel,
    RefusalEvidence, TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal, ToolName,
    validate_provider_json_nesting,
};

use crate::{
    translate::is_valid_function_name,
    wire::{ChatCompletion, WireResponseToolCall, WireUsage},
};

/// Maps the provider's `finish_reason` token to the normalized vocabulary.
///
/// `content_filter` maps to [`FinishReason::Refusal`]: the provider filtered
/// the output, which is its refusal outcome, and the response's `refusal`
/// payload (when present) is carried as refusal evidence. The provider does
/// not distinguish a natural stop from a caller stop-sequence hit — both
/// arrive as `stop` — so a `stop` is normalized to [`FinishReason::EndTurn`]
/// only when the request declared no stop sequences. Otherwise its native
/// token is preserved as unrecognized boundary loss. `length` is also left unrecognized because OpenAI uses the same
/// token for either the requested output ceiling or the model context limit;
/// collapsing those distinct dispositions would invent evidence. The legacy
/// `function_call` token is unrecognized because this adapter never requests
/// legacy functions.
pub(crate) fn map_finish(token: &str, stop_sequences_declared: bool) -> FinishReason {
    match token {
        "stop" if !stop_sequences_declared => FinishReason::EndTurn,
        "tool_calls" => FinishReason::ToolUse,
        "content_filter" => FinishReason::Refusal,
        other => FinishReason::Unrecognized {
            provider_token: other.to_string(),
        },
    }
}

/// Converts wire usage to the neutral usage record.
///
/// `prompt_tokens_details.cached_tokens` is the provider's cache-read count;
/// Chat Completions reports no cache-creation count, so that fact stays
/// unreported rather than being fabricated as zero.
pub(crate) fn convert_usage(wire: &WireUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: wire.prompt_tokens,
        output_tokens: wire.completion_tokens,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: wire
            .prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens),
    }
}

/// Converts one response tool call into a typed proposal, or reports why it
/// is not recognizable completion material.
pub(crate) fn convert_tool_call(call: &WireResponseToolCall) -> Result<ToolCallProposal, String> {
    if call.kind.as_deref() != Some("function") {
        return Err(format!(
            "tool call carries unrecognized type {:?}",
            call.kind.as_deref().unwrap_or("<absent>")
        ));
    }
    let (Some(id), Some(function)) = (&call.id, &call.function) else {
        return Err("tool call is missing its id or function".to_string());
    };
    let Some(name) = &function.name else {
        return Err("tool call is missing its function name".to_string());
    };
    if !is_valid_function_name(name) {
        return Err(format!("tool call carries invalid function name {name:?}"));
    }
    let Some(arguments) = &function.arguments else {
        // The documented function payload always carries arguments;
        // fabricating bytes the provider never produced could turn a
        // malformed call into an executable zero-argument one.
        return Err("tool call is missing its arguments".to_string());
    };
    if let Err(error) = validate_provider_json_nesting(arguments.as_bytes()) {
        return Err(format!(
            "tool call arguments exceed the provider JSON bound: {error}"
        ));
    }
    Ok(ToolCallProposal {
        id: ToolCallId::new(id.clone()),
        name: ToolName::new(name.clone()),
        arguments_json: arguments.clone(),
    })
}

/// Decodes a complete success-status response body into terminal evidence,
/// emitting the facts it learns as observations along the way.
///
/// A body that is not the documented completion material — unparseable, not
/// exactly one choice, missing its finish reason, or carrying an
/// unrecognizable tool call — is boundary-loss evidence (per
/// `docs/spec/runtime-substrate.md`, a success status without valid
/// completion material is not definitive), with the facts observed before
/// the defect retained. A non-empty `refusal` payload or a `content_filter`
/// finish is refusal evidence, never completion.
pub(crate) fn decode_buffered_response<C: Clone>(
    body: &[u8],
    exchange: ExchangeFacts,
    correlation: &C,
    sink: &mut (dyn ObservationSink<C> + Send),
    stop_sequences_declared: bool,
) -> TerminalEvidence {
    if let Err(error) = validate_provider_json_nesting(body) {
        return unintelligible(
            format!("success response body exceeds the provider JSON bound: {error}"),
            exchange,
            None,
            TokenUsage::unreported(),
        );
    }
    let completion: ChatCompletion = match serde_json::from_slice(body) {
        Ok(completion) => completion,
        Err(error) => {
            return unintelligible(
                format!("success response body is not a chat completion: {error}"),
                exchange,
                None,
                TokenUsage::unreported(),
            );
        }
    };
    if completion.object.as_deref() != Some("chat.completion") {
        return unintelligible(
            format!(
                "success response carries object {:?}; chat.completion is required",
                completion.object.as_deref().unwrap_or("<absent>")
            ),
            exchange,
            completion.model.map(ProviderReportedModel::new),
            completion
                .usage
                .as_ref()
                .map(convert_usage)
                .unwrap_or_default(),
        );
    }
    if completion.id.is_none() {
        return unintelligible(
            "success response carries no completion id".to_string(),
            exchange,
            completion.model.map(ProviderReportedModel::new),
            completion
                .usage
                .as_ref()
                .map(convert_usage)
                .unwrap_or_default(),
        );
    }
    let Some(model) = completion.model else {
        return unintelligible(
            "success response carries no model identity".to_string(),
            exchange,
            None,
            completion
                .usage
                .as_ref()
                .map(convert_usage)
                .unwrap_or_default(),
        );
    };
    let model = ProviderReportedModel::new(model);
    let reported_model = Some(model.clone());
    sink.observe(Observation {
        correlation: correlation.clone(),
        fact: ObservationFact::ProviderModelReported(model),
    });
    let usage = completion
        .usage
        .as_ref()
        .map(convert_usage)
        .unwrap_or_default();
    let message_id = None;
    let [choice] = completion.choices.as_slice() else {
        return unintelligible(
            format!(
                "success response carries {} choices; exactly one is requested",
                completion.choices.len()
            ),
            exchange,
            reported_model,
            usage,
        );
    };
    if choice.index != Some(0) {
        return unintelligible(
            format!(
                "success response carries choice index {:?}; index 0 is requested",
                choice.index
            ),
            exchange,
            reported_model,
            usage,
        );
    }
    let Some(message) = &choice.message else {
        return unintelligible(
            "success response choice carries no message".to_string(),
            exchange,
            reported_model,
            usage,
        );
    };
    if message.role.as_deref() != Some("assistant") {
        return unintelligible(
            format!(
                "success response message carries role {:?}; assistant is required",
                message.role.as_deref().unwrap_or("<absent>")
            ),
            exchange,
            reported_model,
            usage,
        );
    }
    let mut content = Vec::new();
    if let Some(text) = &message.content
        && !text.is_empty()
    {
        content.push(AssistantPart::Text(text.clone()));
    }
    let mut tool_ids = BTreeSet::new();
    for call in &message.tool_calls {
        match convert_tool_call(call) {
            Ok(proposal) => {
                if !tool_ids.insert(proposal.id.as_str().to_string()) {
                    return unintelligible(
                        format!("response repeats tool-call id {:?}", proposal.id.as_str()),
                        exchange,
                        reported_model,
                        usage,
                    );
                }
                content.push(AssistantPart::ToolCall(proposal));
            }
            Err(detail) => return unintelligible(detail, exchange, reported_model, usage),
        }
    }
    if completion.usage.is_some() {
        // The observation claims a provider report; an absent usage member
        // stays unreported rather than being announced as all-none.
        sink.observe(Observation {
            correlation: correlation.clone(),
            fact: ObservationFact::UsageReported(usage),
        });
    }
    let Some(finish_token) = &choice.finish_reason else {
        return unintelligible(
            "success response carries no finish_reason".to_string(),
            exchange,
            reported_model,
            usage,
        );
    };
    let mut finish = map_finish(finish_token, stop_sequences_declared);
    if matches!(finish, FinishReason::Unrecognized { .. }) {
        return unintelligible_after_finish(
            "success response carries an unrecognized finish_reason".to_string(),
            exchange,
            reported_model,
            finish,
            usage,
        );
    }
    let refusal_payload = message
        .refusal
        .clone()
        .filter(|refusal| !refusal.is_empty());
    if refusal_payload.is_some() {
        finish = FinishReason::Refusal;
    }
    let has_tool_calls = content
        .iter()
        .any(|part| matches!(part, AssistantPart::ToolCall(_)));
    if (matches!(finish, FinishReason::ToolUse) && !has_tool_calls)
        || (has_tool_calls && !matches!(finish, FinishReason::ToolUse))
    {
        return unintelligible_after_finish(
            "tool-call content does not match the reported finish_reason".to_string(),
            exchange,
            reported_model,
            finish,
            usage,
        );
    }
    for part in &content {
        if let AssistantPart::ToolCall(proposal) = part {
            sink.observe(Observation {
                correlation: correlation.clone(),
                fact: ObservationFact::ToolCallProposed(proposal.clone()),
            });
        }
    }
    sink.observe(Observation {
        correlation: correlation.clone(),
        fact: ObservationFact::FinishReported(finish.clone()),
    });
    match finish.completion_finish() {
        None => {
            if let Some(refusal) = refusal_payload {
                content.push(AssistantPart::Text(refusal));
            }
            TerminalEvidence::Refused(RefusalEvidence {
                exchange,
                message_id,
                reported_model,
                content,
                usage,
            })
        }
        Some(finish) => TerminalEvidence::Completed(CompletionEvidence {
            exchange,
            message_id,
            reported_model,
            finish,
            content,
            usage,
        }),
    }
}

fn unintelligible(
    detail: String,
    exchange: ExchangeFacts,
    reported_model: Option<ProviderReportedModel>,
    usage: TokenUsage,
) -> TerminalEvidence {
    TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
        cause: LossCause::ResponseUnintelligible { detail },
        exchange,
        reported_model,
        finish_reported: None,
        usage,
    })
}

fn unintelligible_after_finish(
    detail: String,
    exchange: ExchangeFacts,
    reported_model: Option<ProviderReportedModel>,
    finish_reported: FinishReason,
    usage: TokenUsage,
) -> TerminalEvidence {
    TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
        cause: LossCause::ResponseUnintelligible { detail },
        exchange,
        reported_model,
        finish_reported: Some(finish_reported),
        usage,
    })
}

#[cfg(test)]
mod tests {
    use expect_test::expect;
    use signalbox_expect_table::table;
    use signalbox_model_runtime::{
        AssistantPart, CompletionFinish, ExchangeFacts, FinishReason, LossCause, Observation,
        ObservationFact, PROVIDER_JSON_NESTING_LIMIT, ProviderReportedModel, ProviderRequestId,
        TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal, ToolName,
    };

    use super::{decode_buffered_response, map_finish};

    fn exchange() -> ExchangeFacts {
        ExchangeFacts {
            provider_request_id: Some(ProviderRequestId::new("req_1")),
            http_status: Some(200),
        }
    }

    /// Decodes the body against canonical exchange facts, collecting
    /// observations correlated to `"call-1"`.
    fn decode(body: &str) -> (TerminalEvidence, Vec<Observation<String>>) {
        decode_with_stop_sequences(body, false)
    }

    fn decode_with_stop_sequences(
        body: &str,
        stop_sequences_declared: bool,
    ) -> (TerminalEvidence, Vec<Observation<String>>) {
        let mut observations: Vec<Observation<String>> = Vec::new();
        let evidence = decode_buffered_response(
            body.as_bytes(),
            exchange(),
            &"call-1".to_string(),
            &mut observations,
            stop_sequences_declared,
        );
        (evidence, observations)
    }

    #[test]
    fn completed_response_decodes_every_reported_fact() {
        let (evidence, observations) = decode(
            r#"{
                "id": "chatcmpl_1",
                "object": "chat.completion",
                "model": "model-exact-1",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Checking.",
                        "refusal": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "lookup", "arguments": "{\"city\":\"Oslo\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {
                    "prompt_tokens": 12,
                    "completion_tokens": 34,
                    "prompt_tokens_details": {"cached_tokens": 6}
                }
            }"#,
        );

        let TerminalEvidence::Completed(completion) = evidence else {
            panic!("a complete success chat completion must decode as completion evidence");
        };
        assert_eq!(completion.exchange, exchange());
        assert_eq!(completion.message_id, None);
        assert_eq!(
            completion.reported_model,
            Some(ProviderReportedModel::new("model-exact-1"))
        );
        assert_eq!(completion.finish, CompletionFinish::ToolUse);
        assert_eq!(
            completion.content,
            vec![
                AssistantPart::Text("Checking.".to_string()),
                AssistantPart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new("call_1"),
                    name: ToolName::new("lookup"),
                    arguments_json: r#"{"city":"Oslo"}"#.to_string(),
                }),
            ]
        );
        assert_eq!(
            completion.usage,
            TokenUsage {
                input_tokens: Some(12),
                output_tokens: Some(34),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: Some(6),
            }
        );
        assert_eq!(
            observations.first(),
            Some(&Observation {
                correlation: "call-1".to_string(),
                fact: ObservationFact::ProviderModelReported(ProviderReportedModel::new(
                    "model-exact-1"
                )),
            })
        );
    }

    #[test]
    fn refusal_payload_is_refusal_evidence_carrying_the_refusal_text() {
        let (evidence, _) = decode(
            r#"{
                "id": "chatcmpl_1",
                "object": "chat.completion",
                "model": "model-exact-1",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": null,
                                "refusal": "I cannot help with that."},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 9, "completion_tokens": 8}
            }"#,
        );

        let TerminalEvidence::Refused(refusal) = evidence else {
            panic!("a refusal payload must decode as refusal evidence, never completion");
        };
        assert_eq!(
            refusal.content,
            vec![AssistantPart::Text("I cannot help with that.".to_string())]
        );
        assert_eq!(
            refusal.reported_model,
            Some(ProviderReportedModel::new("model-exact-1"))
        );
    }

    #[test]
    fn content_filter_finish_is_refusal_evidence() {
        let (evidence, _) = decode(
            r#"{
                "id": "chatcmpl_1",
                "object": "chat.completion",
                "model": "model-exact-1",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "partial"},
                    "finish_reason": "content_filter"
                }]
            }"#,
        );

        let TerminalEvidence::Refused(refusal) = evidence else {
            panic!("a content_filter finish is the provider's refusal outcome");
        };
        assert_eq!(
            refusal.content,
            vec![AssistantPart::Text("partial".to_string())]
        );
    }

    #[test]
    fn missing_finish_reason_is_boundary_loss_with_retained_facts() {
        let (evidence, _) = decode(
            r#"{
                "id": "chatcmpl_1",
                "object": "chat.completion",
                "model": "model-exact-1",
                "choices": [{"message": {"role": "assistant", "content": "partial"}}],
                "usage": {"prompt_tokens": 3}
            }"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("a success body without finish_reason is not definitive completion material");
        };
        assert!(matches!(
            loss.cause,
            LossCause::ResponseUnintelligible { .. }
        ));
        assert_eq!(
            loss.reported_model,
            Some(ProviderReportedModel::new("model-exact-1"))
        );
        assert_eq!(loss.usage.input_tokens, Some(3));
    }

    #[test]
    fn zero_choices_is_boundary_loss() {
        let (evidence, _) = decode(
            r#"{"id":"chatcmpl_1","object":"chat.completion",
                "model":"model-exact-1","choices":[]}"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("a response without the one requested choice is not definitive");
        };
        assert!(matches!(
            loss.cause,
            LossCause::ResponseUnintelligible { .. }
        ));
    }

    #[test]
    fn a_wrong_completion_object_is_boundary_loss() {
        let (evidence, _) = decode(
            r#"{"id":"chatcmpl_1","object":"response","model":"model-exact-1",
                "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},
                "finish_reason":"stop"}]}"#,
        );

        assert!(matches!(evidence, TerminalEvidence::BoundaryLoss(_)));
    }

    #[test]
    fn tool_content_and_finish_reason_must_agree() {
        let (tool_with_stop, _) = decode(
            r#"{"id":"chatcmpl_1","object":"chat.completion","model":"model-exact-1",
                "choices":[{"index":0,
                "message":{"role":"assistant","tool_calls":[{"id":"call_1",
                "type":"function","function":{"name":"ping","arguments":"{}"}}]},
                "finish_reason":"stop"}]}"#,
        );
        let (tool_finish_without_tool, _) = decode(
            r#"{"id":"chatcmpl_1","object":"chat.completion","model":"model-exact-1",
                "choices":[{"index":0,
                "message":{"role":"assistant","content":"hi"},
                "finish_reason":"tool_calls"}]}"#,
        );

        let TerminalEvidence::BoundaryLoss(tool_with_stop) = tool_with_stop else {
            panic!("tool content with a stop finish must be boundary loss");
        };
        assert_eq!(tool_with_stop.finish_reported, Some(FinishReason::EndTurn));

        let TerminalEvidence::BoundaryLoss(tool_finish_without_tool) = tool_finish_without_tool
        else {
            panic!("a tool finish without tool content must be boundary loss");
        };
        assert_eq!(
            tool_finish_without_tool.finish_reported,
            Some(FinishReason::ToolUse)
        );
    }

    #[test]
    fn ambiguous_length_finish_is_boundary_loss_even_with_partial_tool_material() {
        let (evidence, _) = decode(
            r#"{"id":"chatcmpl_1","object":"chat.completion","model":"model-exact-1","choices":[{
                "index":0,"message":{"role":"assistant","tool_calls":[{
                "id":"call_1","type":"function","function":{"name":"lookup",
                "arguments":"{\"city\":"}}]},"finish_reason":"length"}]}"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("ambiguous finish must remain boundary-loss evidence");
        };
        assert_eq!(
            loss.finish_reported,
            Some(FinishReason::Unrecognized {
                provider_token: "length".to_string(),
            })
        );
    }

    #[test]
    fn a_non_assistant_buffered_message_is_boundary_loss() {
        let (evidence, _) = decode(
            r#"{"id":"chatcmpl_1","object":"chat.completion",
                "model":"model-exact-1","choices":[{
                "index":0,"message":{"role":"user","content":"not assistant output"},
                "finish_reason":"stop"}]}"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("a non-assistant response message must not become completion evidence");
        };
        assert!(matches!(
            loss.cause,
            LossCause::ResponseUnintelligible { .. }
        ));
    }

    #[test]
    fn unrecognized_tool_call_type_is_boundary_loss_not_silent_drop() {
        let (evidence, _) = decode(
            r#"{
                "id": "chatcmpl_1",
                "object": "chat.completion",
                "model": "model-exact-1",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant",
                                "tool_calls": [{"id": "call_1", "type": "custom",
                                                "custom": {"name": "x", "input": "y"}}]},
                    "finish_reason": "tool_calls"
                }]
            }"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("an unrecognized tool-call type must surface as evidence, never drop");
        };
        assert!(matches!(
            loss.cause,
            LossCause::ResponseUnintelligible { .. }
        ));
    }

    #[test]
    fn invalid_response_function_names_are_boundary_loss() {
        for name in ["", "has space", &"x".repeat(65)] {
            let body = format!(
                r#"{{"id":"chatcmpl_1","object":"chat.completion","model":"model-exact-1",
                    "choices":[{{"index":0,"message":{{"role":"assistant","tool_calls":[{{
                    "id":"call_1","type":"function","function":{{"name":{name:?},
                    "arguments":"{{}}"}}}}]}},"finish_reason":"tool_calls"}}]}}"#
            );
            let (evidence, _) = decode(&body);

            assert!(
                matches!(evidence, TerminalEvidence::BoundaryLoss(_)),
                "invalid name {name:?} must not become a proposal"
            );
        }
    }

    #[test]
    fn a_choice_with_an_unexpected_index_is_boundary_loss() {
        let (evidence, _) = decode(
            r#"{
                "id": "chatcmpl_1",
                "object": "chat.completion",
                "model": "model-exact-1",
                "choices": [{
                    "index": 1,
                    "message": {"role": "assistant", "content": "hi"},
                    "finish_reason": "stop"
                }]
            }"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("an unrequested choice index must not become definitive completion");
        };
        assert!(matches!(
            loss.cause,
            LossCause::ResponseUnintelligible { .. }
        ));
    }

    #[test]
    fn a_choice_without_an_index_is_boundary_loss() {
        let (evidence, _) = decode(
            r#"{"id":"chatcmpl_1","object":"chat.completion",
                "model":"model-exact-1","choices":[{
                "message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#,
        );

        assert!(matches!(evidence, TerminalEvidence::BoundaryLoss(_)));
    }

    #[test]
    fn a_tool_call_without_arguments_is_boundary_loss_not_a_fabricated_call() {
        let (evidence, _) = decode(
            r#"{
                "id": "chatcmpl_1",
                "object": "chat.completion",
                "model": "model-exact-1",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant",
                                "tool_calls": [{"id": "call_1", "type": "function",
                                                "function": {"name": "ping"}}]},
                    "finish_reason": "tool_calls"
                }]
            }"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("argument bytes the provider never produced must not be fabricated");
        };
        assert!(matches!(
            loss.cause,
            LossCause::ResponseUnintelligible { .. }
        ));
    }

    #[test]
    fn an_absent_usage_member_is_never_announced_as_a_usage_report() {
        let (evidence, observations) = decode(
            r#"{
                "id": "chatcmpl_1",
                "object": "chat.completion",
                "model": "model-exact-1",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "hi"},
                    "finish_reason": "stop"
                }]
            }"#,
        );

        assert!(matches!(evidence, TerminalEvidence::Completed(_)));
        assert!(
            !observations
                .iter()
                .any(|observation| matches!(observation.fact, ObservationFact::UsageReported(_)))
        );
    }

    #[test]
    fn a_success_response_without_model_identity_is_boundary_loss() {
        let (evidence, observations) = decode(
            r#"{
                "id": "chatcmpl_1",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "hi"},
                    "finish_reason": "stop"
                }]
            }"#,
        );

        assert!(matches!(evidence, TerminalEvidence::BoundaryLoss(_)));
        assert!(observations.is_empty());
    }

    #[test]
    fn unparseable_success_body_is_boundary_loss() {
        let (evidence, observations) = decode("<html>gateway</html>");

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("an unparseable success body is not definitive completion material");
        };
        assert!(matches!(
            loss.cause,
            LossCause::ResponseUnintelligible { .. }
        ));
        assert_eq!(loss.exchange, exchange());
        assert_eq!(observations, vec![]);
    }

    #[test]
    fn overdeep_unknown_success_material_is_response_unintelligible() {
        let nested = format!(
            "{}null{}",
            "[".repeat(PROVIDER_JSON_NESTING_LIMIT + 1),
            "]".repeat(PROVIDER_JSON_NESTING_LIMIT + 1)
        );
        let body = format!(
            r#"{{
                "id":"chatcmpl_1","object":"chat.completion","model":"model-exact-1",
                "choices":[{{"index":0,"message":{{"role":"assistant","content":"ok",
                    "future":{nested}}},"finish_reason":"stop"}}]
            }}"#
        );

        let TerminalEvidence::BoundaryLoss(loss) = decode(&body).0 else {
            panic!("overdeep unknown content must be rejected before typed parsing");
        };
        let LossCause::ResponseUnintelligible { detail } = loss.cause else {
            panic!("deep success JSON must be response-unintelligible evidence");
        };
        let expected = format!("{PROVIDER_JSON_NESTING_LIMIT}-container nesting limit");
        assert!(detail.contains(&expected));
    }

    #[test]
    fn overdeep_buffered_tool_arguments_are_response_unintelligible() {
        let depth = PROVIDER_JSON_NESTING_LIMIT + 1;
        let arguments = format!("{}null{}", "[".repeat(depth), "]".repeat(depth));
        let arguments = serde_json::to_string(&arguments).expect("fixture JSON string serializes");
        let body = format!(
            r#"{{
                "id":"chatcmpl_1","object":"chat.completion","model":"model-exact-1",
                "choices":[{{"index":0,"message":{{"role":"assistant","tool_calls":[{{
                    "id":"call_1","type":"function","function":{{
                        "name":"lookup","arguments":{arguments}
                    }}
                }}]}},"finish_reason":"tool_calls"}}]
            }}"#
        );

        let TerminalEvidence::BoundaryLoss(loss) = decode(&body).0 else {
            panic!("overdeep buffered tool arguments must not become a proposal");
        };
        let LossCause::ResponseUnintelligible { detail } = loss.cause else {
            panic!("deep buffered arguments must be response-unintelligible evidence");
        };
        let expected = format!("{PROVIDER_JSON_NESTING_LIMIT}-container nesting limit");
        assert!(detail.contains(&expected));
    }

    #[test]
    fn shallow_additive_fields_remain_tolerated() {
        let (evidence, _) = decode(
            r#"{
                "id":"chatcmpl_1","object":"chat.completion","model":"model-exact-1",
                "future_envelope":{"enabled":true},
                "choices":[{"index":0,"message":{"role":"assistant","content":"ok",
                    "future_message":[1,2]},"finish_reason":"stop"}]
            }"#,
        );

        assert!(matches!(evidence, TerminalEvidence::Completed(_)));
    }

    #[test]
    fn duplicate_tool_call_ids_are_boundary_loss() {
        let (evidence, _) = decode(
            r#"{"id":"chatcmpl_1","object":"chat.completion","model":"model-exact-1",
                "choices":[{"index":0,
                "message":{"role":"assistant","tool_calls":[
                    {"id":"call_1","type":"function",
                     "function":{"name":"first","arguments":"{}"}},
                    {"id":"call_1","type":"function",
                     "function":{"name":"second","arguments":"{}"}}]},
                "finish_reason":"tool_calls"}]}"#,
        );

        assert!(matches!(evidence, TerminalEvidence::BoundaryLoss(_)));
    }

    #[test]
    fn a_success_response_without_a_completion_id_is_boundary_loss() {
        let (evidence, _) = decode(
            r#"{"object":"chat.completion","model":"model-exact-1","choices":[{
                "index":0,"message":{"role":"assistant","content":"hi"},
                "finish_reason":"stop"}]}"#,
        );

        assert!(matches!(evidence, TerminalEvidence::BoundaryLoss(_)));
    }

    #[test]
    fn stop_with_a_declared_sequence_preserves_boundary_ambiguity() {
        let (evidence, _) = decode_with_stop_sequences(
            r#"{"id":"chatcmpl_1","object":"chat.completion","model":"model-exact-1",
                "choices":[{"index":0,"message":{"role":"assistant","content":"partial"},
                "finish_reason":"stop"}]}"#,
            true,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("ambiguous finish must remain boundary-loss evidence");
        };
        assert_eq!(
            loss.finish_reported,
            Some(FinishReason::Unrecognized {
                provider_token: "stop".to_string(),
            })
        );
    }

    #[derive(Debug)]
    #[allow(
        dead_code,
        reason = "the table renderer reads every field through the Debug derive"
    )]
    struct FinishRow {
        token: &'static str,
        finish: String,
    }

    /// Renders one mapping row per finish-reason token, in the given order.
    fn finish_rows(tokens: &[&'static str]) -> Vec<FinishRow> {
        tokens
            .iter()
            .map(|token| FinishRow {
                token,
                finish: format!("{:?}", map_finish(token, false)),
            })
            .collect()
    }

    #[test]
    fn every_documented_finish_reason_maps_and_unknown_is_retained_verbatim() {
        let rows = finish_rows(&[
            "stop",
            "length",
            "tool_calls",
            "content_filter",
            "function_call",
        ]);

        expect![[r#"
            ┌────────────────┬────────────────────────────────────────────────────┐
            │ token          │ finish                                             │
            ├────────────────┼────────────────────────────────────────────────────┤
            │ stop           │ EndTurn                                            │
            │ length         │ Unrecognized { provider_token: \"length\" }        │
            │ tool_calls     │ ToolUse                                            │
            │ content_filter │ Refusal                                            │
            │ function_call  │ Unrecognized { provider_token: \"function_call\" } │
            └────────────────┴────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table(rows));
    }
}
