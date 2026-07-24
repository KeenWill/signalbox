//! Bridge from the application-owned model-call port to a Layer-1 runtime.
//!
//! The layer boundary in docs/spec/runtime-substrate.md keeps runtime types
//! out of the domain and application crates. This crate is the outward
//! adapter: it translates one checked application operation, moves the
//! runtime's opaque one-shot capability across durable authorization, and
//! maps typed terminal evidence into the domain dispositions defined in
//! docs/spec/model-call-execution.md. It owns no retry, fallback, lifecycle,
//! or durable state.

use std::{collections::HashMap, error::Error, fmt, future::Future, sync::Arc};

use serde::Deserialize;
use signalbox_application::{
    ClassifyOperatorFailure, ModelCallCapabilityPreparation, ModelCallProvider,
    ModelConversationMessage, ModelToolResultContent, OperatorFailureClass, PreparedModelOperation,
};
use signalbox_domain::{
    AssistantResponsePart, AssistantText, AuthorizedModelCall, ContextFrontierId,
    FrozenModelSelection, ModelCallId, ModelCallTerminalObservation, NormalizedToolArguments,
    ResolvedProviderTarget, SessionId, ToolArgumentsKind,
    ToolCallProposal as DomainToolCallProposal, ToolExecutionErrorKind, ToolName as DomainToolName,
    ToolResultContent, ToolUsingAssistantResponse, TurnAttemptId, TurnId,
};
use signalbox_model_runtime::{
    AssistantPart, CancellationSignal, CompletionFinish, ConversationMessage, ConversationRole,
    CredentialReference, MessagePart, ModelOperation, ModelRuntime, ModelSettings, Observation,
    ObservationFact, ObservationSink, PreparationOutcome, ProviderReportedModel, RequestedTarget,
    ResolvedTarget, TerminalEvidence, ToolCallId, ToolCallProposal, ToolDefinition,
    ToolName as RuntimeToolName, ToolResultRecord, UnsentCause,
};

/// One exact provider-model spelling and baseline request limit for a durable
/// domain target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeModelDefinition {
    target: ResolvedProviderTarget,
    provider_model: String,
    max_output_tokens: u32,
}

impl RuntimeModelDefinition {
    /// Associates a durable target with one exact provider model spelling.
    pub fn try_new(
        target: ResolvedProviderTarget,
        provider_model: String,
        max_output_tokens: u32,
    ) -> Result<Self, RuntimeModelDefinitionError> {
        if provider_model.is_empty() || provider_model.trim() != provider_model {
            return Err(RuntimeModelDefinitionError::InvalidProviderModel);
        }
        if max_output_tokens == 0 {
            return Err(RuntimeModelDefinitionError::InvalidOutputLimit);
        }
        Ok(Self {
            target,
            provider_model,
            max_output_tokens,
        })
    }

    /// Returns the durable exact target represented by this mapping.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.target
    }

    /// Returns the exact provider-native model spelling.
    pub fn provider_model(&self) -> &str {
        &self.provider_model
    }

    /// Returns the required provider output-token ceiling.
    pub const fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }
}

/// A runtime delivery definition cannot construct a request-safe mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeModelDefinitionError {
    /// The provider model spelling was empty or padded.
    InvalidProviderModel,
    /// A provider request requires a positive output-token ceiling.
    InvalidOutputLimit,
}

impl fmt::Display for RuntimeModelDefinitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidProviderModel => "provider model spelling is empty or padded",
            Self::InvalidOutputLimit => "provider output-token limit is zero",
        })
    }
}

impl Error for RuntimeModelDefinitionError {}

/// Immutable runtime delivery mappings indexed by durable exact target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeModelCatalog {
    definitions: HashMap<ResolvedProviderTarget, RuntimeModelDefinition>,
}

impl RuntimeModelCatalog {
    /// Builds a catalog and rejects conflicting meanings for one target.
    pub fn try_from_definitions(
        definitions: impl IntoIterator<Item = RuntimeModelDefinition>,
    ) -> Result<Self, RuntimeModelCatalogError> {
        let mut by_target = HashMap::new();
        for definition in definitions {
            if let Some(existing) = by_target.get(&definition.target)
                && existing != &definition
            {
                return Err(RuntimeModelCatalogError::ConflictingTarget {
                    target: definition.target,
                });
            }
            by_target.insert(definition.target, definition);
        }
        Ok(Self {
            definitions: by_target,
        })
    }

    /// Looks up the exact runtime delivery mapping for a durable target.
    pub fn resolve(&self, target: ResolvedProviderTarget) -> Option<&RuntimeModelDefinition> {
        self.definitions.get(&target)
    }
}

/// Two deployment definitions assigned conflicting meanings to one target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeModelCatalogError {
    /// One target named distinct provider spellings or output limits.
    ConflictingTarget {
        /// The target whose immutable meaning conflicted.
        target: ResolvedProviderTarget,
    },
}

impl fmt::Display for RuntimeModelCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("runtime model catalog contains a conflicting target")
    }
}

impl Error for RuntimeModelCatalogError {}

