//! Application-owned transaction shapes implemented by durable tool storage.
//!
//! These ports keep storage adapters dependent on application contracts while
//! leaving tool selection, execution, and retry orchestration in the
//! application layer.

use std::{collections::BTreeMap, fmt, future::Future, sync::Arc};

use signalbox_domain::{
    AcceptedInputId, AuthorizedToolAttempt, CorrelatedToolAttemptObservation, CurrentToolAttempt,
    DecideToolRequest, EndedToolAttempt, FailedModelCallTurn, FailedModelCallTurnIdentities,
    ModelCallId, NormalizedToolArguments, PreparedDecideToolRequest, SemanticTranscriptEntryId,
    SemanticTranscriptEntryRef, SessionId, ToolApprovalResolution, ToolArgumentsKind,
    ToolAttemptCrashOutcome, ToolAttemptId, ToolBatch, ToolEffectClass, ToolExecutionError,
    ToolExecutionErrorDetail, ToolName, ToolPermissionDefault, ToolRequest, TurnAttemptId, TurnId,
};

use crate::ClassifyOperatorFailure;

/// Canonical JSON object used as a model-facing argument schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolInputSchema(String);

impl ToolInputSchema {
    /// Normalizes and checks one provider-neutral JSON Schema object.
    pub fn try_new(value: String) -> Result<Self, ToolInputSchemaError> {
        let normalized =
            NormalizedToolArguments::try_from_provider_text(value.clone()).map_err(|error| {
                ToolInputSchemaError {
                    value: value.clone(),
                    failure: ToolInputSchemaFailure::OutsideArgumentBound(error.failure()),
                }
            })?;
        if normalized.kind() != ToolArgumentsKind::Json {
            return Err(ToolInputSchemaError {
                value,
                failure: ToolInputSchemaFailure::NotJson,
            });
        }
        if !normalized.as_str().starts_with('{') {
            return Err(ToolInputSchemaError {
                value,
                failure: ToolInputSchemaFailure::NotObject,
            });
        }
        Ok(Self(normalized.into_parts().1))
    }

    /// Borrows the compact canonical schema text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a tool schema was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolInputSchemaFailure {
    /// The text did not decode as JSON.
    NotJson,
    /// Tool arguments require an object-shaped schema.
    NotObject,
    /// The schema exceeded the domain argument bound or could not normalize.
    OutsideArgumentBound(signalbox_domain::ToolArgumentsFailure),
}

/// Failed schema construction retaining the exact rejected text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolInputSchemaError {
    value: String,
    failure: ToolInputSchemaFailure,
}

impl ToolInputSchemaError {
    /// Borrows the rejected schema.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the exact validation failure.
    pub const fn failure(&self) -> ToolInputSchemaFailure {
        self.failure
    }

    /// Returns the rejected schema and failure.
    pub fn into_parts(self) -> (String, ToolInputSchemaFailure) {
        (self.value, self.failure)
    }
}

/// Immutable model-facing and execution-risk metadata for one tool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolDefinition {
    name: ToolName,
    description: String,
    input_schema: ToolInputSchema,
    permission_default: ToolPermissionDefault,
    effect_class: ToolEffectClass,
}

impl ToolDefinition {
    /// Declares one complete provider-neutral tool definition.
    pub const fn new(
        name: ToolName,
        description: String,
        input_schema: ToolInputSchema,
        permission_default: ToolPermissionDefault,
        effect_class: ToolEffectClass,
    ) -> Self {
        Self {
            name,
            description,
            input_schema,
            permission_default,
            effect_class,
        }
    }

    /// Borrows the checked model-facing name.
    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    /// Borrows the model-facing description.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Borrows the canonical argument schema.
    pub const fn input_schema(&self) -> &ToolInputSchema {
        &self.input_schema
    }

    /// Returns the registry approval default.
    pub const fn permission_default(&self) -> ToolPermissionDefault {
        self.permission_default
    }

    /// Returns the crash-relevant effect class.
    pub const fn effect_class(&self) -> ToolEffectClass {
        self.effect_class
    }
}

/// Argument validation associated with one immutable catalog declaration.
pub trait ToolArgumentValidator: Send + Sync {
    /// Checks exact normalized JSON against the declaration's argument type.
    fn validate(&self, arguments: &NormalizedToolArguments)
    -> Result<(), ToolExecutionErrorDetail>;
}

