//! Responses SSE payload dispatch and terminal integrity.
//! A terminal response event, rather than EOF, completes the exchange.

use std::collections::BTreeMap;

use signalbox_model_runtime::{
    BoundaryLossEvidence, ExchangeFacts, LossCause, ObservationFact, ObservationSink,
    ProviderJsonNestingValidator, ProviderReportedModel, SseRecord, StreamInterruption,
    TerminalEvidence, TokenUsage, ToolCallsAtLoss, validate_provider_json_nesting,
};

use crate::response::{convert_usage, decode_response, emit, provider_error};
use crate::wire::{Response, ResponseError, ResponseEvent, WireOutputItem};

pub(crate) enum StreamStep {
    Continue,
    Terminal(Box<TerminalEvidence>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaterRecords {
    Unapplied,
    AllApplied,
}

pub(crate) struct StreamDecoder {
    exchange: ExchangeFacts,
    response_id: Option<String>,
    reported_model: Option<ProviderReportedModel>,
    usage: TokenUsage,
    opened_tool_calls: bool,
    discarded_unexamined_bytes: bool,
    later_records: LaterRecords,
    item_ids: BTreeMap<u32, String>,
    argument_nesting: BTreeMap<u32, ProviderJsonNestingValidator>,
}

impl StreamDecoder {
    pub(crate) fn new(exchange: ExchangeFacts) -> Self {
        Self {
            exchange,
            response_id: None,
            reported_model: None,
            usage: TokenUsage::unreported(),
            opened_tool_calls: false,
            discarded_unexamined_bytes: false,
            later_records: LaterRecords::AllApplied,
            item_ids: BTreeMap::new(),
            argument_nesting: BTreeMap::new(),
        }
    }

    pub(crate) fn apply<C: Clone>(
        &mut self,
        record: &SseRecord,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) -> StreamStep {
        if let Err(e) = validate_provider_json_nesting(record.data.as_bytes()) {
            return StreamStep::Terminal(Box::new(
                self.undecoded_violation_evidence(e.to_string()),
            ));
        }
        let event: ResponseEvent = match serde_json::from_str(&record.data) {
            Ok(event) => event,
            Err(e) => {
                return StreamStep::Terminal(Box::new(
                    self.undecoded_violation_evidence(e.to_string()),
                ));
            }
        };
        if event.kind.starts_with("response.function_call_arguments.") {
            self.opened_tool_calls = true;
        }
        match event.kind.as_str() {
            "error" => StreamStep::Terminal(Box::new(provider_error(
                ResponseError {
                    code: event.code,
                    message: event.message,
                },
                self.exchange.clone(),
                self.reported_model.clone(),
                self.usage,
            ))),
            "response.created" | "response.in_progress" | "response.queued" => {
                let Some(response) = event.response else {
                    return self.violation("lifecycle event lacks response");
                };
                if let Err(detail) = self.observe_response(&response, correlation, sink) {
                    return self.violation(detail);
                }
                StreamStep::Continue
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                let Some(response) = event.response else {
                    return self.violation("terminal event lacks response");
                };
                let terminal_has_tools = response.output.as_ref().is_some_and(|items| {
                    items.iter().any(|raw| {
                        serde_json::from_str::<WireOutputItem>(raw.get())
                            .is_ok_and(|item| item.kind == "function_call")
                    })
                });
                if self.opened_tool_calls
                    && !terminal_has_tools
                    && response.status.as_deref() == Some("completed")
                {
                    return self.violation("completed response omits an announced function call");
                }
                self.opened_tool_calls |= terminal_has_tools;
                if response.status.as_deref() != event.kind.strip_prefix("response.") {
                    return self.violation("terminal event and response status disagree");
                }
                if let Err(detail) = self.observe_response(&response, correlation, sink) {
                    return self.violation(detail);
                }
                if response.usage.is_none()
                    && self.usage == TokenUsage::unreported()
                    && response.status.as_deref() != Some("failed")
                {
                    return self.violation("terminal response lacks usage");
                }
                let evidence = decode_response(
                    response,
                    self.usage,
                    self.exchange.clone(),
                    correlation,
                    sink,
                );
                match evidence {
                    TerminalEvidence::BoundaryLoss(mut loss) => {
                        loss.cause = LossCause::StreamProtocolViolation {
                            detail: format!("invalid terminal response: {:?}", loss.cause),
                        };
                        if self.tool_calls_at_loss() != ToolCallsAtLoss::NoneOpened {
                            loss.tool_calls = self.tool_calls_at_loss();
                        }
                        StreamStep::Terminal(Box::new(TerminalEvidence::BoundaryLoss(loss)))
                    }
                    evidence => StreamStep::Terminal(Box::new(evidence)),
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                let (Some(index), Some(raw)) = (event.output_index, event.item) else {
                    return self.violation("output item event lacks index or item");
                };
                let item: WireOutputItem = match serde_json::from_str(raw.get()) {
                    Ok(item) => item,
                    Err(e) => {
                        return StreamStep::Terminal(Box::new(
                            self.undecoded_violation_evidence(e.to_string()),
                        ));
                    }
                };
                if item.kind == "function_call" {
                    self.opened_tool_calls = true;
                }
                if !matches!(
                    item.kind.as_str(),
                    "message" | "function_call" | "reasoning"
                ) {
                    return self.violation("unrecognized output item type");
                }
                let Some(id) = item.id else {
                    return self.violation("output item lacks id");
                };
                if let Err(detail) = self.observe_item(index, &id) {
                    return self.violation(detail);
                }
                StreamStep::Continue
            }
            "response.output_text.delta"
            | "response.refusal.delta"
            | "response.function_call_arguments.delta" => {
                if event.kind == "response.function_call_arguments.delta" {
                    self.opened_tool_calls = true;
                }
                let (Some(index), Some(id), Some(delta)) =
                    (event.output_index, event.item_id, event.delta)
                else {
                    return self.violation("delta lacks index, item id, or fragment");
                };
                if let Err(detail) = self.observe_item(index, &id) {
                    return self.violation(detail);
                }
                let fact = if event.kind == "response.function_call_arguments.delta" {
                    if let Err(e) = self
                        .argument_nesting
                        .entry(index)
                        .or_default()
                        .validate_fragment(delta.as_bytes())
                    {
                        return self.violation(e.to_string());
                    }
                    ObservationFact::ToolArgumentsDelta {
                        index,
                        fragment: delta,
                    }
                } else {
                    ObservationFact::TextDelta { index, text: delta }
                };
                emit(correlation, sink, fact);
                StreamStep::Continue
            }
            "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.output_text.annotation.added"
            | "response.refusal.done"
            | "response.function_call_arguments.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_text.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done" => StreamStep::Continue,
            other => self.violation(format!("unrecognized Responses event type {other:?}")),
        }
    }

    fn observe_response<C: Clone>(
        &mut self,
        response: &Response,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) -> Result<(), String> {
        if response.id.as_deref().is_none_or(str::is_empty) {
            return Err("response event lacks its response id".to_string());
        }
        if let Some(id) = &response.id {
            if self
                .response_id
                .as_ref()
                .is_some_and(|previous| previous != id)
            {
                return Err("response id changed during stream".to_string());
            }
            self.response_id = Some(id.clone());
        }
        if let Some(model) = &response.model {
            let model = ProviderReportedModel::new(model.clone());
            if self
                .reported_model
                .as_ref()
                .is_some_and(|previous| previous != &model)
            {
                return Err("model identity changed during stream".to_string());
            }
            if self.reported_model.is_none() {
                emit(
                    correlation,
                    sink,
                    ObservationFact::ProviderModelReported(model.clone()),
                );
            }
            self.reported_model = Some(model);
        }
        if let Some(usage) = &response.usage {
            self.usage.absorb(convert_usage(usage));
        }
        Ok(())
    }

    fn observe_item(&mut self, index: u32, id: &str) -> Result<(), String> {
        if id.is_empty()
            || self
                .item_ids
                .get(&index)
                .is_some_and(|previous| previous != id)
        {
            return Err("output item id is empty or changed at its index".to_string());
        }
        self.item_ids.insert(index, id.to_string());
        Ok(())
    }

    fn tool_calls_at_loss(&self) -> ToolCallsAtLoss {
        if self.opened_tool_calls {
            ToolCallsAtLoss::Opened
        } else if self.discarded_unexamined_bytes || self.later_records == LaterRecords::Unapplied {
            ToolCallsAtLoss::Unobserved
        } else {
            ToolCallsAtLoss::NoneOpened
        }
    }

    pub(crate) fn note_discarded_unexamined_bytes(&mut self) {
        self.discarded_unexamined_bytes = true;
    }
    pub(crate) fn note_later_records(&mut self, later_records: LaterRecords) {
        self.later_records = later_records;
    }

    fn loss(&self, cause: LossCause, tool_calls: ToolCallsAtLoss) -> TerminalEvidence {
        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause,
            exchange: self.exchange.clone(),
            reported_model: self.reported_model.clone(),
            finish_reported: None,
            tool_calls,
            usage: self.usage,
        })
    }

    pub(crate) fn undecoded_violation_evidence(
        &self,
        detail: impl Into<String>,
    ) -> TerminalEvidence {
        self.loss(
            LossCause::StreamProtocolViolation {
                detail: detail.into(),
            },
            if self.opened_tool_calls {
                ToolCallsAtLoss::Opened
            } else {
                ToolCallsAtLoss::Unobserved
            },
        )
    }
    pub(crate) fn lost(self, interruption: StreamInterruption) -> TerminalEvidence {
        self.loss(
            LossCause::StreamEndedWithoutTerminalMarker { interruption },
            self.tool_calls_at_loss(),
        )
    }
    pub(crate) fn cancelled(&self) -> TerminalEvidence {
        self.loss(LossCause::CancellationRequested, self.tool_calls_at_loss())
    }
    fn violation(&self, detail: impl Into<String>) -> StreamStep {
        StreamStep::Terminal(Box::new(self.loss(
            LossCause::StreamProtocolViolation {
                detail: detail.into(),
            },
            self.tool_calls_at_loss(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use signalbox_model_runtime::{CompletionFinish, Observation, ProviderErrorKind};

    fn terminal() -> Value {
        json!({"type":"response.completed", "response":{"id":"resp_fixture","object":"response","model":"model-fixture","status":"completed",
            "output":[{"type":"message","id":"msg_fixture","role":"assistant","content":[{"type":"output_text","text":"ready"}]}],
            "usage":{"input_tokens":5,"output_tokens":2}}})
    }
    fn apply(
        decoder: &mut StreamDecoder,
        event: Value,
        sink: &mut Vec<Observation<String>>,
    ) -> StreamStep {
        decoder.apply(
            &SseRecord {
                data: event.to_string(),
                event: Some("deliberately-unrelated-event-name".to_string()),
            },
            &"fixture".to_string(),
            sink,
        )
    }
    fn decode(event: Value) -> TerminalEvidence {
        let mut decoder = StreamDecoder::new(ExchangeFacts::default());
        let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut Vec::new()) else {
            panic!("fixture must terminate");
        };
        *evidence
    }
    #[test]
    fn terminal_payload_type_completes_despite_an_unrelated_sse_event_name() {
        assert!(matches!(decode(terminal()), TerminalEvidence::Completed(_)));
    }
    #[test]
    fn text_delta_does_not_complete_a_stream() {
        let mut decoder = StreamDecoder::new(ExchangeFacts::default());
        let mut sink = Vec::new();
        let event = json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg_fixture","delta":"ready"});
        assert!(matches!(
            apply(&mut decoder, event, &mut sink),
            StreamStep::Continue
        ));
        assert_eq!(
            sink[0].fact,
            ObservationFact::TextDelta {
                index: 0,
                text: "ready".to_string()
            }
        );
        assert!(matches!(
            decoder.lost(StreamInterruption::EndOfStream),
            TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                cause: LossCause::StreamEndedWithoutTerminalMarker { .. },
                ..
            })
        ));
    }
    #[test]
    fn unknown_payload_type_is_a_protocol_violation() {
        assert!(matches!(
            decode(json!({"type":"response.future"})),
            TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                cause: LossCause::StreamProtocolViolation { .. },
                ..
            })
        ));
    }
    #[test]
    fn terminal_response_requires_usage() {
        let mut event = terminal();
        event["response"].as_object_mut().unwrap().remove("usage");
        assert!(matches!(decode(event), TerminalEvidence::BoundaryLoss(_)));
    }
    #[test]
    fn terminal_usage_preserves_earlier_fields_and_supersedes_reported_fields() {
        // Distinct counts expose replacement and accidental summation on all axes.
        for status in ["completed", "incomplete", "failed"] {
            for terminal_usage in [
                json!({"output_tokens":11,"input_tokens_details":{"cached_tokens":3}}),
                json!({"output_tokens":11}),
                Value::Null,
            ] {
                let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                let mut sink = Vec::new();
                apply(
                    &mut decoder,
                    json!({"type":"response.in_progress","response":{
                        "id":"resp_fixture","model":"model-fixture","usage":{
                            "input_tokens":17,"output_tokens":7,
                            "input_tokens_details":{"cached_tokens":5,"cache_write_tokens":2}
                        }
                    }}),
                    &mut sink,
                );
                let expected = TokenUsage {
                    input_tokens: Some(17),
                    output_tokens: Some(if terminal_usage.is_null() { 7 } else { 11 }),
                    cache_read_input_tokens: Some(
                        if terminal_usage.get("input_tokens_details").is_some() {
                            3
                        } else {
                            5
                        },
                    ),
                    cache_creation_input_tokens: Some(2),
                };
                let mut event = terminal();
                event["type"] = json!(format!("response.{status}"));
                event["response"]["status"] = json!(status);
                event["response"]["usage"] = terminal_usage;
                event["response"]["incomplete_details"] = json!({"reason":"max_output_tokens"});
                event["response"]["error"] = json!({"code":"server_error"});
                let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink) else {
                    panic!("terminal event must terminate");
                };
                let usage = match *evidence {
                    TerminalEvidence::Completed(result) => result.usage,
                    TerminalEvidence::ProviderError(error) => error.usage,
                    other => panic!("unexpected terminal evidence: {other:?}"),
                };
                assert_eq!(usage, expected);
                assert_eq!(
                    sink.iter()
                        .rev()
                        .find_map(|observation| match observation.fact {
                            ObservationFact::UsageReported(usage) => Some(usage),
                            _ => None,
                        }),
                    Some(expected)
                );
            }
        }
    }