#[derive(Clone, Copy)]
struct PreparedBinding {
    session: SessionId,
    turn: TurnId,
    attempt: TurnAttemptId,
    call: ModelCallId,
    selection: FrozenModelSelection,
    target: ResolvedProviderTarget,
    frontier: ContextFrontierId,
}

impl PreparedBinding {
    fn matches(&self, authorized: &AuthorizedModelCall) -> bool {
        self.session == authorized.session()
            && self.turn == authorized.turn()
            && self.attempt == authorized.attempt().id()
            && self.call == authorized.call().id()
            && self.selection == authorized.call().selection()
            && self.target == authorized.call().target()
            && self.frontier == authorized.call().frontier().snapshot()
    }
}

/// Opaque runtime capability plus the application facts it was prepared from.
pub struct RuntimeModelCallCapability<Prepared> {
    prepared: Prepared,
    binding: PreparedBinding,
    provider_model: String,
}

/// Sanitized adapter defect; provider response text and credentials are never
/// retained in this error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeModelCallProviderError {
    /// A durably resolved target had no matching runtime mapping.
    UnconfiguredTarget,
    /// Runtime preparation reported a local adapter defect.
    PreparationDefect,
    /// The runtime returned a different caller-owned correlation identity.
    CorrelationMismatch,
    /// Durable authorization did not match the prepared one-shot request.
    AuthorizationMismatch,
    /// A runtime observation did not carry the caller-owned call identity.
    ObservationCorrelationMismatch,
    /// A provider-reported model mismatch cannot yet form durable evidence.
    UnrepresentableProviderTargetMismatch,
    /// Definitive response material is outside the first text-only slice.
    UnsupportedCompletionMaterial,
    /// A runtime text part cannot construct exact domain assistant text.
    InvalidAssistantText,
    /// A checked application schema could not form a runtime JSON value.
    InvalidToolSchema,
    /// Runtime tool material could not form a bounded domain proposal.
    InvalidToolProposal,
}

impl fmt::Display for RuntimeModelCallProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnconfiguredTarget => "resolved model target has no runtime mapping",
            Self::PreparationDefect => "model runtime preparation reported a defect",
            Self::CorrelationMismatch => "model runtime returned a different correlation",
            Self::AuthorizationMismatch => {
                "authorized model call differs from the prepared capability"
            }
            Self::ObservationCorrelationMismatch => {
                "model runtime observation carried a different correlation"
            }
            Self::UnrepresentableProviderTargetMismatch => {
                "provider target mismatch cannot be represented durably"
            }
            Self::UnsupportedCompletionMaterial => {
                "provider completion contains unsupported assistant material"
            }
            Self::InvalidAssistantText => "provider completion contains invalid assistant text",
            Self::InvalidToolSchema => "application tool schema is invalid at the runtime bridge",
            Self::InvalidToolProposal => "provider completion contains an invalid tool proposal",
        })
    }
}

impl Error for RuntimeModelCallProviderError {}

impl ClassifyOperatorFailure for RuntimeModelCallProviderError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
}

/// Application-port adapter over one provider-neutral model runtime.
pub struct RuntimeModelCallProvider<R> {
    runtime: Arc<R>,
    models: RuntimeModelCatalog,
}

struct AcceptanceObservations<AcceptancePossible, Correlation> {
    expected_correlation: Correlation,
    correlation_mismatch: bool,
    acceptance_possible: Option<AcceptancePossible>,
    observations: Vec<Observation<Correlation>>,
}

impl<AcceptancePossible, Correlation> ObservationSink<Correlation>
    for AcceptanceObservations<AcceptancePossible, Correlation>
where
    AcceptancePossible: FnOnce(),
    Correlation: PartialEq,
{
    fn observe(&mut self, observation: Observation<Correlation>) {
        if observation.correlation != self.expected_correlation {
            self.correlation_mismatch = true;
            self.observations.push(observation);
            return;
        }
        if matches!(&observation.fact, ObservationFact::SendCommenced)
            && let Some(acceptance_possible) = self.acceptance_possible.take()
        {
            acceptance_possible();
        }
        self.observations.push(observation);
    }
}

impl<R> RuntimeModelCallProvider<R> {
    /// Supplies the runtime and immutable target mapping.
    pub fn new(runtime: R, models: RuntimeModelCatalog) -> Self {
        Self {
            runtime: Arc::new(runtime),
            models,
        }
    }
}

impl<R> Clone for RuntimeModelCallProvider<R> {
    fn clone(&self) -> Self {
        Self {
            runtime: Arc::clone(&self.runtime),
            models: self.models.clone(),
        }
    }
}

impl<R> fmt::Debug for RuntimeModelCallProvider<R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeModelCallProvider")
            .field("runtime", &"[provider runtime]")
            .field("models", &self.models)
            .finish()
    }
}