impl<Validate> ToolArgumentValidator for Validate
where
    Validate: Fn(&NormalizedToolArguments) -> Result<(), ToolExecutionErrorDetail> + Send + Sync,
{
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        self(arguments)
    }
}

/// One compiled declaration plus its non-effecting argument validator.
#[derive(Clone)]
pub struct CompiledTool {
    definition: ToolDefinition,
    validator: Arc<dyn ToolArgumentValidator>,
}

impl fmt::Debug for CompiledTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompiledTool")
            .field("definition", &self.definition)
            .finish_non_exhaustive()
    }
}

impl CompiledTool {
    /// Binds immutable metadata to a pure argument validator.
    pub fn new(
        definition: ToolDefinition,
        validator: impl ToolArgumentValidator + 'static,
    ) -> Self {
        Self {
            definition,
            validator: Arc::new(validator),
        }
    }

    /// Borrows immutable declaration metadata.
    pub const fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
}

/// Catalog construction rejected duplicate declarations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateToolDefinition {
    name: ToolName,
}

impl DuplicateToolDefinition {
    /// Borrows the duplicated checked name.
    pub const fn name(&self) -> &ToolName {
        &self.name
    }
}

/// Immutable compiled catalog used by the first hub composition.
#[derive(Clone, Debug, Default)]
pub struct CompiledToolCatalog {
    tools: BTreeMap<ToolName, CompiledTool>,
}

impl CompiledToolCatalog {
    /// Constructs one stable catalog and rejects duplicate names.
    pub fn try_new(
        tools: impl IntoIterator<Item = CompiledTool>,
    ) -> Result<Self, DuplicateToolDefinition> {
        let mut by_name = BTreeMap::new();
        for tool in tools {
            let name = tool.definition.name.clone();
            if by_name.insert(name.clone(), tool).is_some() {
                return Err(DuplicateToolDefinition { name });
            }
        }
        Ok(Self { tools: by_name })
    }
}

/// Provider-neutral registry port.
pub trait ToolCatalog: Send + Sync {
    /// Returns one stable definition snapshot in deterministic order.
    fn definitions(&self) -> Box<[ToolDefinition]>;

    /// Resolves current immutable metadata for one exact name.
    fn definition(&self, name: &ToolName) -> Option<ToolDefinition>;

    /// Validates exact normalized arguments without performing the tool effect.
    fn validate_arguments(
        &self,
        name: &ToolName,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolCatalogValidationFailure>;
}

impl ToolCatalog for CompiledToolCatalog {
    fn definitions(&self) -> Box<[ToolDefinition]> {
        self.tools
            .values()
            .map(|tool| tool.definition.clone())
            .collect()
    }

    fn definition(&self, name: &ToolName) -> Option<ToolDefinition> {
        self.tools.get(name).map(|tool| tool.definition.clone())
    }

    fn validate_arguments(
        &self,
        name: &ToolName,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolCatalogValidationFailure> {
        let tool = self
            .tools
            .get(name)
            .ok_or(ToolCatalogValidationFailure::UnknownTool)?;
        if arguments.kind() != ToolArgumentsKind::Json {
            return Err(ToolCatalogValidationFailure::InvalidArguments { detail: None });
        }
        tool.validator.validate(arguments).map_err(|detail| {
            ToolCatalogValidationFailure::InvalidArguments {
                detail: Some(detail),
            }
        })
    }
}

/// Empty catalog retained for callers that do not compose tool support.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoToolCatalog;

impl ToolCatalog for NoToolCatalog {
    fn definitions(&self) -> Box<[ToolDefinition]> {
        Box::new([])
    }

    fn definition(&self, _name: &ToolName) -> Option<ToolDefinition> {
        None
    }

