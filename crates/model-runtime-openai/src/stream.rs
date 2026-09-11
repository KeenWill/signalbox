//! Responses SSE payload dispatch and terminal integrity.
//! A terminal response event, rather than EOF, completes the exchange.

use std::collections::BTreeMap;

use signalbox_model_runtime::{
    BoundaryLossEvidence, ExchangeFacts, FinishReason, LossCause, ObservationFact, ObservationSink,
    ProviderJsonNestingValidator, ProviderReportedModel, SseRecord, StreamInterruption,
    TerminalEvidence, TokenUsage, ToolCallsAtLoss, validate_provider_json_nesting,
};

use crate::response::{
    convert_usage, decode_response, emit, map_terminal, output_tool_calls, provider_error,
};
use crate::wire::{Response, ResponseError, ResponseEvent, WireContent, WireOutputItem};

pub(crate) enum StreamStep {
    Continue,
    Terminal(Box<TerminalEvidence>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaterRecords {
    Unapplied,
    AllApplied,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SnapshotEnd<'a> {
    Added,
    Done,
    Terminal(&'a str),
}

enum ContentState {
    Open {
        deltas: Option<String>,
        snapshot: String,
    },
    Done(String),
}

struct ContentPart {
    kind: &'static str,
    state: ContentState,
}

impl ContentPart {
    fn occupancy(&self) -> Option<bool> {
        match &self.state {
            ContentState::Done(value) => Some(!value.is_empty()),
            ContentState::Open { deltas, snapshot } => (!snapshot.is_empty()
                || deltas.as_ref().is_some_and(|s| !s.is_empty()))
            .then_some(true),
        }
    }
}

enum ItemPhase {
    Open,
    MessageDone,
    FunctionDone {
        call_id: Option<String>,
        name: Option<String>,
        status: Option<String>,
    },
    ReasoningDone {
        width: u32,
        raw: Box<serde_json::value::RawValue>,
    },
}

struct ItemState {
    id: String,
    kind: String,
    phase: ItemPhase,
    content: BTreeMap<u32, ContentPart>,
    argument_nesting: ProviderJsonNestingValidator,
}

enum ItemUpdate<'a> {
    Delta {
        index: u32,
        kind: &'static str,
        fragment: &'a str,
    },
    Content {
        index: u32,
        kind: &'static str,
        value: &'a str,
        end: SnapshotEnd<'a>,
    },
    Snapshot {
        item: WireOutputItem,
        raw: &'a mut Box<serde_json::value::RawValue>,
        end: SnapshotEnd<'a>,
    },
    Indexed,
    ReasoningDelta,
}

impl ItemState {
    fn transition(&mut self, update: ItemUpdate<'_>) -> Result<(), String> {
        match (&self.phase, &update) {
            (ItemPhase::Open, _) => {}
            (_, ItemUpdate::Delta { .. } | ItemUpdate::ReasoningDelta) => {
                return Err("delta follows output item completion".into());
            }
            _ => {}
        }
        match update {
            ItemUpdate::Delta {
                index,
                kind,
                fragment,
            } => {
                if self.kind == "function_call" {
                    self.argument_nesting
                        .validate_fragment(fragment.as_bytes())
                        .map_err(|e| e.to_string())?;
                }
                let ContentState::Open { deltas, .. } = &mut self.part(index, kind)?.state else {
                    return Err("delta follows content completion".into());
                };
                if !fragment.is_empty() {
                    deltas.get_or_insert_with(String::new).push_str(fragment);
                }
            }
            ItemUpdate::Content {
                index,
                kind,
                value,
                end,
            } => {
                let part = self.part(index, kind)?;
                match &mut part.state {
                    ContentState::Open { deltas, snapshot }
                        if value.starts_with(snapshot.as_str())
                            && (end == SnapshotEnd::Added
                                || deltas.as_deref().is_none_or(|s| s == value)) =>
                    {
                        if end == SnapshotEnd::Added {
                            *snapshot = value.to_string();
                        } else {
                            part.state = ContentState::Done(value.to_string());
                        }
                    }
                    ContentState::Done(previous) if previous == value => {}
                    _ => return Err("content snapshot does not fit accumulated bytes".into()),
                }
            }
            ItemUpdate::Snapshot { item, raw, end } => {
                if let SnapshotEnd::Terminal(status) = end {
                    match (item.kind.as_str(), item.status.as_deref(), status) {
                        ("reasoning", None, _)
                        | (_, Some("completed"), _)
                        | (_, Some("incomplete"), "incomplete") => {}
                        _ => return Err("item status does not fit terminal response".into()),
                    }
                }
                match &self.phase {
                    ItemPhase::FunctionDone {
                        call_id,
                        name,
                        status,
                    } if (&item.call_id, &item.name, &item.status) != (call_id, name, status) => {
                        return Err("function snapshot does not fit completed item".into());
                    }
                    ItemPhase::ReasoningDone { raw: retained, .. } => {
                        if !matches!(end, SnapshotEnd::Terminal(_)) && raw.get() != retained.get() {
                            return Err("reasoning snapshot does not fit completed item".into());
                        }
                        *raw = retained.clone();
                        return Ok(());
                    }
                    ItemPhase::MessageDone
                        if item.content.as_ref().map_or(0, Vec::len) != self.content.len() =>
                    {
                        return Err("snapshot changes completed content layout".into());
                    }
                    _ => {}
                }
                match self.kind.as_str() {
                    "message" => {
                        let parts = item.content.as_deref().unwrap_or_default();
                        let length = u32::try_from(parts.len()).map_err(|e| e.to_string())?;
                        if end != SnapshotEnd::Added
                            && self.content.range(length..).next().is_some()
                        {
                            return Err("snapshot omits observed content".into());
                        }
                        for (index, part) in parts.iter().enumerate() {
                            let index = u32::try_from(index).map_err(|e| e.to_string())?;
                            let (kind, value) = content_value(part)?;
                            self.transition(ItemUpdate::Content {
                                index,
                                kind,
                                value,
                                end,
                            })?;
                        }
                    }
                    "function_call" => {
                        if let Some(value) = item.arguments.as_deref() {
                            self.transition(ItemUpdate::Content {
                                index: 0,
                                kind: "function_call_arguments",
                                value,
                                end,
                            })?;
                        }
                    }
                    _ => {}
                }
                if end != SnapshotEnd::Added && matches!(self.phase, ItemPhase::Open) {
                    self.phase = match self.kind.as_str() {
                        "function_call" => ItemPhase::FunctionDone {
                            call_id: item.call_id,
                            name: item.name,
                            status: item.status,
                        },
                        "reasoning" => ItemPhase::ReasoningDone {
                            width: u32::from(item.encrypted_content.is_some()),
                            raw: raw.clone(),
                        },
                        _ => ItemPhase::MessageDone,
                    };
                }
            }
            ItemUpdate::Indexed | ItemUpdate::ReasoningDelta => {}
        }
        Ok(())
    }

    fn part(&mut self, index: u32, kind: &'static str) -> Result<&mut ContentPart, String> {
        if !matches!(self.phase, ItemPhase::Open) && !self.content.contains_key(&index) {
            return Err("completed item gained content".into());
        }
        let part = self.content.entry(index).or_insert_with(|| ContentPart {
            kind,
            state: ContentState::Open {
                deltas: None,
                snapshot: String::new(),
            },
        });
        if part.kind != kind {
            return Err("content type does not fit accumulated item".into());
        }
        Ok(part)
    }

    fn width(&self) -> Option<u32> {
        match (&*self.kind, &self.phase) {
            ("function_call", _) => Some(1),
            ("reasoning", ItemPhase::ReasoningDone { width, .. }) => Some(*width),
            ("message", ItemPhase::MessageDone) => u32::try_from(
                self.content
                    .values()
                    .filter(|part| part.occupancy() == Some(true))
                    .count(),
            )
            .ok(),
            _ => None,
        }
    }
}

fn content_value(part: &WireContent) -> Result<(&'static str, &str), String> {
    match part {
        WireContent::OutputText { text } => Ok(("output_text", text)),
        WireContent::Refusal { refusal } => Ok(("refusal", refusal)),
        WireContent::Unknown => Err("unrecognized output content type".into()),
    }
}

struct PendingDelta {
    output_index: u32,
    content_index: u32,
    fragment: String,
    tool_arguments: bool,
}

pub(crate) struct StreamDecoder {
    exchange: ExchangeFacts,
    response_id: Option<String>,
    reported_model: Option<ProviderReportedModel>,
    finish_reported: Option<FinishReason>,
    usage: TokenUsage,
    opened_tool_calls: bool,
    discarded_unexamined_bytes: bool,
    later_records: LaterRecords,
    items: BTreeMap<u32, ItemState>,
    pending_deltas: Vec<PendingDelta>,
}

impl StreamDecoder {
    pub(crate) fn new(exchange: ExchangeFacts) -> Self {
        Self {
            exchange,
            response_id: None,
            reported_model: None,
            finish_reported: None,
            usage: TokenUsage::unreported(),
            opened_tool_calls: false,
            discarded_unexamined_bytes: false,
            later_records: LaterRecords::AllApplied,
            items: BTreeMap::new(),
            pending_deltas: Vec::new(),
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
        let step = match event.kind.as_str() {
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
                let Some(mut response) = event.response else {
                    return self.violation("lifecycle event lacks response");
                };
                if let Err(detail) = self.observe_response(&mut response, correlation, sink) {
                    return self.violation(detail);
                }
                StreamStep::Continue
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                let Some(mut response) = event.response else {
                    return self.violation("terminal event lacks response");
                };
                let failed = event.kind == "response.failed";
                if response.status.as_deref() != event.kind.strip_prefix("response.") {
                    self.observe_response_facts(&response, correlation, sink);
                    self.discarded_unexamined_bytes = true;
                    return self.violation("terminal event and response status disagree");
                }
                if failed && response.error.is_some() {
                    if response.model.as_deref().is_none_or(str::is_empty) {
                        response.model = self
                            .reported_model
                            .as_ref()
                            .map(|model| model.as_str().to_string());
                    }
                    return StreamStep::Terminal(Box::new(decode_response(
                        response,
                        self.usage,
                        self.exchange.clone(),
                        correlation,
                        sink,
                    )));
                }
                if response.status.as_deref() == Some("incomplete") && response.error.is_none() {
                    let finish = map_terminal(
                        "incomplete",
                        response
                            .incomplete_details
                            .as_ref()
                            .map(|details| details.reason.as_str()),
                        self.tool_calls_at_loss(),
                    );
                    if !matches!(finish, FinishReason::Unrecognized { .. }) {
                        self.finish_reported = Some(finish);
                    }
                }
                if let Err(detail) = self.observe_response(&mut response, correlation, sink) {
                    return self.violation(detail);
                }
                if response.usage.is_none()
                    && self.usage == TokenUsage::unreported()
                    && response.status.as_deref() != Some("failed")
                {
                    return self.violation("terminal response lacks usage");
                }
                self.flush_deltas(correlation, sink);
                let evidence = decode_response(
                    response,
                    self.usage,
                    self.exchange.clone(),
                    correlation,
                    sink,
                );
                match evidence {
                    TerminalEvidence::BoundaryLoss(mut loss) => {
                        loss.finish_reported = loss
                            .finish_reported
                            .or_else(|| self.finish_reported.clone());
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
            _ => match self.observe_item_event(event) {
                Ok(()) => StreamStep::Continue,
                Err(detail) => self.violation(detail),
            },
        };
        if matches!(step, StreamStep::Continue) {
            self.flush_deltas(correlation, sink);
        }
        step
    }

    fn observe_response<C: Clone>(
        &mut self,
        response: &mut Response,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) -> Result<(), String> {
        let metadata = self.observe_response_metadata(response, correlation, sink);
        let mut output = response.output_items().map_err(|error| {
            self.discarded_unexamined_bytes = true;
            error.to_string()
        })?;
        if output.is_some() {
            match output_tool_calls(output.as_deref()) {
                ToolCallsAtLoss::Opened => self.opened_tool_calls = true,
                ToolCallsAtLoss::Unobserved => self.discarded_unexamined_bytes = true,
                ToolCallsAtLoss::NoneOpened => {}
            }
        }
        metadata?;
        if response.status.as_deref() == Some("completed")
            && response.error.is_none()
            && response.incomplete_details.is_none()
        {
            self.finish_reported = Some(map_terminal("completed", None, self.tool_calls_at_loss()));
        }
        response
            .reported_usage()
            .map_err(|error| error.to_string())?;
        if matches!(response.status.as_deref(), Some("completed" | "incomplete")) {
            let length = u32::try_from(output.as_ref().map_or(0, Vec::len))
                .map_err(|error| error.to_string())?;
            if self.items.range(length..).next().is_some() {
                return Err("terminal response omits an observed output index".to_string());
            }
        }
        let end = match response.status.as_deref() {
            Some(status @ ("completed" | "incomplete" | "failed")) => SnapshotEnd::Terminal(status),
            _ => SnapshotEnd::Added,
        };
        for (index, raw) in output.iter_mut().flatten().enumerate() {
            let index = u32::try_from(index).map_err(|error| error.to_string())?;
            self.observe_snapshot(index, raw, end)?;
        }
        if let Some(output) = output {
            response.output =
                Some(serde_json::value::to_raw_value(&output).map_err(|e| e.to_string())?);
        }
        Ok(())
    }

    fn observe_response_metadata<C: Clone>(
        &mut self,
        response: &Response,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) -> Result<(), String> {
        if response.id.as_deref().is_none_or(str::is_empty) {
            return Err("response event lacks its response id".to_string());
        }
        if response.model.as_deref().is_none_or(str::is_empty) {
            return Err("response event lacks its reported model".to_string());
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
        }
        self.observe_response_facts(response, correlation, sink);
        Ok(())
    }

    fn observe_response_facts<C: Clone>(
        &mut self,
        response: &Response,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) {
        if let Some(model) = response.model.as_ref().filter(|model| !model.is_empty()) {
            let model = ProviderReportedModel::new(model.clone());
            if self.reported_model.as_ref() != Some(&model) {
                emit(
                    correlation,
                    sink,
                    ObservationFact::ProviderModelReported(model.clone()),
                );
            }
            self.reported_model = Some(model);
        }
        if let Ok(Some(usage)) = response.reported_usage() {
            self.usage.absorb(convert_usage(&usage));
        }
    }

    fn item(&mut self, index: u32, id: &str, kind: &str) -> Result<&mut ItemState, String> {
        let item = self.items.entry(index).or_insert_with(|| ItemState {
            id: id.to_string(),
            kind: kind.to_string(),
            phase: ItemPhase::Open,
            content: BTreeMap::new(),
            argument_nesting: ProviderJsonNestingValidator::default(),
        });
        if id.is_empty() || (item.id.as_str(), item.kind.as_str()) != (id, kind) {
            return Err("item identity does not fit its output index".into());
        }
        Ok(item)
    }

    fn observe_snapshot(
        &mut self,
        index: u32,
        raw: &mut Box<serde_json::value::RawValue>,
        end: SnapshotEnd<'_>,
    ) -> Result<(), String> {
        let item: WireOutputItem = serde_json::from_str(raw.get()).map_err(|e| {
            self.discarded_unexamined_bytes = true;
            e.to_string()
        })?;
        match item.kind.as_str() {
            "function_call" => self.opened_tool_calls = true,
            "message" | "reasoning" => {}
            _ => {
                self.discarded_unexamined_bytes = true;
                return Err("unrecognized output item type".into());
            }
        }
        let state = self.item(
            index,
            item.id.as_deref().ok_or("output item lacks id")?,
            &item.kind,
        )?;
        state.transition(ItemUpdate::Snapshot { item, raw, end })
    }

    fn observe_item_event(&mut self, event: ResponseEvent) -> Result<(), String> {
        if matches!(
            event.kind.as_str(),
            "response.output_item.added" | "response.output_item.done"
        ) {
            let index = event.output_index.ok_or("output item lacks index")?;
            let mut raw = event.item.ok_or("output item event lacks item")?;
            let end = if event.kind == "response.output_item.done" {
                SnapshotEnd::Done
            } else {
                SnapshotEnd::Added
            };
            return self.observe_snapshot(index, &mut raw, end);
        }
        let content_index = || {
            event
                .content_index
                .ok_or_else(|| "content event lacks index".to_string())
        };
        let (kind, update) = match event.kind.as_str() {
            "response.output_text.delta"
            | "response.refusal.delta"
            | "response.function_call_arguments.delta" => {
                let (kind, content_kind, index) = match event.kind.as_str() {
                    "response.function_call_arguments.delta" => {
                        ("function_call", "function_call_arguments", 0)
                    }
                    "response.refusal.delta" => ("message", "refusal", content_index()?),
                    _ => ("message", "output_text", content_index()?),
                };
                (
                    kind,
                    ItemUpdate::Delta {
                        index,
                        kind: content_kind,
                        fragment: event.delta.as_deref().ok_or("delta lacks fragment")?,
                    },
                )
            }
            "response.output_text.done"
            | "response.refusal.done"
            | "response.function_call_arguments.done" => {
                let (kind, content_kind, index, value) = match event.kind.as_str() {
                    "response.function_call_arguments.done" => (
                        "function_call",
                        "function_call_arguments",
                        0,
                        &event.arguments,
                    ),
                    "response.refusal.done" => {
                        ("message", "refusal", content_index()?, &event.refusal)
                    }
                    _ => ("message", "output_text", content_index()?, &event.text),
                };
                (
                    kind,
                    ItemUpdate::Content {
                        index,
                        kind: content_kind,
                        value: value.as_deref().ok_or("content completion lacks value")?,
                        end: SnapshotEnd::Done,
                    },
                )
            }
            "response.content_part.added" | "response.content_part.done" => {
                let (kind, value) =
                    content_value(event.part.as_ref().ok_or("content part event lacks part")?)?;
                let end = if event.kind == "response.content_part.done" {
                    SnapshotEnd::Done
                } else {
                    SnapshotEnd::Added
                };
                (
                    "message",
                    ItemUpdate::Content {
                        index: content_index()?,
                        kind,
                        value,
                        end,
                    },
                )
            }
            "response.output_text.annotation.added" => ("message", ItemUpdate::Indexed),
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                ("reasoning", ItemUpdate::ReasoningDelta)
            }
            "response.reasoning_text.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.done" => ("reasoning", ItemUpdate::Indexed),
            other => {
                self.discarded_unexamined_bytes = true;
                return Err(format!("unrecognized Responses event type {other:?}"));
            }
        };
        let index = event
            .output_index
            .ok_or("indexed event lacks output index")?;
        let id = event
            .item_id
            .as_deref()
            .ok_or("indexed event lacks item id")?;
        let pending = match &update {
            ItemUpdate::Delta {
                index: content_index,
                fragment,
                ..
            } if kind == "function_call" || !fragment.is_empty() => Some(PendingDelta {
                output_index: index,
                content_index: *content_index,
                fragment: (*fragment).to_string(),
                tool_arguments: kind == "function_call",
            }),
            _ => None,
        };
        self.item(index, id, kind)?.transition(update)?;
        self.pending_deltas.extend(pending);
        Ok(())
    }

    fn part_index(&self, output_index: u32, content_index: u32) -> Option<u32> {
        let mut offset = 0u32;
        let mut next_item = 0;
        for (&index, layout) in self.items.range(..output_index) {
            if index != next_item {
                return None;
            }
            offset = offset.checked_add(layout.width()?)?;
            next_item += 1;
        }
        if next_item != output_index {
            return None;
        }
        let layout = self.items.get(&output_index)?;
        let mut next_part = 0;
        for (&index, part) in layout.content.range(..content_index) {
            if index != next_part {
                return None;
            }
            offset = offset.checked_add(u32::from(part.occupancy()?))?;
            next_part += 1;
        }
        (next_part == content_index).then_some(offset)
    }

    fn flush_deltas<C: Clone>(
        &mut self,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) {
        // Pending fragments share the transport's total streamed-response byte bound.
        for delta in std::mem::take(&mut self.pending_deltas) {
            if let Some(index) = self.part_index(delta.output_index, delta.content_index) {
                let fact = if delta.tool_arguments {
                    ObservationFact::ToolArgumentsDelta {
                        index,
                        fragment: delta.fragment,
                    }
                } else {
                    ObservationFact::TextDelta {
                        index,
                        text: delta.fragment,
                    }
                };
                emit(correlation, sink, fact);
            } else {
                self.pending_deltas.push(delta);
            }
        }
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
            response_content_observed: self.opened_tool_calls
                || self.items.values().any(|item| {
                    item.kind == "reasoning"
                        || item
                            .content
                            .values()
                            .any(|part| part.occupancy() == Some(true))
                }),
            cause,
            exchange: self.exchange.clone(),
            reported_model: self.reported_model.clone(),
            finish_reported: self.finish_reported.clone(),
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
    use signalbox_model_runtime::{CompletionFinish, FinishReason, Observation, ProviderErrorKind};

    fn terminal() -> Value {
        json!({"type":"response.completed", "response":{"id":"resp_fixture","object":"response","model":"model-fixture","status":"completed",
            "output":[{"type":"message","id":"msg_fixture","status":"completed","role":"assistant","content":[{"type":"output_text","text":"ready"}]}],
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
    fn output_ceiling_retains_arguments_through_open_and_done_item_states() {
        let arguments = r#"{"query":"part"#;
        let item = json!({"type":"function_call","id":"fc_fixture","status":"incomplete",
            "call_id":"call_fixture","name":"lookup","arguments":arguments});
        for closing_events in [
            vec![],
            vec![
                json!({"type":"response.function_call_arguments.done","output_index":0,
                    "item_id":"fc_fixture","arguments":arguments}),
                json!({"type":"response.output_item.done","output_index":0,"item":item}),
            ],
        ] {
            let mut decoder = StreamDecoder::new(ExchangeFacts::default());
            let mut sink = Vec::new();
            assert!(matches!(
                apply(
                    &mut decoder,
                    json!({"type":"response.output_item.added",
                "output_index":0,"item":{"type":"function_call","id":"fc_fixture",
                "status":"in_progress","arguments":""}}),
                    &mut sink
                ),
                StreamStep::Continue
            ));
            assert!(matches!(
                apply(
                    &mut decoder,
                    json!({"type":"response.function_call_arguments.delta",
                "output_index":0,"item_id":"fc_fixture","delta":arguments}),
                    &mut sink
                ),
                StreamStep::Continue
            ));
            for event in closing_events {
                assert!(matches!(
                    apply(&mut decoder, event, &mut sink),
                    StreamStep::Continue
                ));
            }
            assert!(!sink.iter().any(|o| matches!(
                o.fact,
                ObservationFact::ToolCallProposed(_) | ObservationFact::FinishReported(_)
            )));
            let mut event = terminal();
            event["type"] = json!("response.incomplete");
            event["response"]["status"] = json!("incomplete");
            event["response"]["incomplete_details"] = json!({"reason":"max_output_tokens"});
            event["response"]["output"] = json!([item]);
            let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink) else {
                panic!("output ceiling terminates the stream");
            };
            let TerminalEvidence::Completed(result) = *evidence else {
                panic!("open and done incomplete calls retain typed output-ceiling completion");
            };
            assert_eq!(result.finish, CompletionFinish::MaxOutputTokens);
            assert!(
                matches!(result.content.as_slice(), [signalbox_model_runtime::AssistantPart::ToolCall(call)]
                if call.arguments_json == arguments && call.id.as_str() == "call_fixture")
            );
        }
    }

    #[test]
    fn frozen_content_cannot_resume_even_with_an_empty_delta() {
        for done in [
            json!({"type":"response.output_text.done","output_index":0,"content_index":0,
                "item_id":"msg_fixture","text":"ready"}),
            json!({"type":"response.output_item.done","output_index":0,
                "item":{"type":"message","id":"msg_fixture","status":"completed","role":"assistant",
                    "content":[{"type":"output_text","text":"ready"}]}}),
        ] {
            let mut decoder = StreamDecoder::new(ExchangeFacts::default());
            let mut sink = Vec::new();
            assert!(matches!(
                apply(&mut decoder, done, &mut sink),
                StreamStep::Continue
            ));
            let StreamStep::Terminal(evidence) = apply(
                &mut decoder,
                json!({"type":"response.output_text.delta",
                "output_index":0,"content_index":0,"item_id":"msg_fixture","delta":""}),
                &mut sink,
            ) else {
                panic!("a frozen content part cannot resume");
            };
            assert!(matches!(
                *evidence,
                TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                    response_content_observed: true,
                    cause: LossCause::StreamProtocolViolation { .. },
                    ..
                })
            ));
            assert!(
                sink.is_empty(),
                "invalid transitions publish no completion or refusal"
            );
        }
    }

    #[test]
    fn terminal_payload_type_completes_despite_an_unrelated_sse_event_name() {
        assert!(matches!(decode(terminal()), TerminalEvidence::Completed(_)));
    }
    #[test]
    fn unprojected_text_delta_still_marks_content_before_transport_loss() {
        let mut decoder = StreamDecoder::new(ExchangeFacts::default());
        let mut sink = Vec::new();
        let event = json!({"type":"response.output_text.delta","output_index":1,"content_index":0,"item_id":"later_item","delta":"partial"});
        assert!(matches!(
            apply(&mut decoder, event, &mut sink),
            StreamStep::Continue
        ));
        assert!(
            sink.is_empty(),
            "the preceding output item is not yet available"
        );
        let TerminalEvidence::BoundaryLoss(loss) = decoder.lost(StreamInterruption::EndOfStream)
        else {
            panic!("incomplete stream reports loss");
        };
        assert!(loss.response_content_observed);
    }

    #[test]
    fn text_delta_does_not_complete_a_stream() {
        let mut decoder = StreamDecoder::new(ExchangeFacts::default());
        let mut sink = Vec::new();
        let event = json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg_fixture","delta":"ready"});
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
                response_content_observed: true,
                cause: LossCause::StreamEndedWithoutTerminalMarker { .. },
                ..
            })
        ));
    }
    #[test]
    fn completed_function_call_identity_must_survive_repeated_snapshots() {
        for terminal_snapshot in [false, true] {
            for replacement in [
                None,
                Some(("call_id", json!("call_changed"))),
                Some(("name", json!("changed_tool"))),
                Some(("status", json!("incomplete"))),
                Some(("call_id", Value::Null)),
                Some(("name", Value::Null)),
                Some(("status", Value::Null)),
            ] {
                let mut item = json!({"type":"function_call","id":"fc_fixture",
                    "status":"completed","call_id":"call_fixture","name":"lookup","arguments":"{}"});
                let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                let mut sink = Vec::new();
                assert!(matches!(
                    apply(
                        &mut decoder,
                        json!({
                            "type":"response.output_item.done","output_index":0,"item":item
                        }),
                        &mut sink
                    ),
                    StreamStep::Continue
                ));
                if let Some((field, value)) = &replacement {
                    item[*field] = value.clone();
                }
                let event = if terminal_snapshot {
                    let mut event = terminal();
                    event["response"]["output"] = json!([item]);
                    event
                } else {
                    json!({"type":"response.output_item.done","output_index":0,"item":item})
                };
                let step = apply(&mut decoder, event, &mut sink);
                if replacement.is_some() {
                    assert!(matches!(step, StreamStep::Terminal(evidence) if matches!(
                        *evidence, TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                            cause: LossCause::StreamProtocolViolation { .. },
                            tool_calls: ToolCallsAtLoss::Opened, ..
                        })
                    )));
                    assert!(!sink.iter().any(|observation| matches!(
                        observation.fact,
                        ObservationFact::ToolCallProposed(_) | ObservationFact::FinishReported(_)
                    )));
                } else if terminal_snapshot {
                    assert!(matches!(step, StreamStep::Terminal(evidence)
                        if matches!(*evidence, TerminalEvidence::Completed(_))));
                } else {
                    assert!(matches!(step, StreamStep::Continue));
                }
            }
        }
    }