impl<R> ModelCallProvider for RuntimeModelCallProvider<R>
where
    R: ModelRuntime<ModelCallId> + Send + Sync,
{
    type Capability = RuntimeModelCallCapability<R::Prepared>;
    type Error = RuntimeModelCallProviderError;

    async fn prepare_capability<Cancellation>(
        &mut self,
        operation: PreparedModelOperation,
        cancellation: Cancellation,
    ) -> Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>
    where
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        let request = operation.request();
        let call = request.call();
        let credential =
            CredentialReference::new(operation.credential_reference().as_str().to_owned());
        let definition = self
            .models
            .resolve(call.target())
            .ok_or(RuntimeModelCallProviderError::UnconfiguredTarget)?;
        let correlation = call.id();
        let binding = PreparedBinding {
            session: request.session(),
            turn: request.turn(),
            attempt: request.attempt(),
            call: correlation,
            selection: call.selection(),
            target: call.target(),
            frontier: call.frontier().snapshot(),
        };
        let messages = render_runtime_messages(operation.messages());
        let tools = operation
            .tools()
            .iter()
            .map(|definition| {
                let schema = decode_checked_json(definition.input_schema().as_str())
                    .map_err(|_| RuntimeModelCallProviderError::InvalidToolSchema)?;
                Ok(ToolDefinition::with_schema(
                    definition.name().as_str(),
                    definition.description(),
                    schema,
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut runtime_operation = ModelOperation::new(
            correlation,
            credential,
            RequestedTarget::new(render_requested_target(call.selection())),
            ResolvedTarget::new(definition.provider_model().to_owned()),
            messages,
            ModelSettings::new(definition.max_output_tokens()),
        );
        runtime_operation.tools = tools;
        let provider_model = definition.provider_model().to_owned();
        match self
            .runtime
            .prepare(runtime_operation, CancellationSignal::when(cancellation))
            .await
        {
            PreparationOutcome::Prepared(prepared) => Ok(ModelCallCapabilityPreparation::Ready(
                RuntimeModelCallCapability {
                    prepared,
                    binding,
                    provider_model,
                },
            )),
            PreparationOutcome::Cancelled {
                correlation: returned,
            } => {
                require_correlation(correlation, returned)?;
                Ok(ModelCallCapabilityPreparation::Cancelled)
            }
            PreparationOutcome::Failed {
                correlation: returned,
                ..
            } => {
                require_correlation(correlation, returned)?;
                Ok(ModelCallCapabilityPreparation::KnownFailure)
            }
            PreparationOutcome::Defect {
                correlation: returned,
                ..
            } => {
                require_correlation(correlation, returned)?;
                Err(RuntimeModelCallProviderError::PreparationDefect)
            }
        }
    }

    async fn invoke<AcceptancePossible, Cancellation>(
        &mut self,
        authorized: AuthorizedModelCall,
        capability: Self::Capability,
        acceptance_possible: AcceptancePossible,
        cancellation: Cancellation,
    ) -> Result<signalbox_domain::CorrelatedModelCallTerminalObservation, Self::Error>
    where
        AcceptancePossible: FnOnce() + Send,
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        if !capability.binding.matches(&authorized) {
            return Err(RuntimeModelCallProviderError::AuthorizationMismatch);
        }
        let correlation = authorized.call().id();
        let mut observations = AcceptanceObservations {
            expected_correlation: correlation,
            correlation_mismatch: false,
            acceptance_possible: Some(acceptance_possible),
            observations: Vec::new(),
        };
        let report = self
            .runtime
            .execute(
                capability.prepared,
                &mut observations,
                CancellationSignal::when(cancellation),
            )
            .await;
        require_correlation(correlation, report.correlation)?;
        if observations.correlation_mismatch {
            return Err(RuntimeModelCallProviderError::ObservationCorrelationMismatch);
        }
        let observation = classify_terminal(
            report.evidence,
            &observations.observations,
            capability.provider_model.as_str(),
        )?;
        Ok(authorized
            .observation_correlation()
            .bind_terminal_observation(observation))
    }
}

fn render_runtime_messages(messages: &[ModelConversationMessage]) -> Vec<ConversationMessage> {
    let mut rendered = Vec::new();
    let mut assistant_call = None;
    let mut collecting_tool_results = false;
    for message in messages {
        match message {
            ModelConversationMessage::User { content, .. } => {
                rendered.push(ConversationMessage::user_text(content.text().as_str()));
                assistant_call = None;
                collecting_tool_results = false;
            }
            ModelConversationMessage::Assistant {
                producing_call,
                content,
                ..
            } => {
                if assistant_call == Some(*producing_call) {
                    if let Some(message) = rendered.last_mut() {
                        message
                            .parts
                            .push(MessagePart::Text(content.as_str().to_owned()));
                    } else {
                        rendered.push(ConversationMessage::assistant_text(content.as_str()));
                    }
                } else {
                    rendered.push(ConversationMessage::assistant_text(content.as_str()));
                    assistant_call = Some(*producing_call);
                }
                collecting_tool_results = false;
            }
            ModelConversationMessage::AssistantToolUse {
                producing_call,
                request,
                ..
            } => {
                let part = MessagePart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new(request.id().into_uuid().to_string()),
                    name: RuntimeToolName::new(request.name().as_str()),
                    arguments_json: replay_safe_arguments(request),
                });
                if assistant_call == Some(*producing_call) {
                    if let Some(message) = rendered.last_mut() {
                        message.parts.push(part);
                    } else {
                        rendered.push(ConversationMessage {
                            role: ConversationRole::Assistant,
                            parts: vec![part],
                        });
                    }
                } else {
                    rendered.push(ConversationMessage {
                        role: ConversationRole::Assistant,
                        parts: vec![part],
                    });
                    assistant_call = Some(*producing_call);
                }
                collecting_tool_results = false;
            }
            ModelConversationMessage::ToolResult {
                request, content, ..
            } => {
                let (content, is_error) = render_tool_result(content);
                let part = MessagePart::ToolResult(ToolResultRecord {
                    tool_call_id: ToolCallId::new(request.into_uuid().to_string()),
                    content,
                    is_error,
                });
                if collecting_tool_results {
                    if let Some(message) = rendered.last_mut() {
                        message.parts.push(part);
                    } else {
                        rendered.push(ConversationMessage {
                            role: ConversationRole::User,
                            parts: vec![part],
                        });
                    }
                } else {
                    rendered.push(ConversationMessage {
                        role: ConversationRole::User,
                        parts: vec![part],
                    });
                }
                assistant_call = None;
                collecting_tool_results = true;
            }
        }
    }
    rendered
}