    fn validate_arguments(
        &self,
        _name: &ToolName,
        _arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolCatalogValidationFailure> {
        Err(ToolCatalogValidationFailure::UnknownTool)
    }
}

/// Pure catalog preflight failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolCatalogValidationFailure {
    /// No declaration currently matches the request name.
    UnknownTool,
    /// Arguments are undecodable or do not match the selected type.
    InvalidArguments {
        /// Optional bounded sanitized decoder detail.
        detail: Option<ToolExecutionErrorDetail>,
    },
}

/// Storage-resolved authority for one tool-related semantic entry.
///
/// The prepared-model renderer correlates this evidence against the exact
/// reference-only semantic payload before exposing provider-neutral messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolvedToolConversationEntry {
    /// The request record referenced by one assistant tool-use entry.
    AssistantToolUse {
        /// Source-qualified semantic entry.
        source: SemanticTranscriptEntryRef,
        /// Immutable request content authority.
        request: ToolRequest,
    },
    /// The terminal attempt and request referenced by an execution-result entry.
    ExecutionResult {
        /// Source-qualified semantic entry.
        source: SemanticTranscriptEntryRef,
        /// Immutable request content authority.
        request: ToolRequest,
        /// Terminal physical result authority.
        attempt: EndedToolAttempt,
    },
    /// The owner decision and request referenced by one denial entry.
    Denied {
        /// Source-qualified semantic entry.
        source: SemanticTranscriptEntryRef,
        /// Immutable request content authority.
        request: ToolRequest,
        /// Exact durable denial and provenance.
        approval: ToolApprovalResolution,
    },
    /// The request referenced by one closed-by-turn-end entry.
    Closed {
        /// Source-qualified semantic entry.
        source: SemanticTranscriptEntryRef,
        /// Immutable request content authority.
        request: ToolRequest,
    },
}

impl ResolvedToolConversationEntry {
    /// Returns the semantic entry whose references this evidence resolves.
    pub const fn source(&self) -> SemanticTranscriptEntryRef {
        match self {
            Self::AssistantToolUse { source, .. }
            | Self::ExecutionResult { source, .. }
            | Self::Denied { source, .. }
            | Self::Closed { source, .. } => *source,
        }
    }
}

/// Authoritative reread after an ambiguous attempt-authorization commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolAttemptAuthorizationStatus {
    /// Authorization did not commit; the exact attempt remains prepared.
    Prepared(CurrentToolAttempt),
    /// Authorization committed; this exact fence may enter the executor.
    InFlight(AuthorizedToolAttempt),
}

/// Transaction consuming one owner decision and advancing the exact wait.
pub trait DecideToolRequestTransaction {
    /// Adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Applies a replay-safe command, consuming a fresh attempt candidate only
    /// when the final decision opens execution.
    fn decide<NextAttempt>(
        &mut self,
        command: DecideToolRequest,
        next_attempt: NextAttempt,
    ) -> impl Future<Output = Result<PreparedDecideToolRequest, Self::Error>> + Send
    where
        NextAttempt: FnMut() -> TurnAttemptId + Send;
}

/// Fresh identities for one all-resolved continuation transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolContinuationIdentities {
    result_entries: Box<[SemanticTranscriptEntryId]>,
    result_frontier: signalbox_domain::ContextFrontierId,
    call: ModelCallId,
    target_failure: FailedModelCallTurnIdentities,
    steering_frontier: signalbox_domain::ContextFrontierId,
}

impl ToolContinuationIdentities {
    /// Supplies exact proposal-order result identities and staged-call candidates.
    pub fn new(
        result_entries: Vec<SemanticTranscriptEntryId>,
        result_frontier: signalbox_domain::ContextFrontierId,
        call: ModelCallId,
        target_failure: FailedModelCallTurnIdentities,
        steering_frontier: signalbox_domain::ContextFrontierId,
    ) -> Self {
        Self {
            result_entries: result_entries.into_boxed_slice(),
            result_frontier,
            call,
            target_failure,
            steering_frontier,
        }
    }

    /// Returns result-entry identities in request order.
    pub fn result_entries(&self) -> &[SemanticTranscriptEntryId] {
        &self.result_entries
    }

    /// Returns the yielded-plus-results frontier candidate.
    pub const fn result_frontier(&self) -> signalbox_domain::ContextFrontierId {
        self.result_frontier
    }

    /// Returns the next model-call candidate.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Borrows target-failure closure candidates.
    pub const fn target_failure(&self) -> &FailedModelCallTurnIdentities {
        &self.target_failure
    }

    /// Returns the pending-steering frontier candidate.
    pub const fn steering_frontier(&self) -> signalbox_domain::ContextFrontierId {
        self.steering_frontier
    }
}

/// Fresh identities for proposal-ordered closure before a known crash failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCrashClosureIdentities {
    result_entries: Box<[SemanticTranscriptEntryId]>,
    result_frontier: signalbox_domain::ContextFrontierId,
    failure: FailedModelCallTurnIdentities,
}