    #[test]
    fn content_part_events_require_the_part_payload() {
        for kind in ["response.content_part.added", "response.content_part.done"] {
            for explicit_null in [false, true] {
                let mut event = json!({"type":kind,"output_index":0,
                    "content_index":0,"item_id":"msg_fixture"});
                if explicit_null {
                    event["part"] = Value::Null;
                }
                let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                let mut sink = Vec::new();
                assert!(matches!(apply(&mut decoder, event, &mut sink),
                    StreamStep::Terminal(evidence) if matches!(*evidence,
                        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                            cause: LossCause::StreamProtocolViolation { .. }, ..
                        })
                    )
                ));
                assert!(sink.is_empty());
            }
        }
    }

    #[test]
    fn malformed_terminal_usage_retains_the_model_and_recognized_finish() {
        for status in ["completed", "incomplete"] {
            for usage in [json!([]), json!("invalid"), json!({"output_tokens":-1})] {
                let mut event = terminal();
                event["type"] = json!(format!("response.{status}"));
                event["response"]["status"] = json!(status);
                event["response"]["usage"] = usage;
                let expected_finish = if status == "incomplete" {
                    event["response"]["incomplete_details"] = json!({"reason":"max_output_tokens"});
                    FinishReason::MaxOutputTokens
                } else {
                    FinishReason::EndTurn
                };
                let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                let mut sink = Vec::new();
                let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink) else {
                    panic!("malformed usage must terminate");
                };
                let TerminalEvidence::BoundaryLoss(loss) = *evidence else {
                    panic!("malformed usage must remain boundary loss");
                };
                assert!(matches!(
                    loss.cause,
                    LossCause::StreamProtocolViolation { .. }
                ));
                assert_eq!(
                    loss.reported_model,
                    Some(ProviderReportedModel::new("model-fixture"))
                );
                assert_eq!(loss.finish_reported, Some(expected_finish));
                assert_eq!(loss.usage, TokenUsage::unreported());
                assert_eq!(sink.len(), 1);
                assert_eq!(
                    sink[0].fact,
                    ObservationFact::ProviderModelReported(ProviderReportedModel::new(
                        "model-fixture"
                    ))
                );
            }
        }
    }

    #[test]
    fn content_done_values_reconcile_with_deltas_and_terminal_snapshots() {
        for (family, field) in [
            ("output_text", "text"),
            ("refusal", "refusal"),
            ("function_call_arguments", "arguments"),
        ] {
            // Both spellings are valid JSON so argument failures test byte integrity.
            for (with_delta, done_value, terminal_value) in [
                (true, "{ }", "{}"),
                (false, "{}", "{ }"),
                (true, "{}", "{}"),
                (false, "{}", "{}"),
            ] {
                let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                let mut sink = Vec::new();
                if with_delta {
                    assert!(matches!(
                        apply(
                            &mut decoder,
                            json!({
                                "type":format!("response.{family}.delta"),"output_index":0,
                                "content_index":0,"item_id":"msg_fixture","delta":"{}"
                            }),
                            &mut sink
                        ),
                        StreamStep::Continue
                    ));
                    assert_eq!(sink.len(), 1, "the delta reached the sink");
                }
                let mut done = json!({
                    "type":format!("response.{family}.done"),"output_index":0,
                    "content_index":0,"item_id":"msg_fixture"
                });
                done[field] = json!(done_value);
                let step = apply(&mut decoder, done, &mut sink);
                let evidence = if with_delta && done_value != "{}" {
                    let StreamStep::Terminal(evidence) = step else {
                        panic!("{family} done must reject rewritten delta bytes");
                    };
                    evidence
                } else {
                    assert!(matches!(step, StreamStep::Continue));
                    let mut event = terminal();
                    if family == "function_call_arguments" {
                        event["response"]["output"][0] = json!({
                            "type":"function_call","id":"msg_fixture","status":"completed",
                            "call_id":"call_fixture","name":"lookup","arguments":terminal_value
                        });
                    } else {
                        let mut part = json!({"type":family});
                        part[field] = json!(terminal_value);
                        event["response"]["output"][0]["content"] = json!([part]);
                    }
                    let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink)
                    else {
                        panic!("terminal must terminate");
                    };
                    evidence
                };
                if done_value == terminal_value {
                    if family == "refusal" {
                        assert!(matches!(*evidence, TerminalEvidence::Refused(_)));
                    } else {
                        assert!(matches!(*evidence, TerminalEvidence::Completed(_)));
                    }
                } else {
                    assert!(
                        matches!(
                            *evidence,
                            TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                                cause: LossCause::StreamProtocolViolation { .. },
                                ..
                            })
                        ),
                        "{family}"
                    );
                    assert!(!sink.iter().any(|observation| matches!(
                        observation.fact,
                        ObservationFact::FinishReported(_)
                    )));
                }
            }
        }
    }

    #[test]
    fn content_done_requires_its_value_and_message_content_index() {
        for (family, field) in [
            ("output_text", "text"),
            ("refusal", "refusal"),
            ("function_call_arguments", "arguments"),
        ] {
            for missing in [field, "content_index"] {
                if family == "function_call_arguments" && missing == "content_index" {
                    continue;
                }
                let mut event = json!({
                    "type":format!("response.{family}.done"),"output_index":0,
                    "content_index":0,"item_id":"msg_fixture"
                });
                event[field] = json!("{}");
                event.as_object_mut().unwrap().remove(missing);
                let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                let mut sink = Vec::new();
                assert!(
                    matches!(apply(&mut decoder, event, &mut sink),
                        StreamStep::Terminal(evidence) if matches!(*evidence,
                            TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                                cause: LossCause::StreamProtocolViolation { .. }, ..
                            })
                        )
                    ),
                    "{family} missing {missing}"
                );
            }
        }
    }

    #[test]
    fn terminal_content_must_match_published_delta_bytes_and_type() {
        for (delta_type, fragment, item, expected_finish) in [
            (
                "response.output_text.delta",
                "ready",
                json!({"type":"message","id":"msg_fixture","status":"completed","role":"assistant","content":[{"type":"output_text","text":"changed"}]}),
                FinishReason::EndTurn,
            ),
            (
                "response.output_text.delta",
                "ready",
                json!({"type":"message","id":"msg_fixture","status":"completed","role":"assistant","content":[{"type":"refusal","refusal":"ready"}]}),
                FinishReason::EndTurn,
            ),
            (
                "response.refusal.delta",
                "ready",
                json!({"type":"message","id":"msg_fixture","status":"completed","role":"assistant","content":[{"type":"output_text","text":"ready"}]}),
                FinishReason::EndTurn,
            ),
            (
                "response.refusal.delta",
                "ready",
                json!({"type":"message","id":"msg_fixture","status":"completed","role":"assistant","content":[{"type":"refusal","refusal":"changed"}]}),
                FinishReason::EndTurn,
            ),
            (
                "response.function_call_arguments.delta",
                "{}",
                json!({"type":"function_call","id":"msg_fixture","status":"completed","call_id":"call_fixture","name":"lookup","arguments":"{ }"}),
                FinishReason::ToolUse,
            ),
        ] {
            let mut decoder = StreamDecoder::new(ExchangeFacts::default());
            let mut sink = Vec::new();
            assert!(matches!(
                apply(
                    &mut decoder,
                    json!({
                        "type":delta_type,"output_index":0,"content_index":0,"item_id":"msg_fixture","delta":fragment
                    }),
                    &mut sink
                ),
                StreamStep::Continue
            ));
            assert_eq!(sink.len(), 1, "the original fragment reached the sink");
            let mut event = terminal();
            event["response"]["output"] = json!([item]);
            let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink) else {
                panic!("contradictory terminal must terminate");
            };
            let TerminalEvidence::BoundaryLoss(loss) = *evidence else {
                panic!("{delta_type} cannot complete with rewritten content");
            };
            assert!(matches!(
                loss.cause,
                LossCause::StreamProtocolViolation { .. }
            ));
            assert_eq!(loss.finish_reported, Some(expected_finish));
            assert_eq!(
                loss.reported_model,
                Some(ProviderReportedModel::new("model-fixture"))
            );
            assert_eq!(loss.usage.input_tokens, Some(5));
            assert!(
                !sink.iter().any(|observation| matches!(
                    observation.fact,
                    ObservationFact::FinishReported(_)
                ))
            );
        }
    }

    #[test]
    fn item_completion_rejects_rewritten_streamed_content() {
        let mut decoder = StreamDecoder::new(ExchangeFacts::default());
        let mut sink = Vec::new();
        for fragment in ["rea", "dy"] {
            assert!(matches!(
                apply(
                    &mut decoder,
                    json!({"type":"response.output_text.delta",
                "output_index":0,"content_index":0,"item_id":"msg_fixture","delta":fragment}),
                    &mut sink
                ),
                StreamStep::Continue
            ));
        }
        let StreamStep::Terminal(evidence) = apply(
            &mut decoder,
            json!({"type":"response.output_item.done",
            "output_index":0,"item":{"type":"message","id":"msg_fixture","status":"completed","role":"assistant",
            "content":[{"type":"output_text","text":"changed"}]}}),
            &mut sink,
        ) else {
            panic!("rewritten completed item must terminate");
        };
        assert!(matches!(
            *evidence,
            TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                cause: LossCause::StreamProtocolViolation { .. },
                ..
            })
        ));
        assert_eq!(sink.len(), 2);
    }

    #[test]
    fn terminal_cannot_rewrite_an_item_done_snapshot_without_deltas() {
        let mut decoder = StreamDecoder::new(ExchangeFacts::default());
        let mut sink = Vec::new();
        assert!(matches!(
            apply(
                &mut decoder,
                json!({"type":"response.output_item.done",
            "output_index":0,"item":terminal()["response"]["output"][0]}),
                &mut sink
            ),
            StreamStep::Continue
        ));
        let mut event = terminal();
        event["response"]["output"][0]["content"][0]["text"] = json!("changed");
        assert!(
            matches!(apply(&mut decoder, event, &mut sink), StreamStep::Terminal(evidence)
            if matches!(*evidence, TerminalEvidence::BoundaryLoss(_)))
        );
    }

    #[test]
    fn matching_content_snapshots_preserve_accumulated_deltas() {
        let mut decoder = StreamDecoder::new(ExchangeFacts::default());
        let mut sink = Vec::new();
        for fragment in ["rea", "dy"] {
            assert!(matches!(
                apply(
                    &mut decoder,
                    json!({"type":"response.output_text.delta",
                "output_index":0,"content_index":0,"item_id":"msg_fixture","delta":fragment}),
                    &mut sink
                ),
                StreamStep::Continue
            ));
        }
        assert!(matches!(
            apply(
                &mut decoder,
                json!({"type":"response.content_part.done",
            "output_index":0,"content_index":0,"item_id":"msg_fixture","part":{"type":"output_text","text":"ready"}}),
                &mut sink
            ),
            StreamStep::Continue
        ));
        assert!(matches!(
            apply(
                &mut decoder,
                json!({"type":"response.output_item.done",
            "output_index":0,"item":terminal()["response"]["output"][0]}),
                &mut sink
            ),
            StreamStep::Continue
        ));
        let StreamStep::Terminal(evidence) = apply(&mut decoder, terminal(), &mut sink) else {
            panic!("matching terminal must terminate");
        };
        let TerminalEvidence::Completed(completion) = *evidence else {
            panic!("matching content completes");
        };
        assert_eq!(
            completion.content,
            vec![signalbox_model_runtime::AssistantPart::Text(
                "ready".to_string()
            )]
        );
        assert_eq!(
            sink[0].fact,
            ObservationFact::TextDelta {
                index: 0,
                text: "rea".to_string()
            }
        );
        assert_eq!(
            sink[1].fact,
            ObservationFact::TextDelta {
                index: 0,
                text: "dy".to_string()
            }
        );
    }

    #[test]
    fn multiple_message_content_parts_have_distinct_flattened_delta_indices() {
        let mut decoder = StreamDecoder::new(ExchangeFacts::default());
        let mut sink = Vec::new();
        for (content_index, text) in [(0, "first"), (1, "second")] {
            apply(
                &mut decoder,
                json!({"type":"response.output_text.delta","output_index":0,
                "content_index":content_index,"item_id":"msg_fixture","delta":text}),
                &mut sink,
            );
        }
        let mut event = terminal();
        event["response"]["output"][0]["content"] = json!([
            {"type":"output_text","text":"first"},{"type":"output_text","text":"second"}
        ]);
        let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink) else {
            panic!("must terminate");
        };
        let TerminalEvidence::Completed(completion) = *evidence else {
            panic!("must complete");
        };
        assert_eq!(
            completion.content,
            vec![
                signalbox_model_runtime::AssistantPart::Text("first".to_string()),
                signalbox_model_runtime::AssistantPart::Text("second".to_string())
            ]
        );
        assert_eq!(
            sink[0].fact,
            ObservationFact::TextDelta {
                index: 0,
                text: "first".to_string()
            }
        );
        assert_eq!(
            sink[1].fact,
            ObservationFact::TextDelta {
                index: 1,
                text: "second".to_string()
            }
        );
    }

    #[test]
    fn delta_indices_skip_empty_content_and_reasoning_and_count_tool_parts() {
        let mut decoder = StreamDecoder::new(ExchangeFacts::default());
        let mut sink = Vec::new();
        let reasoning = json!({"type":"reasoning","id":"rs_fixture"});
        let message = json!({"type":"message","id":"msg_fixture","status":"completed","role":"assistant","content":[
            {"type":"output_text","text":""},{"type":"output_text","text":"first"},
            {"type":"output_text","text":""},{"type":"output_text","text":"second"}
        ]});
        let call = json!({"type":"function_call","id":"fc_fixture","status":"completed","call_id":"call_fixture","name":"lookup","arguments":"{}"});
        let refusal = json!({"type":"message","id":"msg_refusal","status":"completed","role":"assistant","content":[{"type":"refusal","refusal":"declined"}]});
        apply(
            &mut decoder,
            json!({"type":"response.output_item.added","output_index":0,"item":reasoning}),
            &mut sink,
        );
        // The preceding message's final size is not known when these arrive.
        apply(
            &mut decoder,
            json!({"type":"response.function_call_arguments.delta","output_index":2,"item_id":"fc_fixture","delta":"{}"}),
            &mut sink,
        );
        apply(
            &mut decoder,
            json!({"type":"response.refusal.delta","output_index":3,"content_index":0,"item_id":"msg_refusal","delta":"declined"}),
            &mut sink,
        );
        for (content_index, text) in [(3, "second"), (1, "first")] {
            apply(
                &mut decoder,
                json!({"type":"response.output_text.delta","output_index":1,"content_index":content_index,"item_id":"msg_fixture","delta":text}),
                &mut sink,
            );
        }
        assert!(sink.is_empty());
        apply(
            &mut decoder,
            json!({"type":"response.output_item.done","output_index":1,"item":message}),
            &mut sink,
        );
        let mut event = terminal();
        event["response"]["output"] = json!([reasoning, message, call, refusal]);
        let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink) else {
            panic!("must terminate");
        };
        let TerminalEvidence::Refused(result) = *evidence else {
            panic!("refusal is retained");
        };
        let fragments: BTreeMap<_, _> = sink
            .iter()
            .filter_map(|observation| match &observation.fact {
                ObservationFact::TextDelta { index, text } => Some((*index, text.as_str())),
                ObservationFact::ToolArgumentsDelta { index, fragment } => {
                    Some((*index, fragment.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            fragments,
            BTreeMap::from([(0, "first"), (1, "second"), (2, "{}"), (3, "declined")])
        );
        for (index, part) in result.content.iter().enumerate() {
            let text = match part {
                signalbox_model_runtime::AssistantPart::Text(text) => text.as_str(),
                signalbox_model_runtime::AssistantPart::ToolCall(call) => {
                    call.arguments_json.as_str()
                }
                other => panic!("unexpected part: {other:?}"),
            };
            assert_eq!(fragments[&u32::try_from(index).unwrap()], text);
        }
        assert_eq!(result.content.len(), 4);
    }

    #[test]
    fn terminal_content_resolves_a_delta_after_an_empty_part() {
        let mut decoder = StreamDecoder::new(ExchangeFacts::default());
        let mut sink = Vec::new();
        apply(
            &mut decoder,
            json!({"type":"response.output_text.delta","output_index":0,"content_index":1,"item_id":"msg_fixture","delta":"ready"}),
            &mut sink,
        );
        assert!(sink.is_empty());
        let mut event = terminal();
        event["response"]["output"][0]["content"]
            .as_array_mut()
            .unwrap()
            .insert(0, json!({"type":"output_text","text":""}));
        apply(&mut decoder, event, &mut sink);
        assert!(sink.iter().any(|observation| observation.fact
            == ObservationFact::TextDelta {
                index: 0,
                text: "ready".to_string()
            }));
    }

    #[test]
    fn completed_message_layout_rejects_observed_content_outside_its_final_bounds() {
        for terminal_snapshot in [false, true] {
            for stale_index in [1, 7] {
                let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                let mut sink = Vec::new();
                assert!(matches!(
                    apply(
                        &mut decoder,
                        json!({
                            "type":"response.output_text.delta","output_index":0,
                            "content_index":stale_index,"item_id":"msg_fixture","delta":"orphan"
                        }),
                        &mut sink
                    ),
                    StreamStep::Continue
                ));
                assert!(sink.is_empty());
                let event = if terminal_snapshot {
                    terminal()
                } else {
                    json!({"type":"response.output_item.done","output_index":0,
                        "item":terminal()["response"]["output"][0]})
                };
                let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink) else {
                    panic!("a stale content index must terminate at layout completion");
                };
                assert!(matches!(
                    *evidence,
                    TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                        cause: LossCause::StreamProtocolViolation { .. },
                        ..
                    })
                ));
                assert!(!sink.iter().any(|observation| matches!(
                    observation.fact,
                    ObservationFact::TextDelta { .. } | ObservationFact::FinishReported(_)
                )));
            }
        }
    }

    #[test]
    fn completed_layout_cannot_empty_a_part_targeted_by_an_emitted_delta() {
        for terminal_snapshot in [false, true] {
            let mut decoder = StreamDecoder::new(ExchangeFacts::default());
            let mut sink = Vec::new();
            apply(
                &mut decoder,
                json!({"type":"response.output_text.delta","output_index":0,
                "content_index":0,"item_id":"msg_fixture","delta":"ready"}),
                &mut sink,
            );
            assert_eq!(
                sink[0].fact,
                ObservationFact::TextDelta {
                    index: 0,
                    text: "ready".to_string()
                }
            );
            let mut event = terminal();
            event["response"]["output"][0]["content"][0]["text"] = json!("");
            if !terminal_snapshot {
                event = json!({"type":"response.output_item.done","output_index":0,"item":event["response"]["output"][0]});
            }
            assert!(
                matches!(apply(&mut decoder,event,&mut sink), StreamStep::Terminal(evidence)
                if matches!(*evidence,TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {cause:LossCause::StreamProtocolViolation {..},..})))
            );
            assert!(
                !sink.iter().any(|observation| matches!(
                    observation.fact,
                    ObservationFact::FinishReported(_)
                ))
            );
        }
    }

    #[test]
    fn completed_layout_cannot_grow_and_shift_an_already_emitted_part() {
        for terminal_snapshot in [false, true] {
            let mut decoder = StreamDecoder::new(ExchangeFacts::default());
            let mut sink = Vec::new();
            let mut event = terminal();
            apply(
                &mut decoder,
                json!({"type":"response.output_item.done","output_index":0,"item":event["response"]["output"][0]}),
                &mut sink,
            );
            apply(
                &mut decoder,
                json!({"type":"response.output_text.delta","output_index":1,"content_index":0,"item_id":"msg_second","delta":"second"}),
                &mut sink,
            );
            assert_eq!(
                sink[0].fact,
                ObservationFact::TextDelta {
                    index: 1,
                    text: "second".to_string()
                }
            );
            event["response"]["output"][0]["content"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"output_text","text":"inserted"}));
            event["response"]["output"].as_array_mut().unwrap().push(json!({"type":"message","id":"msg_second","status":"completed","role":"assistant","content":[{"type":"output_text","text":"second"}]}));
            if !terminal_snapshot {
                event = json!({"type":"response.output_item.done","output_index":0,"item":event["response"]["output"][0]});
            }
            assert!(
                matches!(apply(&mut decoder,event,&mut sink),StreamStep::Terminal(evidence)
                if matches!(*evidence,TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {cause:LossCause::StreamProtocolViolation {..},..})))
            );
            assert!(
                !sink.iter().any(|observation| matches!(
                    observation.fact,
                    ObservationFact::FinishReported(_)
                ))
            );
        }
    }

    #[test]
    fn text_delta_kind_must_agree_with_item_declarations_in_either_order() {
        for item_kind in ["reasoning", "function_call"] {
            for declaration_first in [false, true] {
                for terminal_snapshot in [false, true] {
                    let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                    let mut sink = Vec::new();
                    let item = json!({"type":item_kind,"id":"msg_fixture","call_id":"call_fixture","name":"lookup","arguments":"{}"});
                    let delta = json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg_fixture","delta":"ready"});
                    let declaration =
                        json!({"type":"response.output_item.added","output_index":0,"item":item});
                    let conflicting = if declaration_first {
                        assert!(matches!(
                            apply(&mut decoder, declaration, &mut sink),
                            StreamStep::Continue
                        ));
                        delta
                    } else {
                        assert!(matches!(
                            apply(&mut decoder, delta, &mut sink),
                            StreamStep::Continue
                        ));
                        if terminal_snapshot {
                            let mut event = terminal();
                            event["response"]["output"] = json!([item]);
                            event
                        } else {
                            declaration
                        }
                    };
                    assert!(
                        matches!(apply(&mut decoder,conflicting,&mut sink),StreamStep::Terminal(evidence)
                        if matches!(*evidence,TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {cause:LossCause::StreamProtocolViolation {..},..})))
                    );
                }
            }
        }
    }

    #[test]
    fn failed_terminal_preserves_provider_error_despite_malformed_partial_output() {
        for item in [
            json!({"type":"message"}),
            json!({"type":"message","id":"changed","content":"invalid"}),
            json!(42),
        ] {
            let mut decoder = StreamDecoder::new(ExchangeFacts::default());
            let mut sink = Vec::new();
            apply(
                &mut decoder,
                json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg_fixture","delta":"partial"}),
                &mut sink,
            );
            let mut event = terminal();
            event["type"] = json!("response.failed");
            event["response"]["status"] = json!("failed");
            event["response"]["error"] =
                json!({"code":"server_error","message":"generation failed"});
            event["response"]["output"] = json!([item]);
            let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink) else {
                panic!("failed terminal must terminate");
            };
            let TerminalEvidence::ProviderError(error) = *evidence else {
                panic!("partial output must not erase a definitive provider error");
            };
            assert_eq!(error.kind, ProviderErrorKind::ProviderInternal);
            assert_eq!(error.native.message.as_deref(), Some("generation failed"));
            assert!(!error.non_acceptance_proven);
        }
    }

    #[test]
    fn failed_terminal_keeps_provider_error_when_output_is_not_an_array() {
        for output in [json!({}), json!("invalid"), json!(42)] {
            let mut event = terminal();
            event["type"] = json!("response.failed");
            event["response"]["status"] = json!("failed");
            event["response"]["error"] =
                json!({"code":"server_error","message":"generation failed"});
            event["response"]["output"] = output;
            let TerminalEvidence::ProviderError(error) = decode(event) else {
                panic!("malformed output must not erase the failed envelope");
            };
            assert_eq!(error.kind, ProviderErrorKind::ProviderInternal);
            assert_eq!(error.native.error_code.as_deref(), Some("server_error"));
            assert_eq!(error.native.message.as_deref(), Some("generation failed"));
            assert!(!error.non_acceptance_proven);
        }
    }

    #[test]
    fn completed_and_incomplete_terminals_require_no_error_and_array_output() {
        for status in ["completed", "incomplete"] {
            for error in [Value::Null, json!({}), json!({"code":"server_error"})] {
                let mut event = terminal();
                event["type"] = json!(format!("response.{status}"));
                event["response"]["status"] = json!(status);
                if status == "incomplete" {
                    event["response"]["incomplete_details"] = json!({"reason":"max_output_tokens"});
                }
                event["response"]["error"] = error.clone();
                let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                let mut sink = Vec::new();
                let StreamStep::Terminal(evidence) = apply(&mut decoder, event.clone(), &mut sink)
                else {
                    panic!("terminal event must terminate");
                };
                if error.is_null() {
                    assert!(matches!(*evidence, TerminalEvidence::Completed(_)));
                } else {
                    assert!(matches!(
                        *evidence,
                        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                            cause: LossCause::StreamProtocolViolation { .. },
                            ..
                        })
                    ));
                    assert!(!sink.iter().any(|o| matches!(
                        o.fact,
                        ObservationFact::FinishReported(_) | ObservationFact::ToolCallProposed(_)
                    )));
                }
                event["response"]["output"] = json!({});
                assert!(matches!(decode(event), TerminalEvidence::BoundaryLoss(_)));
            }
        }
    }

    #[test]
    fn completed_events_reject_incomplete_details_before_announcing_a_finish() {
        for tool in [false, true] {
            for details in [
                Value::Null,
                json!({"reason":"max_output_tokens"}),
                json!({"reason":"content_filter"}),
            ] {
                let mut event = terminal();
                if tool {
                    event["response"]["output"] = json!([{"type":"function_call","id":"fc_fixture","status":"completed","call_id":"call_fixture","name":"lookup","arguments":"{}"}]);
                }
                event["response"]["incomplete_details"] = details.clone();
                let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                let mut sink = Vec::new();
                let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink) else {
                    panic!("terminal event must terminate");
                };
                if details.is_null() {
                    let TerminalEvidence::Completed(result) = *evidence else {
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
                    let TerminalEvidence::BoundaryLoss(loss) = *evidence else {
                        panic!("completed event with incomplete details must fail closed");
                    };
                    assert!(matches!(
                        loss.cause,
                        LossCause::StreamProtocolViolation { .. }
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
                    assert!(!sink.iter().any(|o| matches!(
                        o.fact,
                        ObservationFact::FinishReported(_) | ObservationFact::ToolCallProposed(_)
                    )));
                }
            }
        }
    }

    #[test]
    fn terminal_output_must_include_every_observed_output_index_before_flushing() {
        for status in ["completed", "incomplete"] {
            for index in [0, 1, 7] {
                for added in [false, true] {
                    let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                    let mut sink = Vec::new();
                    let observed = if added {
                        json!({"type":"response.output_item.added","output_index":index,
                            "item":{"type":"message","id":"msg_missing","status":"in_progress","role":"assistant","content":[]}})
                    } else {
                        json!({"type":"response.output_text.delta","output_index":index,
                            "content_index":0,"item_id":"msg_missing","delta":"missing"})
                    };
                    assert!(matches!(
                        apply(&mut decoder, observed, &mut sink),
                        StreamStep::Continue
                    ));
                    let observed_count = sink.len();
                    let mut event = terminal();
                    event["type"] = json!(format!("response.{status}"));
                    event["response"]["status"] = json!(status);
                    if status == "incomplete" {
                        event["response"]["incomplete_details"] =
                            json!({"reason":"max_output_tokens"});
                    }
                    if index == 0 {
                        event["response"]["output"] = json!([]);
                    }
                    let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink)
                    else {
                        panic!("terminal event must terminate");
                    };
                    assert!(matches!(
                        *evidence,
                        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                            cause: LossCause::StreamProtocolViolation { .. },
                            ..
                        })
                    ));
                    assert!(!sink[observed_count..].iter().any(|o| matches!(
                        o.fact,
                        ObservationFact::TextDelta { .. } | ObservationFact::FinishReported(_)
                    )));
                }
            }
        }
    }

    #[test]
    fn unknown_snapshot_output_withholds_the_no_tool_claim() {
        for kind in [
            "response.in_progress",
            "response.completed",
            "response.output_item.added",
        ] {
            let mut decoder = StreamDecoder::new(ExchangeFacts::default());
            let mut event = terminal();
            event["type"] = json!(kind);
            event["response"]["output"] = json!([{"type":"web_search_call","id":"ws_fixture"}]);
            event["output_index"] = json!(0);
            event["item"] = json!({"type":"web_search_call","id":"ws_fixture"});
            let evidence = match apply(&mut decoder, event, &mut Vec::new()) {
                StreamStep::Continue => decoder.lost(StreamInterruption::EndOfStream),
                StreamStep::Terminal(evidence) => *evidence,
            };
            assert!(
                matches!(
                    evidence,
                    TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                        tool_calls: ToolCallsAtLoss::Unobserved,
                        ..
                    })
                ),
                "{kind}"
            );
        }
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
    fn unknown_events_withhold_no_tool_evidence_without_erasing_opened_calls() {
        for opened in [false, true] {
            let mut decoder = StreamDecoder::new(ExchangeFacts::default());
            let mut sink = Vec::new();
            if opened {
                assert!(matches!(
                    apply(
                        &mut decoder,
                        json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_fixture","delta":"{}"}),
                        &mut sink
                    ),
                    StreamStep::Continue
                ));
            }
            let StreamStep::Terminal(evidence) = apply(
                &mut decoder,
                json!({"type":"response.future_tool_event","future_tool":{"id":"tool_fixture"}}),
                &mut sink,
            ) else {
                panic!("unknown event must fail closed");
            };
            let TerminalEvidence::BoundaryLoss(loss) = *evidence else {
                panic!("unknown event is protocol loss");
            };
            assert!(matches!(
                loss.cause,
                LossCause::StreamProtocolViolation { .. }
            ));
            assert_eq!(
                loss.tool_calls,
                if opened {
                    ToolCallsAtLoss::Opened
                } else {
                    ToolCallsAtLoss::Unobserved
                }
            );
        }
    }

    #[test]
    fn lifecycle_events_require_nonempty_models_even_after_a_valid_snapshot() {
        for kind in [
            "response.created",
            "response.in_progress",
            "response.queued",
        ] {
            for prior_snapshot in [false, true] {
                for model in [None, Some(Value::Null), Some(json!(""))] {
                    let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                    let mut sink = Vec::new();
                    if prior_snapshot {
                        assert!(matches!(
                            apply(
                                &mut decoder,
                                json!({"type":"response.created","response":{"id":"resp_fixture","model":"model-fixture","output":[]}}),
                                &mut sink
                            ),
                            StreamStep::Continue
                        ));
                    }
                    let mut event =
                        json!({"type":kind,"response":{"id":"resp_fixture","output":[]}});
                    if let Some(model) = model {
                        event["response"]["model"] = model;
                    }
                    assert!(
                        matches!(apply(&mut decoder, event, &mut sink), StreamStep::Terminal(evidence)
                        if matches!(*evidence, TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                            cause: LossCause::StreamProtocolViolation {..}, ..
                        })))
                    );
                }
            }
        }
    }

    #[test]
    fn failed_invalid_prompt_event_is_an_invalid_request_without_proof() {
        let mut event = terminal();
        event["type"] = json!("response.failed");
        event["response"]["status"] = json!("failed");
        event["response"]["error"] = json!({"code":"invalid_prompt","message":"prompt rejected"});
        let TerminalEvidence::ProviderError(error) = decode(event) else {
            panic!("accepted failure must retain definitive provider-error evidence");
        };
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert_eq!(error.native.error_code.as_deref(), Some("invalid_prompt"));
        assert!(!error.non_acceptance_proven);
    }

    #[test]
    fn streamed_function_call_content_respects_terminal_response_status() {
        for status in ["completed", "incomplete"] {
            for item_status in [
                None,
                Some("in_progress"),
                Some("incomplete"),
                Some("future"),
                Some("completed"),
            ] {
                let mut event = terminal();
                event["type"] = json!(format!("response.{status}"));
                event["response"]["status"] = json!(status);
                if status == "incomplete" {
                    event["response"]["incomplete_details"] = json!({"reason":"max_output_tokens"});
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
                event["response"]["output"] = json!([call]);
                let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                let mut sink = Vec::new();
                for fragment in [&arguments[..1], &arguments[1..]] {
                    assert!(matches!(
                        apply(
                            &mut decoder,
                            json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_fixture","delta":fragment}),
                            &mut sink
                        ),
                        StreamStep::Continue
                    ));
                }
                let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink) else {
                    panic!("terminal event must terminate");
                };
                if item_status == Some("completed")
                    || (status == "incomplete" && item_status == Some("incomplete"))
                {
                    let TerminalEvidence::Completed(result) = *evidence else {
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
                        sink.iter()
                            .any(|o| matches!(o.fact, ObservationFact::ToolCallProposed(_)))
                    );
                } else {
                    assert!(matches!(
                        *evidence,
                        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                            cause: LossCause::StreamProtocolViolation { .. },
                            tool_calls: ToolCallsAtLoss::Opened,
                            ..
                        })
                    ));
                    assert!(!sink.iter().any(|o| matches!(
                        o.fact,
                        ObservationFact::FinishReported(_) | ObservationFact::ToolCallProposed(_)
                    )));
                }
            }
        }
    }

    #[test]
    fn streamed_message_content_respects_terminal_response_status() {
        for status in ["completed", "incomplete"] {
            for item_status in [
                None,
                Some("in_progress"),
                Some("incomplete"),
                Some("future"),
                Some("completed"),
            ] {
                let mut event = terminal();
                event["type"] = json!(format!("response.{status}"));
                event["response"]["status"] = json!(status);
                if status == "incomplete" {
                    event["response"]["incomplete_details"] = json!({"reason":"max_output_tokens"});
                }
                if let Some(item_status) = item_status {
                    event["response"]["output"][0]["status"] = json!(item_status);
                } else {
                    event["response"]["output"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("status");
                }
                let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                let mut sink = Vec::new();
                let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink) else {
                    panic!("terminal event must terminate");
                };
                if item_status == Some("completed")
                    || (status == "incomplete" && item_status == Some("incomplete"))
                {
                    let TerminalEvidence::Completed(result) = *evidence else {
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
                        vec![signalbox_model_runtime::AssistantPart::Text(
                            "ready".to_string()
                        )]
                    );
                } else {
                    assert!(matches!(
                        *evidence,
                        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                            cause: LossCause::StreamProtocolViolation { .. },
                            ..
                        })
                    ));
                    assert!(!sink.iter().any(|o| matches!(
                        o.fact,
                        ObservationFact::FinishReported(_) | ObservationFact::ToolCallProposed(_)
                    )));
                }
            }
        }
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
                if status == "incomplete" {
                    event["response"]["incomplete_details"] = json!({"reason":"max_output_tokens"});
                }
                if status == "failed" {
                    event["response"]["error"] = json!({"code":"server_error"});
                }
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
    fn first_terminal_status_disagreement_retains_reported_metadata() {
        for (wrapper, status, error) in [
            ("response.completed", "in_progress", Value::Null),
            ("response.incomplete", "completed", Value::Null),
            ("response.failed", "completed", Value::Null),
            (
                "response.completed",
                "failed",
                json!({"code":"server_error","message":"failed"}),
            ),
        ] {
            let mut event = terminal();
            event["type"] = json!(wrapper);
            event["response"]["status"] = json!(status);
            event["response"]["error"] = error;
            let mut decoder = StreamDecoder::new(ExchangeFacts::default());
            let mut sink = Vec::new();
            let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink) else {
                panic!("contradictory terminal envelope must terminate");
            };
            let TerminalEvidence::BoundaryLoss(loss) = *evidence else {
                panic!("contradictory terminal envelope must remain boundary loss");
            };
            assert!(matches!(
                loss.cause,
                LossCause::StreamProtocolViolation { .. }
            ));
            assert_eq!(loss.tool_calls, ToolCallsAtLoss::Unobserved);
            assert_eq!(loss.finish_reported, None);
            assert_eq!(
                loss.reported_model,
                Some(ProviderReportedModel::new("model-fixture")),
                "{wrapper} / {status}"
            );
            assert_eq!(
                loss.usage,
                TokenUsage {
                    input_tokens: Some(5),
                    output_tokens: Some(2),
                    ..TokenUsage::unreported()
                }
            );
            assert_eq!(sink.len(), 1);
            assert_eq!(
                sink[0].fact,
                ObservationFact::ProviderModelReported(ProviderReportedModel::new("model-fixture"))
            );
        }
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
    fn terminal_reasoning_keeps_completed_replay_bytes_when_ciphertext_changes() {
        for (status, details, finish) in [
            ("completed", Value::Null, CompletionFinish::EndTurn),
            (
                "incomplete",
                json!({"reason":"max_output_tokens"}),
                CompletionFinish::MaxOutputTokens,
            ),
        ] {
            for mut reasoning in [
                json!({}),
                json!({"encrypted_content":null}),
                json!({"encrypted_content":"complete"}),
                json!({"encrypted_content":"changed"}),
                json!({"encrypted_content":""}),
            ] {
                let completed = json!({"type":"reasoning","id":"rs_fixture","summary":[],"encrypted_content":"complete"});
                let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                let mut sink = Vec::new();
                assert!(matches!(
                    apply(
                        &mut decoder,
                        json!({
                            "type":"response.output_item.done","output_index":0,"item":completed
                        }),
                        &mut sink
                    ),
                    StreamStep::Continue
                ));
                reasoning["type"] = json!("reasoning");
                reasoning["id"] = json!("rs_fixture");
                let mut event = terminal();
                event["type"] = json!(format!("response.{status}"));
                event["response"]["status"] = json!(status);
                event["response"]["incomplete_details"] = details.clone();
                event["response"]["output"]
                    .as_array_mut()
                    .unwrap()
                    .insert(0, reasoning.clone());
                let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink) else {
                    panic!("{status} / {reasoning}: terminal must terminate");
                };
                let TerminalEvidence::Completed(completion) = *evidence else {
                    panic!(
                        "{status} / {reasoning}: terminal ciphertext cannot replace completed replay bytes"
                    );
                };
                assert_eq!(completion.finish, finish, "{status} / {reasoning}");
                assert_eq!(
                    completion.content[0],
                    signalbox_model_runtime::AssistantPart::ProviderReasoning {
                        item_json: completed.to_string(),
                    },
                    "{status} / {reasoning}"
                );
            }
        }
    }

    #[test]
    fn completed_reasoning_without_a_terminal_marker_is_incomplete_stream_evidence() {
        let mut decoder = StreamDecoder::new(ExchangeFacts::default());
        let mut sink = Vec::new();
        assert!(matches!(
            apply(
                &mut decoder,
                json!({
                    "type":"response.output_item.done","output_index":0,
                    "item":{"type":"reasoning","id":"rs_fixture","encrypted_content":"complete","summary":[]}
                }),
                &mut sink
            ),
            StreamStep::Continue
        ));
        assert!(
            sink.is_empty(),
            "item completion cannot announce a response finish"
        );
        assert!(matches!(
            decoder.lost(StreamInterruption::EndOfStream),
            TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                response_content_observed: true,
                finish_reported: None,
                cause: LossCause::StreamEndedWithoutTerminalMarker { .. },
                ..
            })
        ));
    }

    #[test]
    fn reasoning_replay_uses_the_completed_item_instead_of_the_added_snapshot() {
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
            if kind == "response.output_item.added" {
                apply(
                    &mut decoder,
                    json!({"type":"response.output_text.delta","output_index":1,"content_index":0,"item_id":"msg_fixture","delta":"ready"}),
                    &mut sink,
                );
                assert!(sink.is_empty());
            }
        }
        assert_eq!(sink.len(), 1);
        assert_eq!(
            sink[0].fact,
            ObservationFact::TextDelta {
                index: 1,
                text: "ready".to_string()
            }
        );
        let mut event = terminal();
        event["response"]["output"].as_array_mut().unwrap().insert(
            0,
            json!({"type":"reasoning","id":"rs_fixture","encrypted_content":"complete"}),
        );
        let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink) else {
            panic!("terminal event must terminate");
        };
        let TerminalEvidence::Completed(completion) = *evidence else {
            panic!("matching item identities must complete");
        };
        assert_eq!(completion.content.len(), 2);
        assert_eq!(completion.content[0], signalbox_model_runtime::AssistantPart::ProviderReasoning {
            item_json: json!({"type":"reasoning","id":"rs_fixture","summary":[],"encrypted_content":"complete"}).to_string(),
        });
        assert!(
            !sink.iter().any(|observation| matches!(
                observation.fact,
                ObservationFact::ThinkingDelta { .. }
            ))
        );
    }
    #[test]
    fn indexed_content_and_reasoning_events_must_preserve_the_established_item_id() {
        for kind in [
            "response.content_part.added",
            "response.content_part.done",
            "response.output_text.done",
            "response.output_text.annotation.added",
            "response.refusal.done",
            "response.function_call_arguments.done",
            "response.reasoning_text.delta",
            "response.reasoning_text.done",
            "response.reasoning_summary_part.added",
            "response.reasoning_summary_part.done",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.done",
        ] {
            for id in ["item_established", "item_conflicting"] {
                let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                let mut sink = Vec::new();
                let item_kind = if kind.starts_with("response.reasoning_") {
                    "reasoning"
                } else if kind == "response.function_call_arguments.done" {
                    "function_call"
                } else {
                    "message"
                };
                apply(
                    &mut decoder,
                    json!({"type":"response.output_item.added",
                    "output_index":0,"item":{"type":item_kind,"id":"item_established"}}),
                    &mut sink,
                );
                let mut event = json!({"type":kind,"output_index":0,"item_id":id});
                match kind {
                    "response.content_part.added" | "response.content_part.done" => {
                        event["content_index"] = json!(0);
                        event["part"] = json!({"type":"output_text","text":"ready"});
                    }
                    "response.output_text.done" => {
                        event["content_index"] = json!(0);
                        event["text"] = json!("ready");
                    }
                    "response.refusal.done" => {
                        event["content_index"] = json!(0);
                        event["refusal"] = json!("declined");
                    }
                    "response.function_call_arguments.done" => event["arguments"] = json!("{}"),
                    _ => {}
                }
                if kind.starts_with("response.reasoning_summary_part.") {
                    event["summary_index"] = json!(0);
                    event["part"] = json!({"type":"summary_text","text":"summary"});
                }
                let step = apply(&mut decoder, event, &mut sink);
                if id == "item_established" {
                    assert!(matches!(step, StreamStep::Continue), "{kind}");
                } else {
                    assert!(
                        matches!(step, StreamStep::Terminal(evidence) if matches!(
                            *evidence, TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                                cause: LossCause::StreamProtocolViolation { .. }, ..
                            })
                        )),
                        "{kind}"
                    );
                }
            }
        }
    }

    #[test]
    fn terminal_output_must_preserve_item_ids_at_their_established_indices() {
        for status in ["completed", "incomplete"] {
            for swap_items in [false, true] {
                let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                let mut sink = Vec::new();
                let mut event = terminal();
                event["type"] = json!(format!("response.{status}"));
                event["response"]["status"] = json!(status);
                if status == "incomplete" {
                    event["response"]["incomplete_details"] = json!({"reason":"max_output_tokens"});
                }
                event["response"]["output"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"type":"reasoning","id":"rs_fixture"}));
                for (index, item) in event["response"]["output"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .enumerate()
                {
                    apply(
                        &mut decoder,
                        json!({"type":"response.output_item.added", "output_index":index,"item":item}),
                        &mut sink,
                    );
                }
                if swap_items {
                    event["response"]["output"]
                        .as_array_mut()
                        .unwrap()
                        .swap(0, 1);
                }
                let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink) else {
                    panic!("terminal event must terminate");
                };
                if swap_items {
                    assert!(
                        matches!(
                            *evidence,
                            TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                                cause: LossCause::StreamProtocolViolation { .. },
                                ..
                            })
                        ),
                        "{status}"
                    );
                } else {
                    assert!(
                        matches!(
                            *evidence,
                            TerminalEvidence::Completed(_) | TerminalEvidence::ProviderError(_)
                        ),
                        "{status}"
                    );
                }
            }
        }
    }

    #[test]
    fn lifecycle_snapshot_tools_remain_open_at_loss_despite_unexamined_later_bytes() {
        for kind in [
            "response.created",
            "response.in_progress",
            "response.queued",
        ] {
            for item_kind in ["message", "function_call"] {
                let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                assert!(matches!(
                    apply(
                        &mut decoder,
                        json!({"type":kind,"response":{
                            "id":"resp_fixture","model":"model-fixture","output":[{"type":item_kind,"id":"item_fixture"}]
                        }}),
                        &mut Vec::new()
                    ),
                    StreamStep::Continue
                ));
                if item_kind == "function_call" {
                    decoder.note_discarded_unexamined_bytes();
                    assert!(matches!(
                        decoder.cancelled(),
                        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                            tool_calls: ToolCallsAtLoss::Opened,
                            ..
                        })
                    ));
                }
                let TerminalEvidence::BoundaryLoss(loss) =
                    decoder.lost(StreamInterruption::EndOfStream)
                else {
                    panic!("EOF without a terminal event must lose the boundary");
                };
                assert_eq!(
                    loss.tool_calls,
                    if item_kind == "function_call" {
                        ToolCallsAtLoss::Opened
                    } else {
                        ToolCallsAtLoss::NoneOpened
                    },
                    "{kind}"
                );
            }
        }
    }

    #[test]
    fn malformed_snapshot_output_withholds_the_no_tool_claim() {
        let evidence = decode(json!({"type":"response.in_progress","response":{
            "id":"resp_fixture","model":"model-fixture","output":[{"type":"function_call","arguments":42}]
        }}));
        assert!(matches!(
            evidence,
            TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                tool_calls: ToolCallsAtLoss::Unobserved,
                ..
            })
        ));
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
            "item":{"type":"function_call","id":"fc_fixture","status":"in_progress","call_id":"call_fixture","name":"lookup","arguments":""}}),
            &mut sink,
        );
        assert!(
            matches!(apply(&mut decoder, terminal(), &mut sink), StreamStep::Terminal(evidence)
            if matches!(*evidence, TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {tool_calls: ToolCallsAtLoss::Opened, ..})))
        );
    }
    #[test]
    fn streamed_item_conversion_loss_retains_only_recognized_terminal_finishes() {
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
                let mut event = terminal();
                event["type"] = json!(format!("response.{status}"));
                event["response"]["status"] = json!(status);
                if let Some(reason) = reason {
                    event["response"]["incomplete_details"] = json!({"reason":reason});
                }
                event["response"]["output"] = json!([first, second]);
                let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                let mut observations = Vec::new();
                let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut observations)
                else {
                    panic!("terminal event must terminate");
                };
                let TerminalEvidence::BoundaryLoss(loss) = *evidence else {
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
                    LossCause::StreamProtocolViolation { .. }
                ));
                assert!(!observations.iter().any(|o| matches!(
                    o.fact,
                    ObservationFact::ToolCallProposed(_) | ObservationFact::FinishReported(_)
                )));
            }
        }
    }
    #[test]
    fn streamed_reasoning_status_must_agree_with_its_terminal_response() {
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
                    let mut event = terminal();
                    event["type"] = json!(format!("response.{status}"));
                    event["response"]["status"] = json!(status);
                    if status == "incomplete" {
                        event["response"]["incomplete_details"] =
                            json!({"reason":"max_output_tokens"});
                    }
                    event["response"]["output"] = json!([reasoning, content]);
                    let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                    let mut observations = Vec::new();
                    let StreamStep::Terminal(evidence) =
                        apply(&mut decoder, event, &mut observations)
                    else {
                        panic!("terminal event must terminate");
                    };
                    let evidence = *evidence;
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
                            LossCause::StreamProtocolViolation { .. }
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
    fn streamed_failed_envelopes_survive_malformed_ancillary_fields() {
        for status in [
            "failed",
            "completed",
            "incomplete",
            "created",
            "in_progress",
            "queued",
        ] {
            for field in ["id", "object", "model", "usage", "incomplete_details"] {
                for malformed in [
                    json!(42),
                    json!([]),
                    json!({"input_tokens":"invalid","reason":42}),
                ] {
                    let mut event = terminal();
                    event["type"] = json!(format!("response.{status}"));
                    event["response"]["status"] = json!(status);
                    if status == "failed" {
                        event["response"]["error"] =
                            json!({"code":"server_error","message":"generation failed"});
                    }
                    if status == "incomplete" {
                        event["response"]["incomplete_details"] =
                            json!({"reason":"max_output_tokens"});
                    }
                    event["response"][field] = malformed;
                    let mut decoder = StreamDecoder::new(ExchangeFacts {
                        http_status: Some(200),
                        ..ExchangeFacts::default()
                    });
                    let mut observations = Vec::new();
                    assert!(matches!(
                        apply(
                            &mut decoder,
                            json!({"type":"response.created","response":{
                                "id":"resp_fixture","model":"model-fixture","status":"in_progress","output":[],
                                "usage":{"input_tokens":23,"output_tokens":7,"input_tokens_details":{"cached_tokens":5,"cache_write_tokens":3}}
                            }}),
                            &mut observations
                        ),
                        StreamStep::Continue
                    ));
                    let StreamStep::Terminal(evidence) =
                        apply(&mut decoder, event, &mut observations)
                    else {
                        panic!("malformed ancillary fields must terminate this fixture");
                    };
                    let evidence = *evidence;
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
                        let expected = TokenUsage {
                            input_tokens: Some(if field == "usage" { 23 } else { 5 }),
                            output_tokens: Some(if field == "usage" { 7 } else { 2 }),
                            cache_read_input_tokens: Some(5),
                            cache_creation_input_tokens: Some(3),
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
    fn failed_status_and_error_survive_missing_malformed_or_changed_identity() {
        for prior in [false, true] {
            for ancillary in [
                json!({}),
                json!({
                    "id":42,"object":[],"model":{},"output":{},"usage":"invalid","incomplete_details":[]
                }),
                json!({"id":"resp_other","object":"other","model":"other-model"}),
            ] {
                let mut response = ancillary;
                response["status"] = json!("failed");
                response["error"] = json!({"code":"invalid_prompt","message":"rejected prompt"});
                let mut decoder = StreamDecoder::new(ExchangeFacts {
                    http_status: Some(200),
                    ..ExchangeFacts::default()
                });
                let mut sink = Vec::new();
                if prior {
                    assert!(matches!(
                        apply(
                            &mut decoder,
                            json!({"type":"response.created","response":{
                                "id":"resp_fixture","model":"model-fixture","status":"in_progress","output":[],
                                "usage":{"input_tokens":23}
                            }}),
                            &mut sink
                        ),
                        StreamStep::Continue
                    ));
                }
                let expected_model = response["model"]
                    .as_str()
                    .or(prior.then_some("model-fixture"));
                let expected_model = expected_model.map(ProviderReportedModel::new);
                let StreamStep::Terminal(evidence) = apply(
                    &mut decoder,
                    json!({"type":"response.failed","response":response}),
                    &mut sink,
                ) else {
                    panic!("failed response must terminate");
                };
                let TerminalEvidence::ProviderError(error) = *evidence else {
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
                assert_eq!(error.reported_model, expected_model);
                assert_eq!(
                    error.usage,
                    TokenUsage {
                        input_tokens: prior.then_some(23),
                        ..TokenUsage::unreported()
                    }
                );
                assert!(!sink.iter().any(|o| matches!(
                    o.fact,
                    ObservationFact::FinishReported(_) | ObservationFact::ToolCallProposed(_)
                )));
            }
        }
    }

    #[test]
    fn item_done_rejects_later_deltas_even_when_the_layout_is_unchanged() {
        for (item, delta_type) in [
            (
                json!({"type":"message","id":"item_fixture","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"ready"}]}),
                "response.output_text.delta",
            ),
            (
                json!({"type":"message","id":"item_fixture","role":"assistant","status":"completed",
                "content":[{"type":"refusal","refusal":"declined"}]}),
                "response.refusal.delta",
            ),
            (
                json!({"type":"function_call","id":"item_fixture","status":"completed","call_id":"call_fixture",
                "name":"lookup","arguments":"{}"}),
                "response.function_call_arguments.delta",
            ),
            (
                json!({"type":"reasoning","id":"item_fixture","status":"completed","summary":[]}),
                "response.reasoning_text.delta",
            ),
            (
                json!({"type":"reasoning","id":"item_fixture","status":"completed","summary":[]}),
                "response.reasoning_summary_text.delta",
            ),
        ] {
            for done in [false, true] {
                let mut decoder = StreamDecoder::new(ExchangeFacts::default());
                let mut sink = Vec::new();
                assert!(matches!(
                    apply(
                        &mut decoder,
                        json!({
                            "type":if done { "response.output_item.done" } else { "response.output_item.added" },
                            "output_index":0,"item":item
                        }),
                        &mut sink
                    ),
                    StreamStep::Continue
                ));
                let observations_before = sink.len();
                let step = apply(
                    &mut decoder,
                    json!({"type":delta_type,"output_index":0,
                    "content_index":0,"summary_index":0,"item_id":"item_fixture","delta":" "}),
                    &mut sink,
                );
                if done {
                    let StreamStep::Terminal(evidence) = step else {
                        panic!("{delta_type} must be rejected after item completion");
                    };
                    let TerminalEvidence::BoundaryLoss(loss) = *evidence else {
                        panic!("post-completion deltas are protocol loss");
                    };
                    assert!(matches!(
                        loss.cause,
                        LossCause::StreamProtocolViolation { .. }
                    ));
                    assert_eq!(
                        loss.tool_calls,
                        if item["type"] == "function_call" {
                            ToolCallsAtLoss::Opened
                        } else {
                            ToolCallsAtLoss::NoneOpened
                        }
                    );
                    assert_eq!(sink.len(), observations_before);
                } else {
                    assert!(
                        matches!(step, StreamStep::Continue),
                        "{delta_type} remains open after added"
                    );
                }
            }
        }
    }
    #[test]
    fn first_incomplete_terminal_retains_facts_when_output_cannot_be_decoded() {
        for (reason, expected_finish) in [
            ("max_output_tokens", Some(FinishReason::MaxOutputTokens)),
            ("content_filter", Some(FinishReason::Refusal)),
            ("unknown", None),
        ] {
            for output in [
                json!({}),
                json!("invalid"),
                json!([{"type":"message","id":"msg_fixture","content":42}]),
            ] {
                let mut event = terminal();
                event["type"] = json!("response.incomplete");
                event["response"]["status"] = json!("incomplete");
                event["response"]["incomplete_details"] = json!({"reason":reason});
                event["response"]["output"] = output.clone();
                let mut decoder = StreamDecoder::new(ExchangeFacts {
                    http_status: Some(200),
                    ..ExchangeFacts::default()
                });
                let mut sink = Vec::new();
                let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink) else {
                    panic!("malformed terminal output must terminate");
                };
                let TerminalEvidence::BoundaryLoss(loss) = *evidence else {
                    panic!("malformed terminal output must remain boundary loss");
                };
                assert!(matches!(
                    loss.cause,
                    LossCause::StreamProtocolViolation { .. }
                ));
                assert_eq!(
                    loss.reported_model,
                    Some(ProviderReportedModel::new("model-fixture"))
                );
                assert_eq!(
                    loss.usage,
                    TokenUsage {
                        input_tokens: Some(5),
                        output_tokens: Some(2),
                        ..TokenUsage::unreported()
                    }
                );
                assert_eq!(loss.finish_reported, expected_finish, "{reason}: {output}");
                assert_eq!(loss.tool_calls, ToolCallsAtLoss::Unobserved);
                assert_eq!(loss.exchange.http_status, Some(200));
                assert!(sink.iter().any(|observation| observation.fact
                    == ObservationFact::ProviderModelReported(ProviderReportedModel::new(
                        "model-fixture"
                    ))));
                assert!(!sink.iter().any(|observation| matches!(
                    observation.fact,
                    ObservationFact::TextDelta { .. }
                        | ObservationFact::ToolCallProposed(_)
                        | ObservationFact::FinishReported(_)
                )));
            }
        }
    }

    #[test]
    fn malformed_terminal_output_preserves_prior_tools_and_merges_usage() {
        let mut decoder = StreamDecoder::new(ExchangeFacts::default());
        let mut sink = Vec::new();
        assert!(matches!(
            apply(
                &mut decoder,
                json!({"type":"response.created","response":{
                    "id":"resp_fixture","model":"model-fixture","status":"in_progress",
                    "output":[{"type":"function_call","id":"fc_fixture","status":"in_progress"}],
                    "usage":{"input_tokens":23,"output_tokens":7,"input_tokens_details":{"cached_tokens":5,"cache_write_tokens":3}}
                }}),
                &mut sink
            ),
            StreamStep::Continue
        ));
        let mut event = terminal();
        event["type"] = json!("response.incomplete");
        event["response"]["status"] = json!("incomplete");
        event["response"]["incomplete_details"] = json!({"reason":"max_output_tokens"});
        event["response"]["output"] = json!({});
        let StreamStep::Terminal(evidence) = apply(&mut decoder, event, &mut sink) else {
            panic!("malformed terminal output must terminate");
        };
        let TerminalEvidence::BoundaryLoss(loss) = *evidence else {
            panic!("malformed terminal output must remain boundary loss");
        };
        assert_eq!(
            loss.reported_model,
            Some(ProviderReportedModel::new("model-fixture"))
        );
        assert_eq!(loss.finish_reported, Some(FinishReason::MaxOutputTokens));
        assert_eq!(
            loss.usage,
            TokenUsage {
                input_tokens: Some(5),
                output_tokens: Some(2),
                cache_read_input_tokens: Some(5),
                cache_creation_input_tokens: Some(3)
            }
        );
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Opened);
    }
}