fn replay_safe_arguments(request: &signalbox_domain::ToolRequest) -> String {
    if request.arguments().kind() == ToolArgumentsKind::Json
        && decode_checked_json(request.arguments().as_str()).is_ok_and(|value| value.is_object())
    {
        request.arguments().as_str().to_owned()
    } else {
        // Exact bytes remain durable authority. Replayed function arguments
        // must be an object even when the provider originally supplied a
        // scalar, array, or undecodable value.
        String::from(r#"{"signalbox_invalid_arguments":true}"#)
    }
}

fn decode_checked_json(value: &str) -> Result<serde_json::Value, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(value);
    deserializer.disable_recursion_limit();
    let decoded = {
        let deserializer = serde_stacker::Deserializer::new(&mut deserializer);
        serde_json::Value::deserialize(deserializer)?
    };
    deserializer.end()?;
    Ok(decoded)
}

fn require_correlation(
    expected: ModelCallId,
    returned: ModelCallId,
) -> Result<(), RuntimeModelCallProviderError> {
    if expected == returned {
        Ok(())
    } else {
        Err(RuntimeModelCallProviderError::CorrelationMismatch)
    }
}

fn render_requested_target(selection: FrozenModelSelection) -> String {
    match selection {
        FrozenModelSelection::Direct(direct) => format!("direct:{}", direct.into_uuid()),
        FrozenModelSelection::FrozenAlias { alias, definition } => format!(
            "alias:{}@direct:{}",
            alias.into_uuid(),
            definition.selected().into_uuid()
        ),
    }
}

fn render_tool_result(content: &ModelToolResultContent) -> (String, bool) {
    match content {
        ModelToolResultContent::Success(ToolResultContent::Text(text)) => {
            (text.as_str().to_owned(), false)
        }
        ModelToolResultContent::ExecutionError(error) => {
            let kind = match error.kind() {
                ToolExecutionErrorKind::UnknownTool => "unknown_tool",
                ToolExecutionErrorKind::InvalidArguments => "invalid_arguments",
                ToolExecutionErrorKind::ExecutionFailed => "execution_failed",
                ToolExecutionErrorKind::ResultTooLarge => "result_too_large",
                ToolExecutionErrorKind::CrashLost => "crash_lost",
            };
            (
                serde_json::json!({
                    "error": {
                        "kind": kind,
                        "detail": error.detail().map(signalbox_domain::ToolExecutionErrorDetail::as_str),
                    }
                })
                .to_string(),
                true,
            )
        }
        ModelToolResultContent::Denied { reason } => (
            serde_json::json!({
                "error": {
                    "kind": "denied",
                    "detail": reason.as_ref().map(signalbox_domain::ToolDenialReason::as_str),
                }
            })
            .to_string(),
            true,
        ),
        ModelToolResultContent::ClosedByTurnEnd => (
            serde_json::json!({
                "error": {
                    "kind": "closed_by_turn_end",
                    "detail": null,
                }
            })
            .to_string(),
            true,
        ),
    }
}

