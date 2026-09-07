use std::collections::VecDeque;

use serde_json::{Value, json};

use super::classify::TurnActivity;
use super::decode::{ProtocolError, decode};
use super::frame::{
    AgentMessage, ErrorNotification, ItemNotification, ItemTextDelta, RpcError, ThreadOptions,
    ThreadStartResponse, TokenUsageUpdated, Turn, TurnCompleted, TurnInput, TurnStartResponse,
    TurnStatus,
};

#[derive(Debug)]
pub(crate) enum Event {
    Ignored,
    RateLimitsUpdated,
    ThreadStarted(String),
    AgentMessage {
        message: AgentMessage,
        completed: bool,
    },
    Delta(ItemTextDelta),
    Usage(TokenUsageUpdated),
    Error(ErrorNotification),
    Terminal(Turn),
    Rejected {
        method: &'static str,
        error: RpcError,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Initialize,
    ThreadStart,
    TurnStart,
    Running,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RateLimitsRead {
    NotPending,
    Pending,
    Superseded,
}

pub(crate) struct Client {
    phase: Phase,
    rate_limits_read: RateLimitsRead,
    thread_params: Value,
    turn_params: Value,
    outbound: VecDeque<Vec<u8>>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    pub(crate) activity: TurnActivity,
    pub(crate) rate_limits: super::frame::RateLimits,
}

impl Client {
    pub(crate) fn new(thread: ThreadOptions, turn: TurnInput) -> Self {
        let mut thread_params = json!(thread);
        let turn_params = json!(turn);
        thread_params["ephemeral"] = json!(true);
        thread_params["sandbox"] = json!("read-only");
        thread_params["approvalPolicy"] = json!("never");
        let mut client = Self {
            phase: Phase::Initialize,
            rate_limits_read: RateLimitsRead::NotPending,
            thread_params,
            turn_params,
            outbound: VecDeque::new(),
            thread_id: None,
            turn_id: None,
            activity: TurnActivity::default(),
            rate_limits: super::frame::RateLimits::default(),
        };
        client.queue(json!({"id":1,"method":"initialize","params":{
            "clientInfo":{"name":"signalbox","version":env!("CARGO_PKG_VERSION")}
        }}));
        client
    }

    fn queue(&mut self, value: Value) {
        let mut bytes = value.to_string().into_bytes();
        bytes.push(b'\n');
        self.outbound.push_back(bytes);
    }

    pub(crate) fn take_stdin_frame(&mut self) -> Option<Vec<u8>> {
        self.outbound.pop_front()
    }

    pub(crate) fn is_terminal(&self) -> bool {
        self.phase == Phase::Closed
    }

    pub(crate) fn receive(&mut self, value: &Value) -> Result<Event, ProtocolError> {
        let object = value
            .as_object()
            .ok_or(ProtocolError("frame must be an object"))?;
        // Capacity replies are independent of the model exchange, including a
        // reply drained after the turn closes.
        if !object.contains_key("method")
            && object.get("id").and_then(Value::as_u64) == Some(4)
            && self.rate_limits_read != RateLimitsRead::NotPending
        {
            if object.contains_key("error") == object.contains_key("result") {
                return Err(ProtocolError(
                    "response requires exactly one result or error",
                ));
            }
            let read = std::mem::replace(&mut self.rate_limits_read, RateLimitsRead::NotPending);
            if let Some(error) = object.get("error") {
                let _: RpcError = decode(error)?;
                return Ok(Event::Ignored);
            }
            let response: super::frame::AccountRateLimitsUpdated = decode(&object["result"])?;
            if read == RateLimitsRead::Superseded {
                return Ok(Event::Ignored);
            }
            self.rate_limits.merge(response.rate_limits);
            return Ok(Event::RateLimitsUpdated);
        }
        if self.is_terminal() {
            return Err(ProtocolError("frame follows terminal closure"));
        }
        if let Some(method) = object.get("method") {
            let method = method
                .as_str()
                .ok_or(ProtocolError("method must be a string"))?;
            if let Some(id) = object.get("id") {
                if !id.is_string() && !id.is_i64() && !id.is_u64() {
                    return Err(ProtocolError("request id must be a string or integer"));
                }
                self.queue(
                    json!({"id":id,"error":{"code":-32601,"message":"Method not supported"}}),
                );
                return Ok(Event::Ignored);
            }
            return self.notification(method, object.get("params").unwrap_or(&Value::Null));
        }
        let (id, method) = match self.phase {
            Phase::Initialize => (1, "initialize"),
            Phase::ThreadStart => (2, "thread/start"),
            Phase::TurnStart => (3, "turn/start"),
            Phase::Running | Phase::Closed => return Err(ProtocolError("unexpected response")),
        };
        if object.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(ProtocolError(
                "response id does not match the pending request",
            ));
        }
        if object.contains_key("error") == object.contains_key("result") {
            return Err(ProtocolError(
                "response requires exactly one result or error",
            ));
        }
        if let Some(error) = object.get("error") {
            let error = decode(error)?;
            self.phase = Phase::Closed;
            return Ok(Event::Rejected { method, error });
        }
        let result = &object["result"];
        match self.phase {
            Phase::Initialize => {
                if !result.is_object() {
                    return Err(ProtocolError("initialize result must be an object"));
                }
                self.queue(json!({"method":"initialized"}));
                self.queue(json!({"id":4,"method":"account/rateLimits/read"}));
                self.rate_limits_read = RateLimitsRead::Pending;
                self.queue(json!({"id":2,"method":"thread/start","params":self.thread_params}));
                self.phase = Phase::ThreadStart;
                Ok(Event::Ignored)
            }
            Phase::ThreadStart => {
                let response: ThreadStartResponse = decode(result)?;
                if response.thread.id.is_empty() {
                    return Err(ProtocolError("thread id is empty"));
                }
                self.thread_id = Some(response.thread.id.clone());
                self.turn_params["threadId"] = json!(response.thread.id);
                self.queue(json!({"id":3,"method":"turn/start","params":self.turn_params}));
                self.phase = Phase::TurnStart;
                Ok(Event::ThreadStarted(response.thread.id))
            }
            Phase::TurnStart => {
                let response: TurnStartResponse = decode(result)?;
                self.activity.assistant_output_observed |= response
                    .turn
                    .items
                    .iter()
                    .any(item_precludes_non_acceptance);
                self.check_turn(&response.turn.id)?;
                self.phase = Phase::Running;
                Ok(Event::Ignored)
            }
            Phase::Running | Phase::Closed => Err(ProtocolError("unexpected response")),
        }
    }

    fn check_turn(&mut self, turn: &str) -> Result<(), ProtocolError> {
        if turn.is_empty() || self.turn_id.as_ref().is_some_and(|id| id != turn) {
            return Err(ProtocolError("turn id does not match the active turn"));
        }
        self.turn_id = Some(turn.to_owned());
        Ok(())
    }

    fn correlate(&mut self, thread: &str, turn: &str) -> Result<(), ProtocolError> {
        if !matches!(self.phase, Phase::TurnStart | Phase::Running)
            || self.thread_id.as_deref() != Some(thread)
        {
            return Err(ProtocolError(
                "notification does not match the active thread",
            ));
        }
        self.check_turn(turn)
    }

    fn notification(&mut self, method: &str, params: &Value) -> Result<Event, ProtocolError> {
        match method {
            "account/rateLimits/updated" => {
                let event: super::frame::AccountRateLimitsUpdated = decode(params)?;
                if event.rate_limits.primary.is_none() && event.rate_limits.secondary.is_none() {
                    return Ok(Event::Ignored);
                }
                if self.rate_limits_read == RateLimitsRead::Pending {
                    self.rate_limits_read = RateLimitsRead::Superseded;
                }
                self.rate_limits.merge(event.rate_limits);
                Ok(Event::RateLimitsUpdated)
            }
            "item/agentMessage/delta" => {
                let event: ItemTextDelta = decode(params)?;
                self.correlate(&event.thread_id, &event.turn_id)?;
                self.activity.assistant_output_observed = true;
                Ok(Event::Delta(event))
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                let event: ItemTextDelta = decode(params)?;
                self.correlate(&event.thread_id, &event.turn_id)?;
                self.activity.assistant_output_observed = true;
                Ok(Event::Ignored)
            }
            "item/started" | "item/completed" => {
                let event: ItemNotification = decode(params)?;
                self.correlate(&event.thread_id, &event.turn_id)?;
                self.activity.assistant_output_observed |=
                    item_precludes_non_acceptance(&event.item);
                if event.item.get("type").and_then(Value::as_str) == Some("agentMessage") {
                    let message = decode(&event.item)?;
                    Ok(Event::AgentMessage {
                        message,
                        completed: method == "item/completed",
                    })
                } else {
                    Ok(Event::Ignored)
                }
            }
            "thread/tokenUsage/updated" => {
                let event: TokenUsageUpdated = decode(params)?;
                self.correlate(&event.thread_id, &event.turn_id)?;
                let total = &event.token_usage.total;
                if [
                    total.input_tokens,
                    total.cached_input_tokens,
                    total.cache_write_input_tokens,
                    total.output_tokens,
                    total.reasoning_output_tokens,
                    total.total_tokens,
                ]
                .into_iter()
                .flatten()
                .any(|count| count > 0)
                {
                    self.activity.assistant_output_observed = true;
                }
                Ok(Event::Usage(event))
            }
            "error" => {
                let event: ErrorNotification = decode(params)?;
                self.correlate(&event.thread_id, &event.turn_id)?;
                self.activity.retry_observed |= event.will_retry;
                Ok(Event::Error(event))
            }
            "turn/completed" => {
                let event: TurnCompleted = decode(params)?;
                self.correlate(&event.thread_id, &event.turn.id)?;
                self.activity.assistant_output_observed |=
                    event.turn.items.iter().any(item_precludes_non_acceptance);
                if event.turn.status == TurnStatus::InProgress {
                    return Ok(Event::Ignored);
                }
                self.phase = Phase::Closed;
                Ok(Event::Terminal(event.turn))
            }
            _ => Ok(Event::Ignored),
        }
    }
}

fn item_precludes_non_acceptance(item: &Value) -> bool {
    !matches!(
        item.get("type").and_then(Value::as_str),
        Some("userMessage" | "hookPrompt")
    )
}