impl ToolCrashClosureIdentities {
    /// Supplies one closure identity per request plus the terminal failure pair.
    pub fn new(
        result_entries: Vec<SemanticTranscriptEntryId>,
        result_frontier: signalbox_domain::ContextFrontierId,
        failure: FailedModelCallTurnIdentities,
    ) -> Self {
        Self {
            result_entries: result_entries.into_boxed_slice(),
            result_frontier,
            failure,
        }
    }

    /// Returns closure-entry identities in proposal order.
    pub fn result_entries(&self) -> &[SemanticTranscriptEntryId] {
        &self.result_entries
    }

    /// Returns the yielded-plus-closures frontier candidate.
    pub const fn result_frontier(&self) -> signalbox_domain::ContextFrontierId {
        self.result_frontier
    }

    /// Borrows the subsequent `TurnFailed` identity pair.
    pub const fn failure(&self) -> &FailedModelCallTurnIdentities {
        &self.failure
    }
}

/// Atomic continuation outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareToolContinuationOutcome {
    /// The scheduling hint no longer identifies an all-resolved active batch.
    NoWork,
    /// Results, steering, and the next Prepared call committed together.
    Checkpointed(ModelCallId),
    /// Target resolution failed and the turn closed in the same transaction.
    TargetUnavailable(Box<FailedModelCallTurn>),
}

/// Authoritative status of one unchanged in-memory executor observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetainedToolAttemptObservationStatus {
    /// The exact in-flight attempt still awaits this observation.
    Pending,
    /// The exact observation is already represented durably.
    AlreadyCommitted,
}

/// Authoritative tool execution and continuation transactions.
pub trait ToolExecutionTransaction {
    /// Adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Reloads one active batch without granting mutation authority.
    fn load_active_batch(
        &mut self,
        session: SessionId,
        turn: TurnId,
    ) -> impl Future<Output = Result<Option<ToolBatch>, Self::Error>> + Send;

    /// Commits the next proposal-order Prepared attempt.
    fn prepare_next_attempt(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        effect_class: ToolEffectClass,
    ) -> impl Future<Output = Result<signalbox_domain::CurrentToolAttempt, Self::Error>> + Send;

    /// Authorizes one exact Prepared attempt.
    fn authorize_attempt(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
    ) -> impl Future<Output = Result<AuthorizedToolAttempt, Self::Error>> + Send;

    /// Rereads whether an ambiguously acknowledged authorization committed.
    fn reread_ambiguous_authorization(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
    ) -> impl Future<Output = Result<ToolAttemptAuthorizationStatus, Self::Error>> + Send;

    /// Commits a catalog/decode failure without authorizing an executor effect.
    fn commit_preflight_error(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        error: ToolExecutionError,
    ) -> impl Future<Output = Result<EndedToolAttempt, Self::Error>> + Send;

    /// Commits exact executor evidence through its durable fence.
    fn commit_observation(
        &mut self,
        observation: CorrelatedToolAttemptObservation,
    ) -> impl Future<Output = Result<EndedToolAttempt, Self::Error>> + Send;

    /// Rereads whether one retained executor observation committed.
    fn reread_observation(
        &mut self,
        observation: &CorrelatedToolAttemptObservation,
    ) -> impl Future<Output = Result<RetainedToolAttemptObservationStatus, Self::Error>> + Send;

    /// Classifies one prior-process live attempt without retrying it.
    fn classify_crash_loss<NextTurn>(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        identities: ToolCrashClosureIdentities,
        next_turn: NextTurn,
    ) -> impl Future<Output = Result<ToolAttemptCrashOutcome, Self::Error>> + Send
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send;

    /// Atomically projects all results, consumes steering, and checkpoints the
    /// next model call.
    fn prepare_continuation<NextSteering>(
        &mut self,
        session: SessionId,
        turn: TurnId,
        producing_call: ModelCallId,
        identities: ToolContinuationIdentities,
        next_steering: NextSteering,
    ) -> impl Future<Output = Result<PrepareToolContinuationOutcome, Self::Error>> + Send
    where
        NextSteering: FnMut(AcceptedInputId) -> (SemanticTranscriptEntryId, TurnId) + Send;
}