fn classify_terminal(
    evidence: TerminalEvidence,
    observations: &[Observation<ModelCallId>],
    expected_provider_model: &str,
) -> Result<ModelCallTerminalObservation, RuntimeModelCallProviderError> {
    if observations.iter().any(|observation| {
        matches!(
            &observation.fact,
            ObservationFact::ProviderModelReported(reported)
                if reported.as_str() != expected_provider_model
        )
    }) || reported_model(&evidence)
        .is_some_and(|reported| reported.as_str() != expected_provider_model)
    {
        // docs/spec/model-call-execution.md forbids collapsing mismatch
        // evidence into an ordinary provider failure. Provider identity
        // normalization and its durable provenance schema remain an
        // owner-gated open question, so fail the adapter stage closed
        // instead of committing the wrong lifecycle.
        return Err(RuntimeModelCallProviderError::UnrepresentableProviderTargetMismatch);
    }

    match evidence {
        TerminalEvidence::Completed(completion) => {
            let finish = completion.finish;
            let mut response_parts = Vec::new();
            let mut tool_count = 0usize;
            for part in completion.content {
                match part {
                    AssistantPart::Text(text) if text.is_empty() => {}
                    AssistantPart::Text(text) => {
                        response_parts.push(AssistantResponsePart::Text(
                            AssistantText::try_new(text)
                                .map_err(|_| RuntimeModelCallProviderError::InvalidAssistantText)?,
                        ));
                    }
                    AssistantPart::ToolCall(proposal) => {
                        tool_count += 1;
                        let Ok(name) = DomainToolName::try_new(proposal.name.as_str().to_owned())
                        else {
                            return Ok(ModelCallTerminalObservation::KnownFailed);
                        };
                        let Ok(arguments) = NormalizedToolArguments::try_from_provider_text(
                            proposal.arguments_json,
                        ) else {
                            return Ok(ModelCallTerminalObservation::KnownFailed);
                        };
                        response_parts.push(AssistantResponsePart::ToolCall(
                            DomainToolCallProposal::new(name, arguments),
                        ));
                    }
                    AssistantPart::Thinking { .. } | AssistantPart::RedactedThinking { .. } => {
                        return Err(RuntimeModelCallProviderError::UnsupportedCompletionMaterial);
                    }
                }
            }
            if tool_count == 0 {
                if matches!(finish, CompletionFinish::ToolUse) {
                    return Ok(ModelCallTerminalObservation::KnownFailed);
                }
                Ok(ModelCallTerminalObservation::Completed {
                    assistant_text: response_parts
                        .into_iter()
                        .map(|part| match part {
                            AssistantResponsePart::Text(text) => text,
                            AssistantResponsePart::ToolCall(_) => {
                                unreachable!("zero tool count excludes tool parts")
                            }
                        })
                        .collect(),
                })
            } else {
                if !matches!(finish, CompletionFinish::ToolUse) {
                    return Ok(ModelCallTerminalObservation::KnownFailed);
                }
                let Ok(response) = ToolUsingAssistantResponse::try_from_parts(response_parts)
                else {
                    return Ok(ModelCallTerminalObservation::KnownFailed);
                };
                Ok(ModelCallTerminalObservation::CompletedWithTools { response })
            }
        }
        TerminalEvidence::Refused(_) => Ok(ModelCallTerminalObservation::Refused),
        TerminalEvidence::ProviderError(_)
        | TerminalEvidence::ProvenUnsent(signalbox_model_runtime::ProvenUnsentEvidence {
            cause: UnsentCause::ConnectFailed(_) | UnsentCause::SendIncompleteProvenUnacceptable(_),
        }) => Ok(ModelCallTerminalObservation::KnownFailed),
        TerminalEvidence::ProvenUnsent(signalbox_model_runtime::ProvenUnsentEvidence {
            cause: UnsentCause::CancelledBeforeSend,
        }) => Ok(ModelCallTerminalObservation::Cancelled),
        TerminalEvidence::CancellationConfirmed(_) => Ok(ModelCallTerminalObservation::Cancelled),
        TerminalEvidence::BoundaryLoss(_) => Ok(ModelCallTerminalObservation::Ambiguous),
    }
}