    #[test]
    fn terminal_event_must_agree_with_response_status() {
        let mut event = terminal();
        event["response"]["status"] = json!("in_progress");
        assert!(matches!(decode(event), TerminalEvidence::BoundaryLoss(_)));
    }
    #[test]
    fn incomplete_output_ceiling_is_typed_completion() {
        let mut event = terminal();
        event["type"] = json!("response.incomplete");
        event["response"]["status"] = json!("incomplete");
        event["response"]["incomplete_details"] = json!({"reason":"max_output_tokens"});
        assert!(
            matches!(decode(event),TerminalEvidence::Completed(result) if result.finish==CompletionFinish::MaxOutputTokens)
        );
    }
    #[test]
    fn bare_error_needs_no_response_id_and_never_proves_non_acceptance() {
        let evidence =
            decode(json!({"type":"error","code":"rate_limit_exceeded","message":"provider error"}));
        let TerminalEvidence::ProviderError(error) = evidence else {
            panic!("bare error is definitive");
        };
        assert_eq!(error.kind, ProviderErrorKind::RateLimited);
        assert!(!error.non_acceptance_proven);
        assert_eq!(error.native.error_token, None);
    }
    #[test]
    fn failed_response_event_is_definitive_without_non_acceptance_proof() {
        let mut event = terminal();
        event["type"] = json!("response.failed");
        event["response"]["status"] = json!("failed");
        event["response"]["error"] =
            json!({"code":"server_error","message":"failed after acceptance"});
        assert!(
            matches!(decode(event),TerminalEvidence::ProviderError(error) if !error.non_acceptance_proven && error.kind==ProviderErrorKind::ProviderInternal)
        );
    }
    #[test]
    fn response_identity_cannot_change_during_the_stream() {
        let mut decoder = StreamDecoder::new(ExchangeFacts::default());
        let mut sink = Vec::new();
        apply(
            &mut decoder,
            json!({"type":"response.created","response":{"id":"resp_first","model":"model-fixture"}}),
            &mut sink,
        );
        assert!(
            matches!(apply(&mut decoder,terminal(),&mut sink),StreamStep::Terminal(evidence) if matches!(*evidence,TerminalEvidence::BoundaryLoss(_)))
        );
    }
    #[test]
    fn model_identity_is_observed_before_any_terminal_event() {
        let mut decoder = StreamDecoder::new(ExchangeFacts::default());
        let mut sink = Vec::new();
        apply(
            &mut decoder,
            json!({"type":"response.created","response":{"id":"resp_fixture","model":"model-fixture"}}),
            &mut sink,
        );
        assert_eq!(
            sink[0].fact,
            ObservationFact::ProviderModelReported(ProviderReportedModel::new("model-fixture"))
        );
    }
    #[test]
    fn incomplete_added_reasoning_never_becomes_visible_content() {
        let mut decoder = StreamDecoder::new(ExchangeFacts::default());
        let mut sink = Vec::new();
        for (kind, ciphertext) in [
            ("response.output_item.added", "partial"),
            ("response.output_item.done", "complete"),
        ] {
            apply(
                &mut decoder,
                json!({"type":kind,"output_index":0,"item":{"type":"reasoning","id":"rs_fixture","summary":[],"encrypted_content":ciphertext}}),
                &mut sink,
            );
        }
        assert!(sink.is_empty());
        assert!(
            matches!(apply(&mut decoder,terminal(),&mut sink),StreamStep::Terminal(evidence) if matches!(*evidence,TerminalEvidence::Completed(_)))
        );
    }
    #[test]
    fn tool_argument_depth_is_bounded_across_fragments() {
        let mut decoder = StreamDecoder::new(ExchangeFacts::default());
        let mut sink = Vec::new();
        let first = json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_fixture","delta":"[".repeat(signalbox_model_runtime::PROVIDER_JSON_NESTING_LIMIT)});
        assert!(matches!(
            apply(&mut decoder, first, &mut sink),
            StreamStep::Continue
        ));
        let next = json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_fixture","delta":"["});
        assert!(
            matches!(apply(&mut decoder,next,&mut sink),StreamStep::Terminal(evidence) if matches!(*evidence,TerminalEvidence::BoundaryLoss(BoundaryLossEvidence{tool_calls:ToolCallsAtLoss::Opened,..})))
        );
    }
    #[test]
    fn unparsed_bytes_do_not_claim_that_no_tool_opened() {
        let decoder = StreamDecoder::new(ExchangeFacts::default());
        assert!(matches!(
            decoder.undecoded_violation_evidence("invalid JSON"),
            TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                tool_calls: ToolCallsAtLoss::Unobserved,
                ..
            })
        ));
    }
    #[test]
    fn completed_response_cannot_discard_an_announced_tool_call() {
        let mut decoder = StreamDecoder::new(ExchangeFacts::default());
        let mut sink = Vec::new();
        apply(
            &mut decoder,
            json!({"type":"response.output_item.added","output_index":0,
            "item":{"type":"function_call","id":"fc_fixture","call_id":"call_fixture","name":"lookup","arguments":""}}),
            &mut sink,
        );
        assert!(
            matches!(apply(&mut decoder, terminal(), &mut sink), StreamStep::Terminal(evidence)
            if matches!(*evidence, TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {tool_calls: ToolCallsAtLoss::Opened, ..})))
        );
    }
}