fn reported_model(evidence: &TerminalEvidence) -> Option<&ProviderReportedModel> {
    match evidence {
        TerminalEvidence::Completed(value) => value.reported_model.as_ref(),
        TerminalEvidence::Refused(value) => value.reported_model.as_ref(),
        TerminalEvidence::ProviderError(value) => value.reported_model.as_ref(),
        TerminalEvidence::CancellationConfirmed(value) => value.reported_model.as_ref(),
        TerminalEvidence::BoundaryLoss(value) => value.reported_model.as_ref(),
        TerminalEvidence::ProvenUnsent(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use signalbox_domain::{
        AssistantText, ModelCallId, ModelCallTerminalObservation, NormalizedToolArguments,
        ProviderModelIdentity, SemanticTranscriptEntryId, SemanticTranscriptEntryRef, SessionId,
        ToolExecutionError, ToolExecutionErrorKind, ToolRequest, ToolRequestId, ToolRequestOrdinal,
        ToolRequestReconstitutionInput, TurnId,
    };
    use signalbox_model_runtime::{
        AssistantPart, BoundaryLossEvidence, CancellationConfirmedEvidence, CompletionEvidence,
        CompletionFinish, ExchangeFacts, LossCause, NativeErrorFacts, Observation, ObservationFact,
        ObservationSink, ProvenUnsentEvidence, ProviderErrorEvidence, ProviderErrorKind,
        ProviderReportedModel, RefusalEvidence, TerminalEvidence, TokenUsage, ToolCallId,
        ToolCallProposal, ToolName, TransportFacts, UnsentCause,
    };
    use uuid::Uuid;

    use super::{
        AcceptanceObservations, RuntimeModelCatalog, RuntimeModelCatalogError,
        RuntimeModelDefinition, classify_terminal, decode_checked_json, render_runtime_messages,
    };
    use signalbox_domain::ResolvedProviderTarget;

    fn call() -> ModelCallId {
        ModelCallId::from_uuid(Uuid::from_u128(1))
    }

    fn target(value: u128) -> ResolvedProviderTarget {
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(value)))
    }

    fn source(value: u128) -> SemanticTranscriptEntryRef {
        SemanticTranscriptEntryRef::from_source(
            SessionId::from_uuid(Uuid::from_u128(10)),
            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value)),
        )
    }

    fn request(value: u128, arguments: &str) -> ToolRequest {
        ToolRequestReconstitutionInput::new(
            ToolRequestId::from_uuid(Uuid::from_u128(value)),
            SessionId::from_uuid(Uuid::from_u128(10)),
            TurnId::from_uuid(Uuid::from_u128(11)),
            call(),
            ToolRequestOrdinal::from_u32(u32::try_from(value - 20).expect("fixture ordinal")),
            signalbox_domain::ToolName::try_new(String::from("current_time"))
                .expect("fixture name"),
            NormalizedToolArguments::try_from_provider_text(arguments.to_owned())
                .expect("fixture arguments"),
        )
        .into_request()
    }

    fn completion(model: &str, content: Vec<AssistantPart>) -> TerminalEvidence {
        TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new(model)),
            finish: CompletionFinish::EndTurn,
            content,
            usage: TokenUsage::unreported(),
        })
    }

    fn tool_completion(model: &str) -> TerminalEvidence {
        TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new(model)),
            finish: CompletionFinish::ToolUse,
            content: vec![
                AssistantPart::Text(String::from("checking")),
                AssistantPart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new("provider-call-opaque"),
                    name: ToolName::new("current_time"),
                    arguments_json: String::from(r#"{ "timezone": "UTC" }"#),
                }),
            ],
            usage: TokenUsage::unreported(),
        })
    }

    /// S10 / INV-002 / INV-005: one provider response and its ordered result
    /// batch remain grouped, while malformed arguments use replay-safe JSON
    /// without replacing their exact durable request evidence.
    #[test]
    fn s10_inv002_inv005_tool_history_is_grouped_and_replay_safe() {
        let first = request(20, "{}");
        let malformed = request(21, "{\"timezone\":");
        let scalar = request(22, "7");
        let messages = [
            signalbox_application::ModelConversationMessage::Assistant {
                source: source(30),
                producing_call: call(),
                content: AssistantText::try_new(String::from("before")).expect("fixture text"),
            },
            signalbox_application::ModelConversationMessage::AssistantToolUse {
                source: source(31),
                producing_call: call(),
                request: first.clone(),
            },
            signalbox_application::ModelConversationMessage::AssistantToolUse {
                source: source(32),
                producing_call: call(),
                request: malformed.clone(),
            },
            signalbox_application::ModelConversationMessage::Assistant {
                source: source(33),
                producing_call: call(),
                content: AssistantText::try_new(String::from("after")).expect("fixture text"),
            },
            signalbox_application::ModelConversationMessage::AssistantToolUse {
                source: source(34),
                producing_call: call(),
                request: scalar.clone(),
            },
            signalbox_application::ModelConversationMessage::Assistant {
                source: source(35),
                producing_call: call(),
                content: AssistantText::try_new(String::from("after")).expect("fixture text"),
            },
            signalbox_application::ModelConversationMessage::ToolResult {
                source: source(36),
                request: first.id(),
                content: signalbox_application::ModelToolResultContent::ExecutionError(
                    ToolExecutionError::new(ToolExecutionErrorKind::ExecutionFailed, None),
                ),
            },
            signalbox_application::ModelConversationMessage::ToolResult {
                source: source(37),
                request: malformed.id(),
                content: signalbox_application::ModelToolResultContent::ExecutionError(
                    ToolExecutionError::new(ToolExecutionErrorKind::InvalidArguments, None),
                ),
            },
            signalbox_application::ModelConversationMessage::ToolResult {
                source: source(38),
                request: scalar.id(),
                content: signalbox_application::ModelToolResultContent::ExecutionError(
                    ToolExecutionError::new(ToolExecutionErrorKind::InvalidArguments, None),
                ),
            },
        ];

        let rendered = render_runtime_messages(&messages);
        assert_eq!(rendered.len(), 2);
        assert_eq!(
            rendered[0].role,
            signalbox_model_runtime::ConversationRole::Assistant
        );
        assert_eq!(rendered[0].parts.len(), 6);
        for part in [&rendered[0].parts[2], &rendered[0].parts[4]] {
            let signalbox_model_runtime::MessagePart::ToolCall(replayed) = part else {
                panic!("invalid proposal remains in the assistant group");
            };
            assert_eq!(
                replayed.arguments_json,
                r#"{"signalbox_invalid_arguments":true}"#
            );
        }
        assert_eq!(malformed.arguments().as_str(), "{\"timezone\":");
        assert_eq!(scalar.arguments().as_str(), "7");
        assert_eq!(
            rendered[1].role,
            signalbox_model_runtime::ConversationRole::User
        );
        assert_eq!(rendered[1].parts.len(), 3);
    }

    /// INV-026: the application-owned dispatch permit is released exactly
    /// when the runtime first reports that provider acceptance is possible.
    #[test]
    fn inv026_send_commenced_releases_acceptance_callback_once() {
        let release_count = Arc::new(AtomicUsize::new(0));
        let callback_count = Arc::clone(&release_count);
        let mut sink = AcceptanceObservations {
            expected_correlation: call(),
            correlation_mismatch: false,
            acceptance_possible: Some(move || {
                callback_count.fetch_add(1, Ordering::SeqCst);
            }),
            observations: Vec::new(),
        };

        sink.observe(Observation {
            correlation: call(),
            fact: ObservationFact::ProviderModelReported(ProviderReportedModel::new("model-exact")),
        });
        assert_eq!(release_count.load(Ordering::SeqCst), 0);

        sink.observe(Observation {
            correlation: call(),
            fact: ObservationFact::SendCommenced,
        });
        sink.observe(Observation {
            correlation: call(),
            fact: ObservationFact::SendCommenced,
        });

        assert_eq!(release_count.load(Ordering::SeqCst), 1);
        assert_eq!(sink.observations.len(), 3);
    }

    /// INV-026: cross-wired acceptance evidence cannot release another
    /// attempt's dispatch/stop gate.
    #[test]
    fn inv026_cross_wired_send_commenced_retains_acceptance_callback() {
        let release_count = Arc::new(AtomicUsize::new(0));
        let callback_count = Arc::clone(&release_count);
        let mut sink = AcceptanceObservations {
            expected_correlation: call(),
            correlation_mismatch: false,
            acceptance_possible: Some(move || {
                callback_count.fetch_add(1, Ordering::SeqCst);
            }),
            observations: Vec::new(),
        };

        sink.observe(Observation {
            correlation: ModelCallId::from_uuid(Uuid::from_u128(2)),
            fact: ObservationFact::SendCommenced,
        });

        assert_eq!(release_count.load(Ordering::SeqCst), 0);
        assert!(sink.correlation_mismatch);
        assert!(sink.acceptance_possible.is_some());
    }

    /// S02 / INV-014 / INV-025: runtime terminal evidence maps to the exact
    /// physical disposition without retryability or error-string inference.
    #[test]
    fn s02_inv014_inv025_terminal_evidence_classification_is_total() {
        let exchange = ExchangeFacts::default();
        assert_eq!(
            classify_terminal(
                TerminalEvidence::Refused(RefusalEvidence {
                    exchange: exchange.clone(),
                    message_id: None,
                    reported_model: None,
                    content: Vec::new(),
                    usage: TokenUsage::unreported(),
                }),
                &[],
                "model-exact",
            )
            .expect("typed refusal evidence is supported"),
            ModelCallTerminalObservation::Refused
        );
        assert_eq!(
            classify_terminal(
                TerminalEvidence::ProviderError(ProviderErrorEvidence {
                    exchange: exchange.clone(),
                    reported_model: None,
                    kind: ProviderErrorKind::RateLimited,
                    native: NativeErrorFacts::default(),
                    usage: TokenUsage::unreported(),
                }),
                &[],
                "model-exact",
            )
            .expect("typed provider-error evidence is supported"),
            ModelCallTerminalObservation::KnownFailed
        );
        assert_eq!(
            classify_terminal(
                TerminalEvidence::CancellationConfirmed(CancellationConfirmedEvidence {
                    exchange: exchange.clone(),
                    reported_model: None,
                    native: NativeErrorFacts::default(),
                }),
                &[],
                "model-exact",
            )
            .expect("typed cancellation evidence is supported"),
            ModelCallTerminalObservation::Cancelled
        );
        assert_eq!(
            classify_terminal(
                TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                    cause: UnsentCause::ConnectFailed(TransportFacts {
                        detail: String::from("safe typed fixture"),
                    }),
                }),
                &[],
                "model-exact",
            )
            .expect("typed non-acceptance evidence is supported"),
            ModelCallTerminalObservation::KnownFailed
        );
        assert_eq!(
            classify_terminal(
                TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                    cause: UnsentCause::CancelledBeforeSend,
                }),
                &[],
                "model-exact",
            )
            .expect("pre-send cancellation evidence is supported"),
            ModelCallTerminalObservation::Cancelled
        );
        assert_eq!(
            classify_terminal(
                TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                    cause: LossCause::TransportFailed(TransportFacts {
                        detail: String::from("safe typed fixture"),
                    }),
                    exchange,
                    reported_model: None,
                    finish_reported: None,
                    usage: TokenUsage::unreported(),
                }),
                &[],
                "model-exact",
            )
            .expect("typed boundary-loss evidence is supported"),
            ModelCallTerminalObservation::Ambiguous
        );
    }

    /// S02 / INV-014: only exact text from a matching reported target becomes
    /// assistant content; empty blocks create no invalid empty entry.
    #[test]
    fn s02_inv014_matching_completion_preserves_text_parts() {
        assert_eq!(
            classify_terminal(
                completion(
                    "model-exact",
                    vec![
                        AssistantPart::Text(String::from("first")),
                        AssistantPart::Text(String::new()),
                        AssistantPart::Text(String::from("second")),
                    ],
                ),
                &[],
                "model-exact",
            )
            .expect("text-only completion is supported"),
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    signalbox_domain::AssistantText::try_new(String::from("first"))
                        .expect("fixture text is admitted"),
                    signalbox_domain::AssistantText::try_new(String::from("second"))
                        .expect("fixture text is admitted"),
                ],
            }
        );
    }

    /// S10 / INV-002 / INV-005: runtime-native tool calls become ordered,
    /// normalized domain proposals without retaining provider identifiers.
    #[test]
    fn s10_inv002_inv005_tool_completion_crosses_as_provider_neutral_proposals() {
        let observation = classify_terminal(tool_completion("model-exact"), &[], "model-exact")
            .expect("tool-use completion is supported");
        let ModelCallTerminalObservation::CompletedWithTools { response } = observation else {
            panic!("tool-use finish produces a same-turn tool round");
        };
        assert_eq!(response.parts().len(), 2);
        assert!(matches!(
            &response.parts()[0],
            signalbox_domain::AssistantResponsePart::Text(text)
                if text.as_str() == "checking"
        ));
        assert!(matches!(
            &response.parts()[1],
            signalbox_domain::AssistantResponsePart::ToolCall(proposal)
                if proposal.name().as_str() == "current_time"
                    && proposal.arguments().as_str() == r#"{"timezone":"UTC"}"#
        ));
    }

    #[test]
    fn checked_tool_json_decoding_is_stack_guarded_beyond_serde_default_depth() {
        let depth = 256;
        let json = format!(r#"{{"nested":{}{}}}"#, "[".repeat(depth), "]".repeat(depth));

        assert!(
            decode_checked_json(&json)
                .expect("checked bounded JSON remains decodable")
                .is_object()
        );
    }

    /// INV-014: malformed tool proposals are terminal known failures, so the
    /// provider operation cannot remain durably in flight.
    #[test]
    fn inv014_invalid_tool_proposals_close_as_known_failure() {
        let invalid_name = TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("model-exact")),
            finish: CompletionFinish::ToolUse,
            content: vec![AssistantPart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("provider-call-opaque"),
                name: ToolName::new("bad name"),
                arguments_json: String::from("{}"),
            })],
            usage: TokenUsage::unreported(),
        });
        let oversized_arguments = TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("model-exact")),
            finish: CompletionFinish::ToolUse,
            content: vec![AssistantPart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("provider-call-opaque"),
                name: ToolName::new("current_time"),
                arguments_json: "x".repeat(1024 * 1024 + 1),
            })],
            usage: TokenUsage::unreported(),
        });
        let mismatched_finish = TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("model-exact")),
            finish: CompletionFinish::EndTurn,
            content: vec![AssistantPart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("provider-call-opaque"),
                name: ToolName::new("current_time"),
                arguments_json: String::from("{}"),
            })],
            usage: TokenUsage::unreported(),
        });

        for evidence in [invalid_name, oversized_arguments, mismatched_finish] {
            assert_eq!(
                classify_terminal(evidence, &[], "model-exact")
                    .expect("invalid proposal has a durable terminal classification"),
                ModelCallTerminalObservation::KnownFailed
            );
        }
    }

    /// INV-014: either early or terminal provider-model mismatch prevents
    /// response material from becoming authoritative.
    #[test]
    fn inv014_reported_target_mismatch_precedes_completion() {
        let early = vec![Observation {
            correlation: call(),
            fact: ObservationFact::ProviderModelReported(ProviderReportedModel::new("other")),
        }];
        assert_eq!(
            classify_terminal(
                completion(
                    "model-exact",
                    vec![AssistantPart::Text(String::from("hidden"))]
                ),
                &early,
                "model-exact",
            )
            .expect_err("unrepresentable mismatch must fail closed"),
            super::RuntimeModelCallProviderError::UnrepresentableProviderTargetMismatch
        );
        assert_eq!(
            classify_terminal(
                completion("other", vec![AssistantPart::Text(String::from("hidden"))]),
                &[],
                "model-exact",
            )
            .expect_err("unrepresentable mismatch must fail closed"),
            super::RuntimeModelCallProviderError::UnrepresentableProviderTargetMismatch
        );
    }

    #[test]
    fn conflicting_runtime_target_meaning_is_rejected() {
        assert_eq!(
            RuntimeModelCatalog::try_from_definitions([
                RuntimeModelDefinition::try_new(target(1), String::from("first"), 64)
                    .expect("fixture definition is valid"),
                RuntimeModelDefinition::try_new(target(1), String::from("second"), 64)
                    .expect("fixture definition is valid"),
            ]),
            Err(RuntimeModelCatalogError::ConflictingTarget { target: target(1) })
        );
    }
}
