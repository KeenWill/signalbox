//! Application orchestration for tool approval, execution, and continuation.
//!
//! `docs/spec/tool-loop.md` owns the behavior. The application selects
//! catalog policy, mints every durable identity candidate, keeps executor work
//! outside transactions, and submits only correlated evidence to persistence.

use std::{collections::BTreeMap, error::Error, fmt, future::Future, num::NonZeroU64, sync::Arc};

use crate::{
    ClassifyOperatorFailure, DecideToolRequestTransaction, InProcessToolDispatchGate,
    InProcessToolDispatchPermit, OperatorFailureClass, OverrideDeniedToolRequestTransaction,
    PrepareToolContinuationOutcome, RetainedToolAttemptObservationStatus,
    ToolAttemptAuthorizationOutcome, ToolAttemptAuthorizationStatus, ToolContinuationIdentities,
    ToolCrashClosureIdentities, ToolExecutionTransaction,
};
#[cfg(test)]
use signalbox_domain::AcceptedInputId;
use signalbox_domain::{
    ChildWait, CorrelatedToolAttemptObservation, CurrentToolAttemptState,
    DangerousToolAutoApproval, DecideToolRequest, DelegationWait, EndedToolAttempt,
    FailedModelCallTurn, FailedModelCallTurnIdentities, InitialToolApproval, IssuedExecutorFence,
    ModelCallId, NormalizedToolArguments, OverrideDeniedToolRequest, PreparedDecideToolRequest,
    PreparedOverrideDeniedToolRequest, SemanticTranscriptEntryId, SessionId, ToolApprovalPosture,
    ToolArgumentsKind, ToolAttemptCrashOutcome, ToolAttemptDispatchCorrelation, ToolAttemptId,
    ToolAttemptObservation, ToolBatch, ToolBatchPhase, ToolDispatchAuthority, ToolEffectClass,
    ToolExecutionError, ToolExecutionErrorDetail, ToolExecutionErrorKind, ToolName,
    ToolPermissionDefault, ToolRequest, ToolRequestId, ToolResultContent, ToolResultText,
    ToolResultTextFailure, TurnAttemptId, TurnId,
};

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
    approval_posture: Option<ToolApprovalPosture>,
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
            approval_posture: None,
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

    /// Configures an explicit posture for this exact tool, superseding the session
    /// blanket and the registry default.
    pub const fn with_approval_posture(mut self, posture: ToolApprovalPosture) -> Self {
        self.approval_posture = Some(posture);
        self
    }

    /// Returns the configured per-tool posture, absent when no explicit
    /// per-tool posture is configured.
    pub const fn approval_posture(&self) -> Option<ToolApprovalPosture> {
        self.approval_posture
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

    /// Derives any durable resource charge required before dispatch authority.
    fn preauthorization(
        &self,
        _arguments: &NormalizedToolArguments,
    ) -> Result<ToolPreauthorization, ToolExecutionErrorDetail> {
        Ok(ToolPreauthorization::Unmetered)
    }
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

/// Pure catalog-derived resource admission supplied to durable authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolPreauthorization {
    /// No additional durable resource charge applies.
    Unmetered,
    /// One metadata request must prove the digest was visible in the frontier.
    BlobMetadata {
        /// Exact digest requested by the logical tool request.
        digest: signalbox_domain::BlobDigest,
    },
    /// One generic blob read charges its decoded byte length once by request.
    BlobRead {
        /// Exact digest requested by the logical tool request.
        digest: signalbox_domain::BlobDigest,
        /// Positive decoded bytes requested by the exact logical tool request.
        decoded_bytes: NonZeroU64,
    },
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
///
/// Implementations may compose immutable snapshots from compiled, database,
/// protocol, or runner-enrollment sources. Orchestration depends only on this
/// lookup/list/validation contract.
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

    /// Derives a typed durable admission charge after argument validation.
    fn preauthorization(
        &self,
        _name: &ToolName,
        _arguments: &NormalizedToolArguments,
    ) -> Result<ToolPreauthorization, ToolCatalogValidationFailure> {
        Ok(ToolPreauthorization::Unmetered)
    }
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

    fn preauthorization(
        &self,
        name: &ToolName,
        arguments: &NormalizedToolArguments,
    ) -> Result<ToolPreauthorization, ToolCatalogValidationFailure> {
        let tool = self
            .tools
            .get(name)
            .ok_or(ToolCatalogValidationFailure::UnknownTool)?;
        tool.validator
            .preauthorization(arguments)
            .map_err(|detail| ToolCatalogValidationFailure::InvalidArguments {
                detail: Some(detail),
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

/// Exact checked content and authorization supplied to one executor effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolExecutionInvocation {
    authority: ToolDispatchAuthority,
    definition: ToolDefinition,
}

impl ToolExecutionInvocation {
    fn try_new(
        request: ToolRequest,
        definition: ToolDefinition,
        authority: ToolDispatchAuthority,
    ) -> Option<Self> {
        (request == *authority.request()
            && request.name() == definition.name()
            && authority.attempt().effect_class() == definition.effect_class())
        .then_some(Self {
            authority,
            definition,
        })
    }

    /// Borrows the immutable request content authority.
    pub const fn request(&self) -> &ToolRequest {
        self.authority.request()
    }

    /// Borrows the sealed request-bearing dispatch authority.
    pub const fn dispatch_authority(&self) -> &ToolDispatchAuthority {
        &self.authority
    }

    /// Borrows the exact declaration selected by preflight.
    pub const fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    /// Returns the complete durable dispatch fence.
    pub const fn correlation(&self) -> ToolAttemptDispatchCorrelation {
        self.authority.correlation()
    }

    /// Binds returned executor evidence to the exact issued fence.
    pub fn bind(self, evidence: ToolExecutorEvidence) -> CorrelatedToolExecutorEvidence {
        CorrelatedToolExecutorEvidence {
            fence: self.authority.executor_fence(),
            evidence,
        }
    }

    /// Seals a claim that the executor transaction already ended this attempt.
    pub fn durable_completion(self) -> CorrelatedDurableToolCompletion {
        CorrelatedDurableToolCompletion {
            correlation: self.authority.correlation(),
        }
    }
}

/// Non-durable evidence returned by a tool executor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolExecutorEvidence {
    /// Exact UTF-8 output awaiting bounded domain admission.
    CompletedText(String),
    /// The tool definitively failed after checked dispatch.
    KnownFailed {
        /// Optional bounded, sanitized detail.
        detail: Option<ToolExecutionErrorDetail>,
    },
    /// The executor cannot establish whether an external effect occurred.
    Ambiguous,
}

/// Executor evidence carrying the exact issued dispatch fence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CorrelatedToolExecutorEvidence {
    fence: IssuedExecutorFence,
    evidence: ToolExecutorEvidence,
}

/// Exact dispatch fence for a terminal transition already committed by an executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CorrelatedDurableToolCompletion {
    correlation: ToolAttemptDispatchCorrelation,
}

impl CorrelatedDurableToolCompletion {
    /// Returns the complete issued dispatch correlation.
    pub const fn correlation(self) -> ToolAttemptDispatchCorrelation {
        self.correlation
    }
}

/// Executor evidence that one exact foreground await committed its durable
/// child-wait transition instead of returning a terminal tool result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CorrelatedDurableChildWait {
    correlation: ToolAttemptDispatchCorrelation,
    wait: DelegationWait,
    child_wait: ChildWait,
}

impl CorrelatedDurableChildWait {
    /// Correlates a foreground wait with its physical dispatch fence.
    pub fn try_new(
        correlation: ToolAttemptDispatchCorrelation,
        wait: DelegationWait,
    ) -> Option<Self> {
        let child_wait = wait.foreground_subject()?;
        (wait.parent() == correlation.session() && wait.awaiting_request() == correlation.request())
            .then_some(Self {
                correlation,
                wait,
                child_wait,
            })
    }

    /// Returns the complete issued dispatch correlation.
    pub const fn correlation(self) -> ToolAttemptDispatchCorrelation {
        self.correlation
    }

    /// Returns the exact registered foreground wait.
    pub const fn wait(self) -> DelegationWait {
        self.wait
    }

    /// Returns the exact child-wait subject retained by the parent turn.
    pub const fn child_wait(self) -> ChildWait {
        self.child_wait
    }
}

/// Nonblocking executor outcome: ordinary evidence or an already-durable transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolExecutorDisposition {
    /// Ordinary executor evidence still requiring durable observation commit.
    Completed(CorrelatedToolExecutorEvidence),
    /// Terminal evidence the executor already committed with its exact effect.
    DurableCompletion(CorrelatedDurableToolCompletion),
    /// The executor's transaction already parked this exact foreground wait.
    DurableChildWait(CorrelatedDurableChildWait),
}
impl CorrelatedToolExecutorEvidence {
    /// Returns the executor-supplied correlation.
    pub const fn correlation(&self) -> ToolAttemptDispatchCorrelation {
        self.fence.correlation()
    }

    /// Borrows returned evidence.
    pub const fn evidence(&self) -> &ToolExecutorEvidence {
        &self.evidence
    }
}

/// In-process or future runner-backed tool executor port.
pub trait ToolExecutor {
    /// Sanitized adapter-specific failure when no trustworthy evidence exists.
    type Error: ClassifyOperatorFailure;

    /// Performs at most one physical effect and returns fenced evidence.
    fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> impl Future<Output = Result<CorrelatedToolExecutorEvidence, Self::Error>> + Send;

    /// Executes with scheduling-aware support for tools whose transaction can
    /// durably yield the current turn. Ordinary executors use terminal evidence.
    fn execute_with_scheduling(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> impl Future<Output = Result<ToolExecutorDisposition, Self::Error>> + Send
    where
        Self: Send,
    {
        async move {
            self.execute(invocation)
                .await
                .map(ToolExecutorDisposition::Completed)
        }
    }
}

/// Supplies UUIDv7 candidates for approval progression.
pub trait ToolApprovalIdGenerator {
    /// Generates a fresh continuation turn-attempt candidate.
    fn next_tool_turn_attempt_id(&mut self) -> TurnAttemptId;
}

/// Supplies UUIDv7 candidates for tool dispatch and continuation.
pub trait ToolExecutionIdGenerator {
    /// Generates a fresh turn-attempt candidate after a durable child wait.
    fn next_tool_turn_attempt_id(&mut self) -> TurnAttemptId;
    /// Generates a fresh physical tool-attempt candidate.
    fn next_tool_attempt_id(&mut self) -> ToolAttemptId;
    /// Generates a fresh semantic result/steering entry candidate.
    fn next_tool_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId;
    /// Generates a fresh result or steering frontier candidate.
    fn next_tool_context_frontier_id(&mut self) -> signalbox_domain::ContextFrontierId;
    /// Generates a fresh continuation model-call candidate.
    fn next_tool_model_call_id(&mut self) -> ModelCallId;
    /// Generates a fresh successor turn for reclassified steering.
    fn next_tool_turn_id(&mut self) -> TurnId;
}

/// Production UUIDv7 generator for all tool-loop application identities.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7ToolLoopIdGenerator;

impl ToolApprovalIdGenerator for UuidV7ToolLoopIdGenerator {
    fn next_tool_turn_attempt_id(&mut self) -> TurnAttemptId {
        TurnAttemptId::from_uuid(uuid::Uuid::now_v7())
    }
}

impl ToolExecutionIdGenerator for UuidV7ToolLoopIdGenerator {
    fn next_tool_turn_attempt_id(&mut self) -> TurnAttemptId {
        TurnAttemptId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_tool_attempt_id(&mut self) -> ToolAttemptId {
        ToolAttemptId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_tool_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
        SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_tool_context_frontier_id(&mut self) -> signalbox_domain::ContextFrontierId {
        signalbox_domain::ContextFrontierId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_tool_model_call_id(&mut self) -> ModelCallId {
        ModelCallId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_tool_turn_id(&mut self) -> TurnId {
        TurnId::from_uuid(uuid::Uuid::now_v7())
    }
}

/// Application service for one durable user approval/denial command.
pub struct DecideToolRequestService<Ids, Transaction> {
    ids: Ids,
    transaction: Transaction,
}

impl<Ids, Transaction> DecideToolRequestService<Ids, Transaction> {
    /// Composes application-owned identities with the authoritative transaction.
    pub const fn new(ids: Ids, transaction: Transaction) -> Self {
        Self { ids, transaction }
    }

    /// Returns both owned roles.
    pub fn into_parts(self) -> (Ids, Transaction) {
        (self.ids, self.transaction)
    }
}

impl<Ids, Transaction> DecideToolRequestService<Ids, Transaction>
where
    Ids: ToolApprovalIdGenerator + Send,
    Transaction: DecideToolRequestTransaction,
{
    /// Applies one command, retrying only fresh-candidate collisions.
    pub async fn execute(
        &mut self,
        command: DecideToolRequest,
    ) -> Result<PreparedDecideToolRequest, Transaction::Error> {
        loop {
            let ids = &mut self.ids;
            match self
                .transaction
                .decide(command.clone(), || ids.next_tool_turn_attempt_id())
                .await
            {
                Err(error)
                    if error.operator_failure_class()
                        == OperatorFailureClass::IdentityCollision =>
                {
                    continue;
                }
                result => return result,
            }
        }
    }
}

/// Application service for one durable delegate-denial override command.
pub struct OverrideDeniedToolRequestService<Transaction> {
    transaction: Transaction,
}

impl<Transaction> OverrideDeniedToolRequestService<Transaction> {
    /// Wraps the authoritative transaction.
    pub const fn new(transaction: Transaction) -> Self {
        Self { transaction }
    }

    /// Returns the owned transaction role.
    pub fn into_transaction(self) -> Transaction {
        self.transaction
    }
}

impl<Transaction> OverrideDeniedToolRequestService<Transaction>
where
    Transaction: OverrideDeniedToolRequestTransaction,
{
    /// Applies one override command; the transaction mints no fresh
    /// identities, so no collision retry exists to run.
    pub async fn execute(
        &mut self,
        command: OverrideDeniedToolRequest,
    ) -> Result<PreparedOverrideDeniedToolRequest, Transaction::Error> {
        self.transaction.override_denied(command).await
    }
}

/// Opaque same-incarnation executor evidence retained across a failed commit.
pub struct RetainedToolExecutionState {
    state: RetainedToolExecutionStateKind,
}

enum RetainedToolExecutionStateKind {
    AuthorizationNonConsumption {
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        request: ToolRequest,
        definition: ToolDefinition,
        result_entry_count: usize,
        dispatch_permit: InProcessToolDispatchPermit,
    },
    Observation {
        observation: CorrelatedToolAttemptObservation,
        dispatch_permit: InProcessToolDispatchPermit,
    },
    DurableCompletion {
        completion: CorrelatedDurableToolCompletion,
        dispatch_permit: InProcessToolDispatchPermit,
    },
    DurableChildWait {
        wait: CorrelatedDurableChildWait,
        dispatch_permit: InProcessToolDispatchPermit,
    },
    CrashClassification {
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        result_entry_count: usize,
        cause: RetainedCrashCause,
        dispatch_permit: InProcessToolDispatchPermit,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RetainedExecutorFailure {
    failure_class: OperatorFailureClass,
    cause_code: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetainedCrashCause {
    PriorProcess,
    Executor(RetainedExecutorFailure),
    CorrelationMismatch,
}

enum UntrustedExecutorFailure<ExecutorError> {
    Executor(ExecutorError),
    CorrelationMismatch,
}

impl fmt::Debug for RetainedToolExecutionState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedToolExecutionState")
            .field(
                "stage",
                &match &self.state {
                    RetainedToolExecutionStateKind::AuthorizationNonConsumption { .. } => {
                        "authorization_non_consumption"
                    }
                    RetainedToolExecutionStateKind::Observation { .. } => "observation",
                    RetainedToolExecutionStateKind::DurableCompletion { .. } => {
                        "durable_completion"
                    }
                    RetainedToolExecutionStateKind::DurableChildWait { .. } => "durable_child_wait",
                    RetainedToolExecutionStateKind::CrashClassification { .. } => {
                        "crash_classification"
                    }
                },
            )
            .field("holds_dispatch_permit", &true)
            .finish()
    }
}

#[cfg(test)]
impl RetainedToolExecutionState {
    fn crash_executor_failure(&self) -> Option<RetainedExecutorFailure> {
        match &self.state {
            RetainedToolExecutionStateKind::CrashClassification {
                cause: RetainedCrashCause::Executor(failure),
                ..
            } => Some(*failure),
            RetainedToolExecutionStateKind::AuthorizationNonConsumption { .. }
            | RetainedToolExecutionStateKind::Observation { .. }
            | RetainedToolExecutionStateKind::DurableCompletion { .. }
            | RetainedToolExecutionStateKind::DurableChildWait { .. }
            | RetainedToolExecutionStateKind::CrashClassification { .. } => None,
        }
    }
}

/// One completed stage of serialized tool execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolExecutionServiceOutcome {
    /// No active tool batch matches the hint.
    NoWork,
    /// The batch remains parked on its earliest undecided request.
    AwaitingApproval(ToolRequestId),
    /// Exact ambiguity remains parked for user recovery.
    AwaitingRecovery(ToolAttemptId),
    /// A delivered foreground child result reopened serialized execution.
    ChildWaitResumed(TurnAttemptId),
    /// A foreground await atomically parked the current turn on its child.
    ChildWaitParked(ChildWait),
    /// A fresh attempt checkpoint committed; execution waits for another pass.
    AttemptCheckpointed(ToolAttemptId),
    /// Pure preflight closed one attempt with typed error evidence.
    PreflightFailed(Box<EndedToolAttempt>),
    /// One executor observation committed durably.
    ObservationCommitted(Box<EndedToolAttempt>),
    /// The retained executor observation was already represented durably.
    ObservationAlreadyCommitted(ToolAttemptId),
    /// A prior-process live attempt or same-process executor failure was
    /// classified without retry.
    CrashClassified(Box<ToolAttemptCrashOutcome>),
    /// The all-resolved continuation call committed atomically.
    ContinuationCheckpointed(ModelCallId),
    /// Continuation target resolution closed the turn atomically.
    ContinuationTargetUnavailable(Box<FailedModelCallTurn>),
    /// Continuation credential-pool exhaustion closed the turn atomically.
    ContinuationPoolExhausted(Box<signalbox_domain::CredentialPoolExhaustedModelCallTurn>),
    /// Reported usage closed the turn before an oversized continuation.
    ContinuationContextCompactionRequired(
        Box<signalbox_domain::ContextHeadroomExhaustedModelCallTurn>,
    ),
}

const fn is_fatal_executor_failure_class(failure: OperatorFailureClass) -> bool {
    match failure {
        OperatorFailureClass::FailClosedCorruption | OperatorFailureClass::CallerOrHubBug => true,
        OperatorFailureClass::Infrastructure { .. } | OperatorFailureClass::IdentityCollision => {
            false
        }
    }
}

fn emit_contained_executor_failure(
    session: SessionId,
    turn: TurnId,
    attempt: ToolAttemptId,
    failure: RetainedExecutorFailure,
) {
    tracing::warn!(
        session_id = %session.as_uuid(),
        turn_id = %turn.as_uuid(),
        tool_attempt_id = %attempt.as_uuid(),
        failure_class = ?failure.failure_class,
        cause_code = failure.cause_code,
        "tool executor failed after dispatch; durable crash classification contains the failure"
    );
}

/// Failure annotated with the exact tool orchestration stage.
#[derive(Debug)]
pub enum ToolExecutionServiceError<TransactionError, ExecutorError> {
    /// Loading current batch state failed.
    Load(TransactionError),
    /// Preparing a durable physical attempt failed.
    Prepare(TransactionError),
    /// Authorizing a prepared attempt failed.
    Authorize(TransactionError),
    /// A commit-ambiguous authorization and its immediate reread both failed.
    AuthorizationReread {
        /// Original commit-ambiguous authorization failure.
        authorization_error: TransactionError,
        /// Failure to establish whether authorization committed.
        reread_error: TransactionError,
    },
    /// A later pass could not reconcile retained non-consumption evidence.
    AuthorizationReconciliation(TransactionError),
    /// A local preflight error could not commit.
    PreflightCommit(TransactionError),
    /// Executor work produced no trustworthy evidence.
    Executor(ExecutorError),
    /// Executor work failed and its required crash classification also failed.
    ExecutorCrashClassification {
        /// Original executor failure.
        executor_error: ExecutorError,
        /// Failure to durably classify the in-flight attempt.
        classification_error: TransactionError,
    },
    /// Executor evidence named a dispatch fence other than the invocation.
    ExecutorCorrelationMismatch,
    /// Cross-wired executor evidence and its required crash classification both failed.
    ExecutorCorrelationMismatchCrashClassification(TransactionError),
    /// Executor evidence could not commit.
    ObservationCommit(TransactionError),
    /// Retained executor evidence could not be reconciled with durable state.
    ObservationReconciliation(TransactionError),
    /// An executor-reported durable completion could not be reread from storage.
    DurableCompletionReconciliation(TransactionError),
    /// An executor-reported durable completion was absent or cross-wired.
    DurableCompletionMismatch,
    /// A reported durable child wait could not be reread from storage.
    ChildWaitReconciliation(TransactionError),
    /// A reported durable child wait was absent or cross-wired.
    ChildWaitMismatch,
    /// Crash classification failed.
    CrashClassification(TransactionError),
    /// A retained fatal executor failure was durably classified on retry.
    RecoveredFatalExecutorFailure {
        /// Original fatal operator classification.
        failure_class: OperatorFailureClass,
        /// Original safe executor cause token.
        cause_code: &'static str,
    },
    /// Atomic continuation preparation failed.
    Continuation(TransactionError),
    /// Catalog metadata no longer matches durable attempt authorization.
    CatalogDrift,
}

impl<TransactionError, ExecutorError> fmt::Display
    for ToolExecutionServiceError<TransactionError, ExecutorError>
where
    TransactionError: fmt::Display,
    ExecutorError: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(error) => write!(formatter, "tool batch load failed: {error}"),
            Self::Prepare(error) => write!(formatter, "tool attempt prepare failed: {error}"),
            Self::Authorize(error) => {
                write!(formatter, "tool attempt authorization failed: {error}")
            }
            Self::AuthorizationReread { reread_error, .. } => {
                write!(
                    formatter,
                    "tool attempt authorization reread failed: {reread_error}"
                )
            }
            Self::AuthorizationReconciliation(error) => {
                write!(
                    formatter,
                    "tool attempt authorization reconciliation failed: {error}"
                )
            }
            Self::PreflightCommit(error) => {
                write!(formatter, "tool preflight evidence commit failed: {error}")
            }
            Self::Executor(error) => write!(formatter, "tool executor failed: {error}"),
            Self::ExecutorCrashClassification {
                executor_error,
                classification_error,
            } => write!(
                formatter,
                "tool executor failed ({executor_error}) and crash classification failed: \
                 {classification_error}"
            ),
            Self::ExecutorCorrelationMismatch => {
                formatter.write_str("tool executor evidence carried a different dispatch fence")
            }
            Self::ExecutorCorrelationMismatchCrashClassification(error) => write!(
                formatter,
                "tool executor evidence carried a different dispatch fence and crash \
                 classification failed: {error}"
            ),
            Self::ObservationCommit(error) => {
                write!(formatter, "tool observation commit failed: {error}")
            }
            Self::ObservationReconciliation(error) => {
                write!(formatter, "tool observation reconciliation failed: {error}")
            }
            Self::DurableCompletionReconciliation(error) => write!(
                formatter,
                "durable tool completion reconciliation failed: {error}"
            ),
            Self::DurableCompletionMismatch => {
                formatter.write_str("executor durable completion did not match storage")
            }
            Self::ChildWaitReconciliation(error) => {
                write!(
                    formatter,
                    "durable child wait reconciliation failed: {error}"
                )
            }
            Self::ChildWaitMismatch => {
                formatter.write_str("executor durable child wait did not match storage")
            }
            Self::CrashClassification(error) => {
                write!(formatter, "tool crash classification failed: {error}")
            }
            Self::RecoveredFatalExecutorFailure { .. } => formatter.write_str(
                "fatal tool executor failure remained fatal after crash classification recovery",
            ),
            Self::Continuation(error) => write!(formatter, "tool continuation failed: {error}"),
            Self::CatalogDrift => {
                formatter.write_str("tool catalog metadata changed after attempt preparation")
            }
        }
    }
}

impl<TransactionError, ExecutorError> Error
    for ToolExecutionServiceError<TransactionError, ExecutorError>
where
    TransactionError: Error + 'static,
    ExecutorError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Load(error)
            | Self::Prepare(error)
            | Self::Authorize(error)
            | Self::AuthorizationReconciliation(error)
            | Self::PreflightCommit(error)
            | Self::ExecutorCorrelationMismatchCrashClassification(error)
            | Self::ObservationCommit(error)
            | Self::ObservationReconciliation(error)
            | Self::DurableCompletionReconciliation(error)
            | Self::ChildWaitReconciliation(error)
            | Self::CrashClassification(error)
            | Self::Continuation(error) => Some(error),
            Self::AuthorizationReread { reread_error, .. } => Some(reread_error),
            Self::Executor(error) => Some(error),
            Self::ExecutorCrashClassification {
                classification_error,
                ..
            } => Some(classification_error),
            Self::ExecutorCorrelationMismatch
            | Self::DurableCompletionMismatch
            | Self::ChildWaitMismatch
            | Self::RecoveredFatalExecutorFailure { .. }
            | Self::CatalogDrift => None,
        }
    }
}

impl<TransactionError, ExecutorError> ClassifyOperatorFailure
    for ToolExecutionServiceError<TransactionError, ExecutorError>
where
    TransactionError: ClassifyOperatorFailure,
    ExecutorError: ClassifyOperatorFailure,
{
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Load(error)
            | Self::Prepare(error)
            | Self::Authorize(error)
            | Self::AuthorizationReconciliation(error)
            | Self::PreflightCommit(error)
            | Self::ObservationCommit(error)
            | Self::ObservationReconciliation(error)
            | Self::DurableCompletionReconciliation(error)
            | Self::ChildWaitReconciliation(error)
            | Self::CrashClassification(error)
            | Self::ExecutorCorrelationMismatchCrashClassification(error)
            | Self::Continuation(error) => error.operator_failure_class(),
            Self::AuthorizationReread { reread_error, .. } => reread_error.operator_failure_class(),
            Self::Executor(error) => error.operator_failure_class(),
            Self::ExecutorCrashClassification {
                classification_error,
                ..
            } => classification_error.operator_failure_class(),
            Self::RecoveredFatalExecutorFailure { failure_class, .. } => *failure_class,
            Self::ExecutorCorrelationMismatch
            | Self::DurableCompletionMismatch
            | Self::ChildWaitMismatch
            | Self::CatalogDrift => OperatorFailureClass::CallerOrHubBug,
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Load(_) => "tool_batch_load",
            Self::Prepare(_) => "tool_attempt_prepare",
            Self::Authorize(_) => "tool_attempt_authorization",
            Self::AuthorizationReread { .. } => "tool_attempt_authorization_reread",
            Self::AuthorizationReconciliation(_) => "tool_attempt_authorization_reconciliation",
            Self::PreflightCommit(_) => "tool_preflight_commit",
            Self::Executor(_) => "tool_executor",
            Self::ExecutorCrashClassification { .. } => "tool_executor_crash_classification",
            Self::ExecutorCorrelationMismatch => "tool_executor_correlation_mismatch",
            Self::ExecutorCorrelationMismatchCrashClassification(_) => {
                "tool_executor_correlation_mismatch_crash_classification"
            }
            Self::ObservationCommit(_) => "tool_observation_commit",
            Self::ObservationReconciliation(_) => "tool_observation_reconciliation",
            Self::DurableCompletionReconciliation(_) => "tool_durable_completion_reconciliation",
            Self::DurableCompletionMismatch => "tool_durable_completion_mismatch",
            Self::ChildWaitReconciliation(_) => "tool_child_wait_reconciliation",
            Self::ChildWaitMismatch => "tool_child_wait_mismatch",
            Self::CrashClassification(_) => "tool_crash_classification",
            Self::RecoveredFatalExecutorFailure { cause_code, .. } => cause_code,
            Self::Continuation(_) => "tool_continuation",
            Self::CatalogDrift => "tool_catalog_drift",
        }
    }
}

/// Coordinates one serialized tool-loop stage.
pub struct ToolExecutionService<Ids, Transaction, Catalog, Executor> {
    ids: Ids,
    transaction: Transaction,
    catalog: Catalog,
    executor: Executor,
    gate: InProcessToolDispatchGate,
    retained_state: Option<RetainedToolExecutionState>,
}

impl<Ids, Transaction, Catalog, Executor>
    ToolExecutionService<Ids, Transaction, Catalog, Executor>
{
    /// Composes application identities, transactions, catalog, and executor.
    pub const fn new(
        ids: Ids,
        transaction: Transaction,
        catalog: Catalog,
        executor: Executor,
        gate: InProcessToolDispatchGate,
    ) -> Self {
        Self {
            ids,
            transaction,
            catalog,
            executor,
            gate,
            retained_state: None,
        }
    }

    /// Reconstitutes an explicitly decomposed service without losing evidence.
    pub const fn from_parts(
        ids: Ids,
        transaction: Transaction,
        catalog: Catalog,
        executor: Executor,
        gate: InProcessToolDispatchGate,
        retained_state: Option<RetainedToolExecutionState>,
    ) -> Self {
        Self {
            ids,
            transaction,
            catalog,
            executor,
            gate,
            retained_state,
        }
    }

    /// Returns every owned role for explicit composition.
    pub fn into_parts(
        self,
    ) -> (
        Ids,
        Transaction,
        Catalog,
        Executor,
        InProcessToolDispatchGate,
        Option<RetainedToolExecutionState>,
    ) {
        (
            self.ids,
            self.transaction,
            self.catalog,
            self.executor,
            self.gate,
            self.retained_state,
        )
    }

    /// Borrows same-incarnation executor evidence awaiting reconciliation.
    pub const fn retained_state(&self) -> Option<&RetainedToolExecutionState> {
        self.retained_state.as_ref()
    }
}

impl<Ids, Transaction, Catalog, Executor> ToolExecutionService<Ids, Transaction, Catalog, Executor>
where
    Ids: ToolExecutionIdGenerator + Send,
    Transaction: ToolExecutionTransaction,
    Catalog: ToolCatalog,
    Executor: ToolExecutor + Send,
{
    /// Runs at most one attempt preparation, executor effect, crash
    /// classification, or continuation checkpoint for an authoritative hint.
    pub async fn execute(
        &mut self,
        session: SessionId,
        turn: TurnId,
    ) -> Result<
        ToolExecutionServiceOutcome,
        ToolExecutionServiceError<Transaction::Error, Executor::Error>,
    > {
        if let Some(retained) = self.retained_state.take() {
            match retained.state {
                RetainedToolExecutionStateKind::AuthorizationNonConsumption {
                    session,
                    turn,
                    attempt,
                    request,
                    definition,
                    result_entry_count,
                    dispatch_permit,
                } => match self
                    .transaction
                    .reread_ambiguous_authorization(session, turn, attempt)
                    .await
                {
                    Ok(ToolAttemptAuthorizationStatus::Prepared(prepared)) => {
                        drop(dispatch_permit);
                        return self.execute_prepared(request, prepared).await;
                    }
                    Ok(ToolAttemptAuthorizationStatus::InFlight(authorized)) => {
                        return self
                            .execute_authorized(
                                request,
                                definition,
                                authorized,
                                result_entry_count,
                                dispatch_permit,
                            )
                            .await;
                    }
                    Err(error) => {
                        self.retained_state = Some(RetainedToolExecutionState {
                            state: RetainedToolExecutionStateKind::AuthorizationNonConsumption {
                                session,
                                turn,
                                attempt,
                                request,
                                definition,
                                result_entry_count,
                                dispatch_permit,
                            },
                        });
                        return Err(ToolExecutionServiceError::AuthorizationReconciliation(
                            error,
                        ));
                    }
                },
                RetainedToolExecutionStateKind::Observation {
                    observation,
                    dispatch_permit,
                } => {
                    let attempt = observation.correlation().attempt();
                    match self.transaction.reread_observation(&observation).await {
                        Ok(RetainedToolAttemptObservationStatus::Pending) => {
                            return self
                                .commit_executor_observation(observation, dispatch_permit)
                                .await;
                        }
                        Ok(RetainedToolAttemptObservationStatus::AlreadyCommitted) => {
                            return Ok(ToolExecutionServiceOutcome::ObservationAlreadyCommitted(
                                attempt,
                            ));
                        }
                        Err(error) => {
                            self.retained_state = Some(RetainedToolExecutionState {
                                state: RetainedToolExecutionStateKind::Observation {
                                    observation,
                                    dispatch_permit,
                                },
                            });
                            return Err(ToolExecutionServiceError::ObservationReconciliation(
                                error,
                            ));
                        }
                    }
                }
                RetainedToolExecutionStateKind::DurableCompletion {
                    completion,
                    dispatch_permit,
                } => {
                    return self
                        .reconcile_durable_completion(completion, dispatch_permit)
                        .await;
                }
                RetainedToolExecutionStateKind::DurableChildWait {
                    wait,
                    dispatch_permit,
                } => {
                    return self
                        .reconcile_durable_child_wait(wait, dispatch_permit)
                        .await;
                }
                RetainedToolExecutionStateKind::CrashClassification {
                    session,
                    turn,
                    attempt,
                    result_entry_count,
                    cause,
                    dispatch_permit,
                } => {
                    let classification = self
                        .classify_crash_loss(
                            session,
                            turn,
                            attempt,
                            result_entry_count,
                            cause,
                            dispatch_permit,
                        )
                        .await;
                    return match (classification, cause) {
                        (Ok(_), RetainedCrashCause::Executor(failure))
                            if is_fatal_executor_failure_class(failure.failure_class) =>
                        {
                            Err(ToolExecutionServiceError::RecoveredFatalExecutorFailure {
                                failure_class: failure.failure_class,
                                cause_code: failure.cause_code,
                            })
                        }
                        (Ok(outcome), RetainedCrashCause::Executor(failure)) => {
                            emit_contained_executor_failure(session, turn, attempt, failure);
                            Ok(ToolExecutionServiceOutcome::CrashClassified(Box::new(
                                outcome,
                            )))
                        }
                        (Ok(outcome), RetainedCrashCause::PriorProcess) => Ok(
                            ToolExecutionServiceOutcome::CrashClassified(Box::new(outcome)),
                        ),
                        (Ok(_), RetainedCrashCause::CorrelationMismatch) => {
                            Err(ToolExecutionServiceError::ExecutorCorrelationMismatch)
                        }
                        (
                            Err(error),
                            RetainedCrashCause::PriorProcess
                            | RetainedCrashCause::Executor(_)
                            | RetainedCrashCause::CorrelationMismatch,
                        ) => Err(ToolExecutionServiceError::CrashClassification(error)),
                    };
                }
            }
        }
        let Some(batch) = self
            .transaction
            .load_active_batch(session, turn)
            .await
            .map_err(ToolExecutionServiceError::Load)?
        else {
            return Ok(ToolExecutionServiceOutcome::NoWork);
        };

        match batch.phase() {
            ToolBatchPhase::AwaitingApproval { request } => {
                Ok(ToolExecutionServiceOutcome::AwaitingApproval(request))
            }
            ToolBatchPhase::AwaitingRecovery { attempt } => {
                Ok(ToolExecutionServiceOutcome::AwaitingRecovery(attempt))
            }
            ToolBatchPhase::AwaitingChild { .. } => loop {
                let continuation = self.ids.next_tool_turn_attempt_id();
                match self
                    .transaction
                    .resume_child_wait(session, turn, continuation)
                    .await
                {
                    Err(error)
                        if error.operator_failure_class()
                            == OperatorFailureClass::IdentityCollision => {}
                    Ok(true) => {
                        return Ok(ToolExecutionServiceOutcome::ChildWaitResumed(continuation));
                    }
                    Ok(false) => return Ok(ToolExecutionServiceOutcome::NoWork),
                    Err(error) => return Err(ToolExecutionServiceError::Prepare(error)),
                }
            },
            ToolBatchPhase::Executing { .. } => self.execute_batch(batch).await,
        }
    }

    async fn execute_batch(
        &mut self,
        batch: ToolBatch,
    ) -> Result<
        ToolExecutionServiceOutcome,
        ToolExecutionServiceError<Transaction::Error, Executor::Error>,
    > {
        for request in batch.requests() {
            let Some(attempt) = batch.attempt(request.id()) else {
                if batch
                    .approval(request.id())
                    .is_some_and(signalbox_domain::ToolApprovalResolution::is_approved)
                {
                    return self.prepare_attempt(&batch, request).await;
                }
                continue;
            };
            if let signalbox_domain::ReconstitutedToolAttempt::Current(current) = attempt {
                return match current.state() {
                    CurrentToolAttemptState::Prepared => {
                        self.execute_prepared(request.clone(), current.clone())
                            .await
                    }
                    CurrentToolAttemptState::InFlight => {
                        let expected_attempt = current.attempt();
                        let _dispatch_permit = self.gate.acquire(current.turn()).await;
                        let Some(reloaded_batch) = self
                            .transaction
                            .load_active_batch(current.session(), current.turn())
                            .await
                            .map_err(ToolExecutionServiceError::Load)?
                        else {
                            return Ok(ToolExecutionServiceOutcome::NoWork);
                        };
                        let Some(signalbox_domain::ReconstitutedToolAttempt::Current(current)) =
                            reloaded_batch.attempt(request.id())
                        else {
                            return Ok(ToolExecutionServiceOutcome::NoWork);
                        };
                        if current.attempt() != expected_attempt
                            || current.state() != CurrentToolAttemptState::InFlight
                        {
                            return Ok(ToolExecutionServiceOutcome::NoWork);
                        }
                        self.classify_crash_loss(
                            current.session(),
                            current.turn(),
                            current.attempt(),
                            reloaded_batch.requests().len(),
                            RetainedCrashCause::PriorProcess,
                            _dispatch_permit,
                        )
                        .await
                        .map(|outcome| {
                            ToolExecutionServiceOutcome::CrashClassified(Box::new(outcome))
                        })
                        .map_err(ToolExecutionServiceError::CrashClassification)
                    }
                };
            }
        }
        self.prepare_continuation(&batch).await
    }

    async fn prepare_attempt(
        &mut self,
        batch: &ToolBatch,
        request: &ToolRequest,
    ) -> Result<
        ToolExecutionServiceOutcome,
        ToolExecutionServiceError<Transaction::Error, Executor::Error>,
    > {
        let effect_class = self
            .catalog
            .definition(request.name())
            .map_or(ToolEffectClass::EffectFree, |definition| {
                definition.effect_class()
            });
        loop {
            let attempt = self.ids.next_tool_attempt_id();
            let _dispatch_permit = self.gate.acquire(batch.turn()).await;
            match self
                .transaction
                .prepare_next_attempt(batch.session(), batch.turn(), attempt, effect_class)
                .await
            {
                Err(error)
                    if error.operator_failure_class()
                        == OperatorFailureClass::IdentityCollision =>
                {
                    continue;
                }
                Ok(Some(prepared)) => {
                    return Ok(ToolExecutionServiceOutcome::AttemptCheckpointed(
                        prepared.attempt(),
                    ));
                }
                Ok(None) => return Ok(ToolExecutionServiceOutcome::NoWork),
                Err(error) => return Err(ToolExecutionServiceError::Prepare(error)),
            }
        }
    }

    async fn execute_prepared(
        &mut self,
        request: ToolRequest,
        prepared: signalbox_domain::CurrentToolAttempt,
    ) -> Result<
        ToolExecutionServiceOutcome,
        ToolExecutionServiceError<Transaction::Error, Executor::Error>,
    > {
        let dispatch_permit = self.gate.acquire(prepared.turn()).await;
        let Some(reloaded_batch) = self
            .transaction
            .load_active_batch(prepared.session(), prepared.turn())
            .await
            .map_err(ToolExecutionServiceError::Load)?
        else {
            return Ok(ToolExecutionServiceOutcome::NoWork);
        };
        let exact_prepared_attempt = matches!(
            reloaded_batch.attempt(request.id()),
            Some(signalbox_domain::ReconstitutedToolAttempt::Current(current))
                if current == &prepared && current.state() == CurrentToolAttemptState::Prepared
        ) && reloaded_batch
            .requests()
            .iter()
            .any(|candidate| candidate == &request);
        if !exact_prepared_attempt {
            return Ok(ToolExecutionServiceOutcome::NoWork);
        }
        let result_entry_count = reloaded_batch.requests().len();

        let definition = self.catalog.definition(request.name());
        let preflight = match definition.as_ref() {
            None => Some(ToolExecutionError::new(
                ToolExecutionErrorKind::UnknownTool,
                None,
            )),
            Some(_) if request.arguments().kind() != ToolArgumentsKind::Json => Some(
                ToolExecutionError::new(ToolExecutionErrorKind::InvalidArguments, None),
            ),
            Some(definition) if definition.effect_class() != prepared.effect_class() => {
                return Err(ToolExecutionServiceError::CatalogDrift);
            }
            Some(_) => match self
                .catalog
                .validate_arguments(request.name(), request.arguments())
            {
                Ok(()) => None,
                Err(ToolCatalogValidationFailure::UnknownTool) => Some(ToolExecutionError::new(
                    ToolExecutionErrorKind::UnknownTool,
                    None,
                )),
                Err(ToolCatalogValidationFailure::InvalidArguments { detail }) => Some(
                    ToolExecutionError::new(ToolExecutionErrorKind::InvalidArguments, detail),
                ),
            },
        };
        if let Some(error) = preflight {
            let ended = self
                .transaction
                .commit_preflight_error(
                    prepared.session(),
                    prepared.turn(),
                    prepared.attempt(),
                    error,
                )
                .await
                .map_err(ToolExecutionServiceError::PreflightCommit)?;
            return Ok(ToolExecutionServiceOutcome::PreflightFailed(Box::new(
                ended,
            )));
        }
        let preauthorization = match self
            .catalog
            .preauthorization(request.name(), request.arguments())
        {
            Ok(preauthorization) => preauthorization,
            Err(ToolCatalogValidationFailure::UnknownTool) => {
                return Err(ToolExecutionServiceError::CatalogDrift);
            }
            Err(ToolCatalogValidationFailure::InvalidArguments { detail }) => {
                let ended = self
                    .transaction
                    .commit_preflight_error(
                        prepared.session(),
                        prepared.turn(),
                        prepared.attempt(),
                        ToolExecutionError::new(ToolExecutionErrorKind::InvalidArguments, detail),
                    )
                    .await
                    .map_err(ToolExecutionServiceError::PreflightCommit)?;
                return Ok(ToolExecutionServiceOutcome::PreflightFailed(Box::new(
                    ended,
                )));
            }
        };
        let definition = definition.ok_or(ToolExecutionServiceError::CatalogDrift)?;
        let authorized = match self
            .transaction
            .authorize_attempt(
                prepared.session(),
                prepared.turn(),
                prepared.attempt(),
                preauthorization,
            )
            .await
        {
            Ok(ToolAttemptAuthorizationOutcome::Authorized(authorized)) => *authorized,
            Ok(ToolAttemptAuthorizationOutcome::PreauthorizationRejected { detail }) => {
                let ended = self
                    .transaction
                    .commit_preflight_error(
                        prepared.session(),
                        prepared.turn(),
                        prepared.attempt(),
                        ToolExecutionError::new(
                            ToolExecutionErrorKind::PreauthorizationRejected,
                            Some(detail),
                        ),
                    )
                    .await
                    .map_err(ToolExecutionServiceError::PreflightCommit)?;
                return Ok(ToolExecutionServiceOutcome::PreflightFailed(Box::new(
                    ended,
                )));
            }
            Err(error)
                if matches!(
                    error.operator_failure_class(),
                    OperatorFailureClass::Infrastructure {
                        commit_ambiguous: true
                    }
                ) =>
            {
                match self
                    .transaction
                    .reread_ambiguous_authorization(
                        prepared.session(),
                        prepared.turn(),
                        prepared.attempt(),
                    )
                    .await
                {
                    Ok(ToolAttemptAuthorizationStatus::Prepared(_)) => {
                        drop(dispatch_permit);
                        return Err(ToolExecutionServiceError::Authorize(error));
                    }
                    Ok(ToolAttemptAuthorizationStatus::InFlight(authorized)) => authorized,
                    Err(reread_error) => {
                        self.retained_state = Some(RetainedToolExecutionState {
                            state: RetainedToolExecutionStateKind::AuthorizationNonConsumption {
                                session: prepared.session(),
                                turn: prepared.turn(),
                                attempt: prepared.attempt(),
                                request,
                                definition,
                                result_entry_count,
                                dispatch_permit,
                            },
                        });
                        return Err(ToolExecutionServiceError::AuthorizationReread {
                            authorization_error: error,
                            reread_error,
                        });
                    }
                }
            }
            Err(error) => {
                return Err(ToolExecutionServiceError::Authorize(error));
            }
        };
        self.execute_authorized(
            request,
            definition,
            authorized,
            result_entry_count,
            dispatch_permit,
        )
        .await
    }

    async fn execute_authorized(
        &mut self,
        request: ToolRequest,
        definition: ToolDefinition,
        authorized: ToolDispatchAuthority,
        result_entry_count: usize,
        dispatch_permit: InProcessToolDispatchPermit,
    ) -> Result<
        ToolExecutionServiceOutcome,
        ToolExecutionServiceError<Transaction::Error, Executor::Error>,
    > {
        let effect_class = definition.effect_class();
        let dispatched_tool = definition.name().clone();
        let expected_correlation = authorized.correlation();
        let invocation = ToolExecutionInvocation::try_new(request, definition, authorized)
            .ok_or(ToolExecutionServiceError::CatalogDrift)?;
        report_tool_dispatch(&dispatched_tool, &expected_correlation);
        let disposition = match self.executor.execute_with_scheduling(invocation).await {
            Ok(disposition) => disposition,
            Err(error) => {
                return self
                    .classify_untrusted_executor_failure(
                        expected_correlation,
                        result_entry_count,
                        dispatch_permit,
                        UntrustedExecutorFailure::Executor(error),
                    )
                    .await;
            }
        };
        let evidence = match disposition {
            ToolExecutorDisposition::Completed(evidence) => evidence,
            ToolExecutorDisposition::DurableCompletion(completion) => {
                if completion.correlation() != expected_correlation {
                    return self
                        .classify_untrusted_executor_failure(
                            expected_correlation,
                            result_entry_count,
                            dispatch_permit,
                            UntrustedExecutorFailure::CorrelationMismatch,
                        )
                        .await;
                }
                return self
                    .reconcile_durable_completion(completion, dispatch_permit)
                    .await;
            }
            ToolExecutorDisposition::DurableChildWait(wait) => {
                if wait.correlation() != expected_correlation {
                    return self
                        .classify_untrusted_executor_failure(
                            expected_correlation,
                            result_entry_count,
                            dispatch_permit,
                            UntrustedExecutorFailure::CorrelationMismatch,
                        )
                        .await;
                }
                return self
                    .reconcile_durable_child_wait(wait, dispatch_permit)
                    .await;
            }
        };
        if evidence.correlation() != expected_correlation {
            return self
                .classify_untrusted_executor_failure(
                    expected_correlation,
                    result_entry_count,
                    dispatch_permit,
                    UntrustedExecutorFailure::CorrelationMismatch,
                )
                .await;
        }
        let observation = admit_executor_evidence(evidence, effect_class);
        report_tool_attempt(&dispatched_tool, &observation);
        self.commit_executor_observation(observation, dispatch_permit)
            .await
    }

    async fn reconcile_durable_completion(
        &mut self,
        completion: CorrelatedDurableToolCompletion,
        dispatch_permit: InProcessToolDispatchPermit,
    ) -> Result<
        ToolExecutionServiceOutcome,
        ToolExecutionServiceError<Transaction::Error, Executor::Error>,
    > {
        let attempt = completion.correlation().attempt();
        match self
            .transaction
            .reread_durable_completion(completion.correlation())
            .await
        {
            Ok(true) => Ok(ToolExecutionServiceOutcome::ObservationAlreadyCommitted(
                attempt,
            )),
            Ok(false) => Err(ToolExecutionServiceError::DurableCompletionMismatch),
            Err(error) => {
                self.retained_state = Some(RetainedToolExecutionState {
                    state: RetainedToolExecutionStateKind::DurableCompletion {
                        completion,
                        dispatch_permit,
                    },
                });
                Err(ToolExecutionServiceError::DurableCompletionReconciliation(
                    error,
                ))
            }
        }
    }

    async fn reconcile_durable_child_wait(
        &mut self,
        wait: CorrelatedDurableChildWait,
        dispatch_permit: InProcessToolDispatchPermit,
    ) -> Result<
        ToolExecutionServiceOutcome,
        ToolExecutionServiceError<Transaction::Error, Executor::Error>,
    > {
        match self.transaction.reread_durable_child_wait(wait).await {
            Ok(true) => Ok(ToolExecutionServiceOutcome::ChildWaitParked(
                wait.child_wait(),
            )),
            Ok(false) => Err(ToolExecutionServiceError::ChildWaitMismatch),
            Err(error) => {
                self.retained_state = Some(RetainedToolExecutionState {
                    state: RetainedToolExecutionStateKind::DurableChildWait {
                        wait,
                        dispatch_permit,
                    },
                });
                Err(ToolExecutionServiceError::ChildWaitReconciliation(error))
            }
        }
    }

    async fn classify_untrusted_executor_failure(
        &mut self,
        correlation: ToolAttemptDispatchCorrelation,
        result_entry_count: usize,
        dispatch_permit: InProcessToolDispatchPermit,
        failure: UntrustedExecutorFailure<Executor::Error>,
    ) -> Result<
        ToolExecutionServiceOutcome,
        ToolExecutionServiceError<Transaction::Error, Executor::Error>,
    > {
        let retained_cause = match &failure {
            UntrustedExecutorFailure::Executor(error) => {
                RetainedCrashCause::Executor(RetainedExecutorFailure {
                    failure_class: error.operator_failure_class(),
                    cause_code: error.operator_failure_cause_code(),
                })
            }
            UntrustedExecutorFailure::CorrelationMismatch => {
                RetainedCrashCause::CorrelationMismatch
            }
        };
        let classification = self
            .classify_crash_loss(
                correlation.session(),
                correlation.turn(),
                correlation.attempt(),
                result_entry_count,
                retained_cause,
                dispatch_permit,
            )
            .await;
        match (failure, classification) {
            (UntrustedExecutorFailure::Executor(executor_error), Ok(outcome)) => {
                let failure_class = executor_error.operator_failure_class();
                if is_fatal_executor_failure_class(failure_class) {
                    return Err(ToolExecutionServiceError::Executor(executor_error));
                }
                emit_contained_executor_failure(
                    correlation.session(),
                    correlation.turn(),
                    correlation.attempt(),
                    RetainedExecutorFailure {
                        failure_class,
                        cause_code: executor_error.operator_failure_cause_code(),
                    },
                );
                Ok(ToolExecutionServiceOutcome::CrashClassified(Box::new(
                    outcome,
                )))
            }
            (UntrustedExecutorFailure::CorrelationMismatch, Ok(_)) => {
                Err(ToolExecutionServiceError::ExecutorCorrelationMismatch)
            }
            (UntrustedExecutorFailure::Executor(executor_error), Err(classification_error)) => {
                Err(ToolExecutionServiceError::ExecutorCrashClassification {
                    executor_error,
                    classification_error,
                })
            }
            (UntrustedExecutorFailure::CorrelationMismatch, Err(error)) => Err(
                ToolExecutionServiceError::ExecutorCorrelationMismatchCrashClassification(error),
            ),
        }
    }

    async fn commit_executor_observation(
        &mut self,
        observation: CorrelatedToolAttemptObservation,
        dispatch_permit: InProcessToolDispatchPermit,
    ) -> Result<
        ToolExecutionServiceOutcome,
        ToolExecutionServiceError<Transaction::Error, Executor::Error>,
    > {
        match self
            .transaction
            .commit_observation(observation.clone())
            .await
        {
            Ok(ended) => Ok(ToolExecutionServiceOutcome::ObservationCommitted(Box::new(
                ended,
            ))),
            Err(error) => {
                self.retained_state = Some(RetainedToolExecutionState {
                    state: RetainedToolExecutionStateKind::Observation {
                        observation,
                        dispatch_permit,
                    },
                });
                Err(ToolExecutionServiceError::ObservationCommit(error))
            }
        }
    }

    async fn classify_crash_loss(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        result_entry_count: usize,
        cause: RetainedCrashCause,
        dispatch_permit: InProcessToolDispatchPermit,
    ) -> Result<ToolAttemptCrashOutcome, Transaction::Error> {
        loop {
            let identities = ToolCrashClosureIdentities::new(
                (0..result_entry_count)
                    .map(|_| self.ids.next_tool_semantic_entry_id())
                    .collect(),
                self.ids.next_tool_context_frontier_id(),
                FailedModelCallTurnIdentities::new(
                    self.ids.next_tool_semantic_entry_id(),
                    self.ids.next_tool_context_frontier_id(),
                ),
            );
            let ids = &mut self.ids;
            match self
                .transaction
                .classify_crash_loss(session, turn, attempt, identities, |_| {
                    ids.next_tool_turn_id()
                })
                .await
            {
                Err(error)
                    if error.operator_failure_class()
                        == OperatorFailureClass::IdentityCollision =>
                {
                    continue;
                }
                Ok(outcome) => return Ok(outcome),
                Err(error) => {
                    self.retained_state = Some(RetainedToolExecutionState {
                        state: RetainedToolExecutionStateKind::CrashClassification {
                            session,
                            turn,
                            attempt,
                            result_entry_count,
                            cause,
                            dispatch_permit,
                        },
                    });
                    return Err(error);
                }
            }
        }
    }

    async fn prepare_continuation(
        &mut self,
        batch: &ToolBatch,
    ) -> Result<
        ToolExecutionServiceOutcome,
        ToolExecutionServiceError<Transaction::Error, Executor::Error>,
    > {
        let _dispatch_permit = self.gate.acquire(batch.turn()).await;
        let Some(batch) = self
            .transaction
            .load_active_batch(batch.session(), batch.turn())
            .await
            .map_err(ToolExecutionServiceError::Load)?
        else {
            return Ok(ToolExecutionServiceOutcome::NoWork);
        };
        loop {
            let result_entries = (0..batch.requests().len())
                .map(|_| self.ids.next_tool_semantic_entry_id())
                .collect();
            let identities = ToolContinuationIdentities::new(
                result_entries,
                self.ids.next_tool_context_frontier_id(),
                self.ids.next_tool_model_call_id(),
                FailedModelCallTurnIdentities::new(
                    self.ids.next_tool_semantic_entry_id(),
                    self.ids.next_tool_context_frontier_id(),
                ),
                self.ids.next_tool_context_frontier_id(),
            );
            let ids = &mut self.ids;
            match self
                .transaction
                .prepare_continuation(
                    batch.session(),
                    batch.turn(),
                    batch.producing_call(),
                    identities,
                    |_| (ids.next_tool_semantic_entry_id(), ids.next_tool_turn_id()),
                )
                .await
            {
                Err(error)
                    if error.operator_failure_class()
                        == OperatorFailureClass::IdentityCollision =>
                {
                    continue;
                }
                Ok(PrepareToolContinuationOutcome::NoWork) => {
                    return Ok(ToolExecutionServiceOutcome::NoWork);
                }
                Ok(PrepareToolContinuationOutcome::Checkpointed(call)) => {
                    return Ok(ToolExecutionServiceOutcome::ContinuationCheckpointed(call));
                }
                Ok(PrepareToolContinuationOutcome::TargetUnavailable(failed)) => {
                    report_tool_turn_terminalization(&failed, "continuation_target_unavailable");
                    return Ok(ToolExecutionServiceOutcome::ContinuationTargetUnavailable(
                        failed,
                    ));
                }
                Ok(PrepareToolContinuationOutcome::PoolExhausted(exhausted)) => {
                    report_tool_turn_terminalization(
                        exhausted.failed(),
                        "continuation_pool_exhausted",
                    );
                    return Ok(ToolExecutionServiceOutcome::ContinuationPoolExhausted(
                        exhausted,
                    ));
                }
                Ok(PrepareToolContinuationOutcome::ContextCompactionRequired(required)) => {
                    report_tool_turn_terminalization(
                        required.failed(),
                        "continuation_context_compaction_required",
                    );
                    return Ok(
                        ToolExecutionServiceOutcome::ContinuationContextCompactionRequired(
                            required,
                        ),
                    );
                }
                Err(error) => return Err(ToolExecutionServiceError::Continuation(error)),
            }
        }
    }
}

fn admit_executor_evidence(
    evidence: CorrelatedToolExecutorEvidence,
    effect_class: ToolEffectClass,
) -> CorrelatedToolAttemptObservation {
    let observation = match evidence.evidence {
        ToolExecutorEvidence::CompletedText(value) => match ToolResultText::try_new(value) {
            Ok(result) => ToolAttemptObservation::Completed {
                result: ToolResultContent::Text(result),
            },
            Err(error) => {
                let kind = match error.failure() {
                    ToolResultTextFailure::TooLarge { .. } => {
                        ToolExecutionErrorKind::ResultTooLarge
                    }
                    ToolResultTextFailure::ContainsNull => ToolExecutionErrorKind::ExecutionFailed,
                };
                ToolAttemptObservation::KnownFailed {
                    error: ToolExecutionError::new(kind, None),
                }
            }
        },
        ToolExecutorEvidence::KnownFailed { detail } => ToolAttemptObservation::KnownFailed {
            error: ToolExecutionError::new(ToolExecutionErrorKind::ExecutionFailed, detail),
        },
        ToolExecutorEvidence::Ambiguous if effect_class == ToolEffectClass::EffectFree => {
            ToolAttemptObservation::KnownFailed {
                error: ToolExecutionError::new(ToolExecutionErrorKind::ExecutionFailed, None),
            }
        }
        ToolExecutorEvidence::Ambiguous => ToolAttemptObservation::Ambiguous,
    };
    evidence.fence.bind(observation)
}

/// The operator-visible signal one admitted tool observation warrants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolAttemptSignal {
    /// Nothing an operator needs: the attempt produced admitted content, or it
    /// parked as ambiguous, which the loop's own outcomes already carry into
    /// reconciliation.
    Silent,
    /// The attempt failed definitively, with this closed error kind.
    Failed(ToolExecutionErrorKind),
}

/// Decides what an admitted observation owes an operator.
const fn tool_attempt_signal(observation: &ToolAttemptObservation) -> ToolAttemptSignal {
    match observation {
        ToolAttemptObservation::KnownFailed { error } => ToolAttemptSignal::Failed(error.kind()),
        ToolAttemptObservation::Completed { .. } | ToolAttemptObservation::Ambiguous => {
            ToolAttemptSignal::Silent
        }
    }
}

/// Records the point at which authorized tool work leaves application control.
///
/// A durable attempt without this event is waiting to dispatch; an attempt with
/// it but no terminal evidence is stuck in an executor. Sanitization is closed
/// over the daemon-authored catalog name and minted aggregate identifiers: no
/// tool arguments, result content, credential, or adapter prose is recorded.
fn report_tool_dispatch(name: &ToolName, correlation: &ToolAttemptDispatchCorrelation) {
    tracing::info!(
        tool = name.as_str(),
        session_id = %correlation.session().as_uuid(),
        turn_id = %correlation.turn().as_uuid(),
        tool_attempt_id = %correlation.attempt().as_uuid(),
        "tool attempt dispatched"
    );
}

/// Records a terminal turn outcome that closed the tool continuation itself.
///
/// This event distinguishes a terminalized turn from one waiting on tool-loop
/// work. The caller supplies the closed outcome label, which together with the
/// daemon-minted identities cannot contain provider prose, credentials, tool
/// arguments, or conversation content.
fn report_tool_turn_terminalization(failed: &FailedModelCallTurn, terminal_outcome: &'static str) {
    tracing::info!(
        session_id = %failed.session().as_uuid(),
        turn_id = %failed.turn().as_uuid(),
        terminal_outcome,
        "turn terminalized"
    );
}

/// Records one definitively failed tool attempt for operators.
///
/// A failed attempt is otherwise resolved entirely inside the next model
/// round, so a deployment fault — an unusable credential, a code host refusing
/// every call — reaches the model and nobody else. This site rather than each
/// executor because the executors sit behind one trait and this layer already
/// holds every typed fact the event carries, so one site covers every tool
/// including the failures admission itself produces.
///
/// Sanitized by construction: the daemon-authored catalog name, two
/// daemon-minted aggregate identifiers, and the closed error kind are the only
/// fields, so no credential material, response body, tool argument, or
/// conversation content can reach telemetry. The bounded error
/// detail is deliberately omitted — executors alone decide what it says.
fn report_tool_attempt(name: &ToolName, observation: &CorrelatedToolAttemptObservation) {
    let ToolAttemptSignal::Failed(error_kind) = tool_attempt_signal(observation.observation())
    else {
        return;
    };
    let correlation = observation.correlation();
    tracing::warn!(
        tool = name.as_str(),
        ?error_kind,
        session_id = %correlation.session().as_uuid(),
        turn_id = %correlation.turn().as_uuid(),
        "tool attempt failed; the next model round observes the typed error"
    );
}

/// Selects initial approval for one proposal from frozen posture and catalog.
///
/// An explicitly configured `Delegated` posture satisfies an `AlwaysConfirm`
/// declaration; a configured `Auto` posture never does. `AlwaysConfirm` exists
/// so that a session blanket cannot silently approve the tool — see
/// [`InitialToolApproval::AlwaysConfirm`], "leave an `AlwaysConfirm` request
/// undecided despite blanket posture". A delegate judge is not a blanket but a
/// distinct decider that can still deny the request or escalate it to the user,
/// so admitting it serves that purpose rather than evading it. Admitting `Auto`
/// would instead erase the decision altogether, which is the loophole this
/// distinction keeps closed. An unconfigured `AlwaysConfirm` declaration still
/// parks for a human under either blanket posture.
pub(crate) fn initial_tool_approval(
    posture: DangerousToolAutoApproval,
    definition: Option<&ToolDefinition>,
) -> InitialToolApproval {
    let configured = definition.and_then(ToolDefinition::approval_posture);
    if definition.is_some_and(|definition| {
        definition.permission_default() == ToolPermissionDefault::AlwaysConfirm
    }) {
        return match configured {
            Some(ToolApprovalPosture::Delegated) => InitialToolApproval::Delegated,
            Some(ToolApprovalPosture::Auto | ToolApprovalPosture::Human) | None => {
                InitialToolApproval::AlwaysConfirm
            }
        };
    }
    if let Some(configured) = configured {
        return match configured {
            ToolApprovalPosture::Auto => InitialToolApproval::PolicyAuto,
            ToolApprovalPosture::Delegated => InitialToolApproval::Delegated,
            ToolApprovalPosture::Human => InitialToolApproval::Human,
        };
    }
    match posture {
        DangerousToolAutoApproval::ApproveAll => InitialToolApproval::SessionBlanket,
        DangerousToolAutoApproval::Disabled => match definition
            .map(ToolDefinition::permission_default)
            .unwrap_or(ToolPermissionDefault::Confirm)
        {
            ToolPermissionDefault::Auto => InitialToolApproval::PolicyAuto,
            ToolPermissionDefault::Confirm | ToolPermissionDefault::AlwaysConfirm => {
                InitialToolApproval::Confirm
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        num::NonZeroU64,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;
    use signalbox_domain::{
        ChildRelationshipPolicy, DelegatedSpawnRequest, DelegationAwaitRequest, DelegationEvent,
        DelegationEventOrdinal, DelegationProvenance, DelegationWaitMode, DurableCommandId,
        ResolvedContextFrontierReconstitutionInput, SessionDelegationReconstitutionInput,
        ToolApprovalResolutionReconstitutionInput, ToolAttemptReconstitutionInput,
        ToolAttemptReconstitutionState, ToolBatchPhaseReconstitutionInput,
        ToolBatchReconstitutionInput, ToolDecisionSource, ToolDispatchGeneration,
        ToolRequestOrdinal, ToolRequestReconstitutionInput,
    };
    use uuid::Uuid;

    fn tool_name(value: &str) -> ToolName {
        ToolName::try_new(value.to_owned()).expect("fixture name is valid")
    }

    fn schema() -> ToolInputSchema {
        ToolInputSchema::try_new(String::from(
            r#"{"type":"object","properties":{"value":{"type":"string"}}}"#,
        ))
        .expect("fixture schema is valid")
    }

    fn definition(
        name: &str,
        permission: ToolPermissionDefault,
        effect: ToolEffectClass,
    ) -> ToolDefinition {
        ToolDefinition::new(
            tool_name(name),
            format!("Runs {name}."),
            schema(),
            permission,
            effect,
        )
    }

    fn confirmation_definition(name: &str) -> ToolDefinition {
        definition(
            name,
            ToolPermissionDefault::Confirm,
            ToolEffectClass::EffectFree,
        )
    }

    fn request_with_seed(arguments: &str, seed: u128) -> ToolRequest {
        ToolRequestReconstitutionInput::new(
            ToolRequestId::from_uuid(Uuid::from_u128(seed + 4)),
            SessionId::from_uuid(Uuid::from_u128(seed + 1)),
            TurnId::from_uuid(Uuid::from_u128(seed + 2)),
            ModelCallId::from_uuid(Uuid::from_u128(seed + 3)),
            ToolRequestOrdinal::from_u32(0),
            tool_name("known"),
            NormalizedToolArguments::try_from_provider_text(arguments.to_owned())
                .expect("fixture arguments fit the admission bound"),
        )
        .into_request()
    }

    fn batch_with_attempt_state(
        arguments: &str,
        effect: ToolEffectClass,
        state: ToolAttemptReconstitutionState,
        seed: u128,
    ) -> (ToolBatch, ToolAttemptId) {
        let request = request_with_seed(arguments, seed);
        let attempt_id = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 6));
        let turn_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 5));
        let approval = ToolApprovalResolutionReconstitutionInput::policy_auto(request.id())
            .reconstitute()
            .expect("implemented policy provenance reconstitutes");
        let attempt = ToolAttemptReconstitutionInput::new(
            attempt_id,
            request.id(),
            request.session(),
            request.turn(),
            turn_attempt,
            effect,
            ToolDispatchGeneration::first(),
            state,
        )
        .reconstitute()
        .expect("tool attempt fixture reconstitutes");
        let snapshot = ResolvedContextFrontierReconstitutionInput::new(
            request.session(),
            signalbox_domain::ContextFrontierId::from_uuid(Uuid::from_u128(seed + 7)),
            Vec::new(),
        )
        .reconstitute()
        .expect("empty fixture snapshot is valid");
        let batch = ToolBatchReconstitutionInput::new(
            request.session(),
            request.turn(),
            request.producing_call(),
            snapshot,
            vec![request],
            vec![approval],
            vec![attempt],
            ToolBatchPhaseReconstitutionInput::Executing { turn_attempt },
        )
        .reconstitute()
        .expect("tool fixture batch is correlated");
        (batch, attempt_id)
    }

    fn prepared_batch(arguments: &str, effect: ToolEffectClass) -> (ToolBatch, ToolAttemptId) {
        batch_with_attempt_state(
            arguments,
            effect,
            ToolAttemptReconstitutionState::Prepared,
            0,
        )
    }

    fn foreground_wait_for(request: &ToolRequest) -> DelegationWait {
        let spawning = ToolRequestReconstitutionInput::new(
            ToolRequestId::from_uuid(Uuid::from_u128(90)),
            request.session(),
            request.turn(),
            request.producing_call(),
            ToolRequestOrdinal::from_u32(1),
            tool_name("spawn_session"),
            NormalizedToolArguments::try_from_provider_text(String::from(
                r#"{"relationship":{"kind":"background"},"task":"inspect"}"#,
            ))
            .expect("spawn arguments are canonical"),
        )
        .into_request();
        let spawning = DelegatedSpawnRequest::parse(
            spawning,
            String::from("inspect"),
            ChildRelationshipPolicy::Background,
        )
        .expect("spawn request is canonical");
        let child = SessionId::from_uuid(Uuid::from_u128(91));
        let relation = SessionDelegationReconstitutionInput::new(
            spawning.clone(),
            child,
            TurnId::from_uuid(Uuid::from_u128(92)),
            vec![DelegationEvent::Spawned {
                ordinal: DelegationEventOrdinal::new(NonZeroU64::MIN),
                provenance: DelegationProvenance::from_spawn(&spawning),
            }],
        )
        .reconstitute()
        .expect("spawn event reconstitutes the relation");
        let awaiting = ToolRequestReconstitutionInput::new(
            request.id(),
            request.session(),
            request.turn(),
            request.producing_call(),
            ToolRequestOrdinal::from_u32(0),
            tool_name("await_session"),
            NormalizedToolArguments::try_from_provider_text(format!(
                r#"{{"child_session_id":"{}","mode":"foreground"}}"#,
                child.as_uuid()
            ))
            .expect("await arguments are canonical"),
        )
        .into_request();
        let awaiting =
            DelegationAwaitRequest::parse(awaiting, child, DelegationWaitMode::Foreground)
                .expect("await request is canonical");
        DelegationWait::reconstitute(&relation, &awaiting)
            .expect("await request names the relation")
    }

    #[track_caller]
    fn assert_child_wait_reconciliation_error(
        error: ToolExecutionServiceError<FakeError, FakeError>,
    ) {
        assert!(matches!(
            error,
            ToolExecutionServiceError::ChildWaitReconciliation(FakeError::Ordinary)
        ));
    }

    #[track_caller]
    fn assert_durable_completion_mismatch(error: ToolExecutionServiceError<FakeError, FakeError>) {
        assert!(matches!(
            error,
            ToolExecutionServiceError::DurableCompletionMismatch
        ));
    }

    #[track_caller]
    fn assert_durable_completion_reconciliation_error(
        error: ToolExecutionServiceError<FakeError, FakeError>,
    ) {
        assert!(matches!(
            error,
            ToolExecutionServiceError::DurableCompletionReconciliation(FakeError::Ordinary)
        ));
    }

    #[track_caller]
    fn current_attempt_fixture(batch: &ToolBatch) -> signalbox_domain::CurrentToolAttempt {
        match batch.attempt(batch.requests()[0].id()) {
            Some(signalbox_domain::ReconstitutedToolAttempt::Current(current)) => current.clone(),
            _ => panic!("fixture has one current attempt"),
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeError {
        Ordinary,
        Infrastructure,
        Corruption,
        CommitAmbiguous,
    }

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(match self {
                Self::Ordinary => "fake tool-loop failure",
                Self::Infrastructure => "fake infrastructure tool-loop failure",
                Self::Corruption => "fake corrupt tool-loop state",
                Self::CommitAmbiguous => "fake commit-ambiguous tool-loop failure",
            })
        }
    }

    #[test]
    fn durable_completion_reconciliation_exposes_transaction_source() {
        let error =
            ToolExecutionServiceError::<FakeError, FakeError>::DurableCompletionReconciliation(
                FakeError::Ordinary,
            );

        assert_eq!(
            error.source().map(ToString::to_string),
            Some(String::from("fake tool-loop failure"))
        );
    }

    #[test]
    fn child_wait_reconciliation_exposes_transaction_source() {
        let error = ToolExecutionServiceError::<FakeError, FakeError>::ChildWaitReconciliation(
            FakeError::Ordinary,
        );

        assert_eq!(
            error.source().map(ToString::to_string),
            Some(String::from("fake tool-loop failure"))
        );
    }

    impl Error for FakeError {}

    impl ClassifyOperatorFailure for FakeError {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            match self {
                Self::Ordinary => OperatorFailureClass::CallerOrHubBug,
                Self::Infrastructure => OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                },
                Self::Corruption => OperatorFailureClass::FailClosedCorruption,
                Self::CommitAmbiguous => OperatorFailureClass::Infrastructure {
                    commit_ambiguous: true,
                },
            }
        }
    }

    struct FailingOverrideTransaction {
        calls: Arc<AtomicUsize>,
    }

    impl OverrideDeniedToolRequestTransaction for FailingOverrideTransaction {
        type Error = FakeError;

        async fn override_denied(
            &mut self,
            _command: OverrideDeniedToolRequest,
        ) -> Result<PreparedOverrideDeniedToolRequest, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(FakeError::Ordinary)
        }
    }

    #[tokio::test]
    async fn override_service_returns_transaction_failure_without_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let transaction = FailingOverrideTransaction {
            calls: Arc::clone(&calls),
        };
        let mut service = OverrideDeniedToolRequestService::new(transaction);
        let command = OverrideDeniedToolRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(1)),
            SessionId::from_uuid(Uuid::from_u128(2)),
            ToolRequestId::from_uuid(Uuid::from_u128(3)),
        )
        .expect("fixture command identity is admitted");

        let error = service
            .execute(command)
            .await
            .expect_err("the transaction failure is returned");

        assert_eq!(error, FakeError::Ordinary);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    struct FakeTransaction {
        batch: ToolBatch,
        prepared: signalbox_domain::CurrentToolAttempt,
        events: Arc<Mutex<Vec<&'static str>>>,
        ambiguous_authorization: bool,
        authorization_committed: bool,
        commit_failures: usize,
        committed: bool,
        load_results: VecDeque<Option<ToolBatch>>,
        allow_crash_classification: bool,
    }

    impl ToolExecutionTransaction for FakeTransaction {
        type Error = FakeError;

        async fn load_active_batch(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
        ) -> Result<Option<ToolBatch>, Self::Error> {
            Ok(self
                .load_results
                .pop_front()
                .unwrap_or_else(|| Some(self.batch.clone())))
        }

        async fn resume_child_wait(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            _continuation: TurnAttemptId,
        ) -> Result<bool, Self::Error> {
            panic!("ordinary tool fixture never resumes a child wait")
        }

        async fn prepare_next_attempt(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            _attempt: ToolAttemptId,
            _effect_class: ToolEffectClass,
        ) -> Result<Option<signalbox_domain::CurrentToolAttempt>, Self::Error> {
            panic!("prepared fixture never creates another attempt")
        }

        async fn authorize_attempt(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            attempt: ToolAttemptId,
            _preauthorization: ToolPreauthorization,
        ) -> Result<ToolAttemptAuthorizationOutcome, Self::Error> {
            self.events.lock().expect("event lock").push("authorize");
            if self.ambiguous_authorization {
                self.authorization_committed = true;
                return Err(FakeError::CommitAmbiguous);
            }
            self.batch
                .authorize_dispatch(attempt)
                .map(Box::new)
                .map(ToolAttemptAuthorizationOutcome::Authorized)
                .map_err(|_| FakeError::Ordinary)
        }

        async fn reread_ambiguous_authorization(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            attempt: ToolAttemptId,
        ) -> Result<ToolAttemptAuthorizationStatus, Self::Error> {
            self.events.lock().expect("event lock").push("reread");
            if self.authorization_committed {
                Ok(ToolAttemptAuthorizationStatus::InFlight(
                    self.batch
                        .authorize_dispatch(attempt)
                        .map_err(|_| FakeError::Ordinary)?,
                ))
            } else {
                Ok(ToolAttemptAuthorizationStatus::Prepared(
                    self.prepared.clone(),
                ))
            }
        }

        async fn commit_preflight_error(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            _attempt: ToolAttemptId,
            error: ToolExecutionError,
        ) -> Result<EndedToolAttempt, Self::Error> {
            self.events.lock().expect("event lock").push("preflight");
            self.prepared
                .clone()
                .end_preflight_error(error)
                .map_err(|_| FakeError::Ordinary)
        }

        async fn commit_observation(
            &mut self,
            observation: CorrelatedToolAttemptObservation,
        ) -> Result<EndedToolAttempt, Self::Error> {
            self.events.lock().expect("event lock").push("commit");
            if self.commit_failures > 0 {
                self.commit_failures -= 1;
                return Err(FakeError::Ordinary);
            }
            let authorized = self
                .batch
                .authorize_attempt(self.prepared.attempt())
                .map_err(|_| FakeError::Ordinary)?;
            let ended = authorized
                .into_parts()
                .0
                .apply_terminal_observation(observation)
                .map_err(|_| FakeError::Ordinary)?;
            self.committed = true;
            Ok(ended)
        }

        async fn reread_observation(
            &mut self,
            _observation: &CorrelatedToolAttemptObservation,
        ) -> Result<RetainedToolAttemptObservationStatus, Self::Error> {
            Ok(if self.committed {
                RetainedToolAttemptObservationStatus::AlreadyCommitted
            } else {
                RetainedToolAttemptObservationStatus::Pending
            })
        }

        async fn reread_durable_completion(
            &mut self,
            _correlation: ToolAttemptDispatchCorrelation,
        ) -> Result<bool, Self::Error> {
            if self.commit_failures > 0 {
                self.commit_failures -= 1;
                return Err(FakeError::Ordinary);
            }
            Ok(self.committed)
        }

        async fn reread_durable_child_wait(
            &mut self,
            _wait: CorrelatedDurableChildWait,
        ) -> Result<bool, Self::Error> {
            self.events
                .lock()
                .expect("event lock")
                .push("reread_child_wait");
            if self.commit_failures > 0 {
                self.commit_failures -= 1;
                return Err(FakeError::Ordinary);
            }
            Ok(true)
        }

        async fn classify_crash_loss<NextTurn>(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            _attempt: ToolAttemptId,
            _identities: ToolCrashClosureIdentities,
            _next_turn: NextTurn,
        ) -> Result<ToolAttemptCrashOutcome, Self::Error>
        where
            NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        {
            if !self.allow_crash_classification {
                panic!("prepared fixture is not a restart loss");
            }
            self.events.lock().expect("event lock").push("classify");
            if self.commit_failures > 0 {
                self.commit_failures -= 1;
                return Err(FakeError::Ordinary);
            }
            Ok(self.prepared.clone().classify_crash_loss())
        }

        async fn prepare_continuation<NextSteering>(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            _producing_call: ModelCallId,
            _identities: ToolContinuationIdentities,
            _next_steering: NextSteering,
        ) -> Result<PrepareToolContinuationOutcome, Self::Error>
        where
            NextSteering: FnMut(AcceptedInputId) -> (SemanticTranscriptEntryId, TurnId) + Send,
        {
            panic!("prepared fixture is not ready for continuation")
        }
    }

    struct FixedIds {
        attempts: VecDeque<ToolAttemptId>,
        entries: VecDeque<SemanticTranscriptEntryId>,
        frontiers: VecDeque<signalbox_domain::ContextFrontierId>,
        calls: VecDeque<ModelCallId>,
        turns: VecDeque<TurnId>,
    }

    impl FixedIds {
        fn new() -> Self {
            Self {
                attempts: [20]
                    .map(|value| ToolAttemptId::from_uuid(Uuid::from_u128(value)))
                    .into(),
                entries: (21..30)
                    .map(|value| SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value)))
                    .collect(),
                frontiers: (30..36)
                    .map(|value| {
                        signalbox_domain::ContextFrontierId::from_uuid(Uuid::from_u128(value))
                    })
                    .collect(),
                calls: [40]
                    .map(|value| ModelCallId::from_uuid(Uuid::from_u128(value)))
                    .into(),
                turns: (41..50)
                    .map(|value| TurnId::from_uuid(Uuid::from_u128(value)))
                    .collect(),
            }
        }
    }

    impl ToolExecutionIdGenerator for FixedIds {
        fn next_tool_turn_attempt_id(&mut self) -> TurnAttemptId {
            TurnAttemptId::from_uuid(Uuid::from_u128(19))
        }

        fn next_tool_attempt_id(&mut self) -> ToolAttemptId {
            self.attempts.pop_front().expect("fixture attempt identity")
        }

        fn next_tool_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
            self.entries.pop_front().expect("fixture entry identity")
        }

        fn next_tool_context_frontier_id(&mut self) -> signalbox_domain::ContextFrontierId {
            self.frontiers
                .pop_front()
                .expect("fixture frontier identity")
        }

        fn next_tool_model_call_id(&mut self) -> ModelCallId {
            self.calls.pop_front().expect("fixture call identity")
        }

        fn next_tool_turn_id(&mut self) -> TurnId {
            self.turns.pop_front().expect("fixture turn identity")
        }
    }

    struct RecordingExecutor {
        events: Arc<Mutex<Vec<&'static str>>>,
        calls: usize,
    }

    struct FixedEvidenceExecutor {
        evidence: Option<CorrelatedToolExecutorEvidence>,
    }

    struct FailingExecutor {
        events: Arc<Mutex<Vec<&'static str>>>,
        error: FakeError,
    }

    struct DurableWaitExecutor {
        wait: DelegationWait,
    }

    struct DurableCompletionExecutor {
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    impl ToolExecutor for DurableCompletionExecutor {
        type Error = FakeError;

        async fn execute(
            &mut self,
            _invocation: ToolExecutionInvocation,
        ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
            panic!("scheduling-aware execution handles durable completion")
        }

        async fn execute_with_scheduling(
            &mut self,
            invocation: ToolExecutionInvocation,
        ) -> Result<ToolExecutorDisposition, Self::Error> {
            self.events
                .lock()
                .expect("event lock")
                .push("execute_durable_completion");
            Ok(ToolExecutorDisposition::DurableCompletion(
                invocation.durable_completion(),
            ))
        }
    }

    impl ToolExecutor for DurableWaitExecutor {
        type Error = FakeError;

        async fn execute(
            &mut self,
            _invocation: ToolExecutionInvocation,
        ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
            panic!("scheduling-aware execution handles durable child waits")
        }

        async fn execute_with_scheduling(
            &mut self,
            invocation: ToolExecutionInvocation,
        ) -> Result<ToolExecutorDisposition, Self::Error> {
            let wait = CorrelatedDurableChildWait::try_new(invocation.correlation(), self.wait)
                .expect("fixture wait matches the invocation");
            Ok(ToolExecutorDisposition::DurableChildWait(wait))
        }
    }

    impl ToolExecutor for FailingExecutor {
        type Error = FakeError;

        async fn execute(
            &mut self,
            _invocation: ToolExecutionInvocation,
        ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
            self.events.lock().expect("event lock").push("execute");
            Err(self.error)
        }
    }

    impl ToolExecutor for FixedEvidenceExecutor {
        type Error = FakeError;

        async fn execute(
            &mut self,
            _invocation: ToolExecutionInvocation,
        ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
            Ok(self
                .evidence
                .take()
                .expect("fixture supplies one executor observation"))
        }
    }

    impl ToolExecutor for RecordingExecutor {
        type Error = FakeError;

        async fn execute(
            &mut self,
            invocation: ToolExecutionInvocation,
        ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
            self.calls += 1;
            self.events.lock().expect("event lock").push("execute");
            Ok(
                invocation.bind(ToolExecutorEvidence::CompletedText(String::from(
                    "exact result",
                ))),
            )
        }
    }

    struct OneShotCatalog {
        definition: ToolDefinition,
        definition_reads: Arc<AtomicUsize>,
    }

    impl ToolCatalog for OneShotCatalog {
        fn definitions(&self) -> Box<[ToolDefinition]> {
            vec![self.definition.clone()].into_boxed_slice()
        }

        fn definition(&self, name: &ToolName) -> Option<ToolDefinition> {
            (name == self.definition.name()
                && self.definition_reads.fetch_add(1, Ordering::SeqCst) == 0)
                .then(|| self.definition.clone())
        }

        fn validate_arguments(
            &self,
            name: &ToolName,
            _arguments: &NormalizedToolArguments,
        ) -> Result<(), ToolCatalogValidationFailure> {
            if name == self.definition.name() {
                Ok(())
            } else {
                Err(ToolCatalogValidationFailure::UnknownTool)
            }
        }
    }

    #[test]
    fn absent_posture_preserves_legacy_confirmation() {
        const SUBJECT_TOOL: &str = "subject";

        assert_eq!(
            initial_tool_approval(
                DangerousToolAutoApproval::Disabled,
                Some(&confirmation_definition(SUBJECT_TOOL)),
            ),
            InitialToolApproval::Confirm
        );
    }

    #[test]
    fn delegated_posture_overrides_confirmation_policy() {
        const SUBJECT_TOOL: &str = "subject";
        let delegated = confirmation_definition(SUBJECT_TOOL)
            .with_approval_posture(ToolApprovalPosture::Delegated);

        assert_eq!(
            initial_tool_approval(DangerousToolAutoApproval::Disabled, Some(&delegated)),
            InitialToolApproval::Delegated
        );
    }

    #[test]
    fn human_posture_overrides_dangerous_session_blanket() {
        const SUBJECT_TOOL: &str = "subject";
        let human =
            confirmation_definition(SUBJECT_TOOL).with_approval_posture(ToolApprovalPosture::Human);

        assert_eq!(
            initial_tool_approval(DangerousToolAutoApproval::ApproveAll, Some(&human)),
            InitialToolApproval::Human
        );
    }

    #[test]
    fn auto_posture_overrides_confirmation_policy() {
        const SUBJECT_TOOL: &str = "subject";
        let automatic =
            confirmation_definition(SUBJECT_TOOL).with_approval_posture(ToolApprovalPosture::Auto);

        assert_eq!(
            initial_tool_approval(DangerousToolAutoApproval::Disabled, Some(&automatic)),
            InitialToolApproval::PolicyAuto
        );
    }

    /// registry automation records policy provenance, while blanket
    /// automation remains explicitly distinct from user agency.
    #[test]
    fn initial_policy_preserves_automation_provenance() {
        let automatic = definition(
            "automatic",
            ToolPermissionDefault::Auto,
            ToolEffectClass::EffectFree,
        );

        assert_eq!(
            initial_tool_approval(DangerousToolAutoApproval::Disabled, Some(&automatic)),
            InitialToolApproval::PolicyAuto
        );
        assert_eq!(
            initial_tool_approval(DangerousToolAutoApproval::ApproveAll, Some(&automatic)),
            InitialToolApproval::SessionBlanket
        );
        assert_eq!(
            initial_tool_approval(DangerousToolAutoApproval::Disabled, None),
            InitialToolApproval::Confirm
        );
        assert_ne!(
            ToolDecisionSource::PolicyAuto,
            ToolDecisionSource::UserCommand
        );
    }

    fn always_confirm_definition(name: &str) -> ToolDefinition {
        definition(
            name,
            ToolPermissionDefault::AlwaysConfirm,
            ToolEffectClass::ExternalEffect,
        )
    }

    /// With no posture configured, an `AlwaysConfirm` declaration parks for a
    /// human under either blanket posture. This is the default `unsandboxed_exec`
    /// behavior, which the configured-posture admission below must not change.
    #[test]
    fn always_confirm_is_not_overridden_by_the_dangerous_session_blanket() {
        let explicit = always_confirm_definition("explicit");

        assert_eq!(
            initial_tool_approval(DangerousToolAutoApproval::Disabled, Some(&explicit)),
            InitialToolApproval::AlwaysConfirm
        );
        assert_eq!(
            initial_tool_approval(DangerousToolAutoApproval::ApproveAll, Some(&explicit)),
            InitialToolApproval::AlwaysConfirm
        );
    }

    /// An explicitly configured `Delegated` posture satisfies `AlwaysConfirm`.
    /// The declaration exists so that a session blanket cannot silently approve
    /// the tool; a delegate judge is not a blanket but a distinct decider that
    /// can still deny the request or escalate it to the user, so routing to it
    /// serves that purpose rather than evading it.
    #[test]
    fn configured_delegated_posture_satisfies_always_confirm() {
        let delegated = always_confirm_definition("explicit")
            .with_approval_posture(ToolApprovalPosture::Delegated);

        assert_eq!(
            initial_tool_approval(DangerousToolAutoApproval::Disabled, Some(&delegated)),
            InitialToolApproval::Delegated
        );
        assert_eq!(
            initial_tool_approval(DangerousToolAutoApproval::ApproveAll, Some(&delegated)),
            InitialToolApproval::Delegated
        );
    }

    /// A configured `Auto` posture must never satisfy `AlwaysConfirm`. Unlike a
    /// delegate judge it erases the decision entirely, which is exactly the
    /// silent automatic approval the declaration refuses; admitting it would be
    /// the loophole that admitting `Delegated` is not.
    #[test]
    fn configured_auto_posture_never_satisfies_always_confirm() {
        let automatic =
            always_confirm_definition("explicit").with_approval_posture(ToolApprovalPosture::Auto);

        assert_eq!(
            initial_tool_approval(DangerousToolAutoApproval::Disabled, Some(&automatic)),
            InitialToolApproval::AlwaysConfirm
        );
        assert_eq!(
            initial_tool_approval(DangerousToolAutoApproval::ApproveAll, Some(&automatic)),
            InitialToolApproval::AlwaysConfirm
        );
    }

    /// A configured `Human` posture keeps the stricter `AlwaysConfirm` outcome
    /// rather than the plain human park, since both await the same user and the
    /// declaration is the more specific fact.
    #[test]
    fn configured_human_posture_leaves_always_confirm_parked() {
        let human =
            always_confirm_definition("explicit").with_approval_posture(ToolApprovalPosture::Human);

        assert_eq!(
            initial_tool_approval(DangerousToolAutoApproval::Disabled, Some(&human)),
            InitialToolApproval::AlwaysConfirm
        );
        assert_eq!(
            initial_tool_approval(DangerousToolAutoApproval::ApproveAll, Some(&human)),
            InitialToolApproval::AlwaysConfirm
        );
    }

    #[test]
    fn compiled_catalog_rejects_duplicate_names() {
        let first = CompiledTool::new(
            definition(
                "same",
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            |_: &NormalizedToolArguments| Ok(()),
        );
        let second = CompiledTool::new(
            definition(
                "same",
                ToolPermissionDefault::Confirm,
                ToolEffectClass::ExternalEffect,
            ),
            |_: &NormalizedToolArguments| Ok(()),
        );

        let error = CompiledToolCatalog::try_new([first, second])
            .expect_err("duplicate dispatch names are ambiguous");
        assert_eq!(error.name(), &tool_name("same"));
    }

    #[test]
    fn schema_is_canonical_and_object_shaped() {
        let schema =
            ToolInputSchema::try_new(String::from(r#"{ "type": "object", "properties": {} }"#))
                .expect("object schema is admitted");
        assert_eq!(schema.as_str(), r#"{"properties":{},"type":"object"}"#);
        assert_eq!(
            ToolInputSchema::try_new(String::from("true"))
                .expect_err("tool arguments require an object schema")
                .failure(),
            ToolInputSchemaFailure::NotObject
        );
    }

    /// an approved unknown request closes with typed
    /// preflight evidence before authorization or executor entry.
    #[tokio::test]
    async fn unknown_tool_never_crosses_executor_boundary() {
        let (batch, attempt) = prepared_batch("{}", ToolEffectClass::EffectFree);
        let events = Arc::new(Mutex::new(Vec::new()));
        let transaction = FakeTransaction {
            prepared: match batch.attempt(batch.requests()[0].id()) {
                Some(signalbox_domain::ReconstitutedToolAttempt::Current(current)) => {
                    current.clone()
                }
                _ => panic!("fixture has one prepared attempt"),
            },
            batch: batch.clone(),
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 0,
            committed: false,
            load_results: VecDeque::new(),
            allow_crash_classification: false,
        };
        let executor = RecordingExecutor {
            events: Arc::clone(&events),
            calls: 0,
        };
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            NoToolCatalog,
            executor,
            InProcessToolDispatchGate::default(),
        );

        let outcome = service
            .execute(batch.session(), batch.turn())
            .await
            .expect("unknown-tool evidence commits");
        let ToolExecutionServiceOutcome::PreflightFailed(ended) = outcome else {
            panic!("unknown tool must close at preflight");
        };
        assert_eq!(ended.attempt(), attempt);
        assert!(matches!(
            ended.end(),
            signalbox_domain::ToolAttemptEnd::KnownFailed { error }
                if error.kind() == ToolExecutionErrorKind::UnknownTool
        ));
        let (_, _, _, executor, _, _) = service.into_parts();
        assert_eq!(executor.calls, 0);
        assert_eq!(*events.lock().expect("event lock"), ["preflight"]);
    }

    /// What one execution against a possibly drifted catalog produced.
    struct ExecutionUnderCatalog {
        result:
            Result<ToolExecutionServiceOutcome, ToolExecutionServiceError<FakeError, FakeError>>,
        executor_calls: usize,
        transaction_events: Vec<&'static str>,
    }

    /// The two effect classes one drift execution puts in disagreement.
    ///
    /// A struct rather than two positional arguments: both fields are
    /// `ToolEffectClass`, the two tests below deliberately pass them in
    /// opposite orders, and a transposition would compile while silently
    /// reversing which side is the drifted one — leaving the assertion
    /// describing a case it no longer covers.
    struct CatalogDrift {
        /// What the durable authorization froze when the attempt was prepared.
        prepared_effect: ToolEffectClass,
        /// What the live, rebuilt catalog declares for the same tool.
        catalog_effect: ToolEffectClass,
    }

    /// Prepares one attempt whose durable authorization froze
    /// `drift.prepared_effect`, then executes it against a live catalog
    /// declaring the same tool `drift.catalog_effect` — the daemon-restart
    /// shape in which a rebuilt catalog can disagree with a parked approval.
    async fn execute_under_catalog_effect_class(drift: CatalogDrift) -> ExecutionUnderCatalog {
        let CatalogDrift {
            prepared_effect,
            catalog_effect,
        } = drift;
        let (batch, _) = prepared_batch("{}", prepared_effect);
        let events = Arc::new(Mutex::new(Vec::new()));
        let transaction = FakeTransaction {
            prepared: current_attempt_fixture(&batch),
            batch: batch.clone(),
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 0,
            committed: false,
            load_results: VecDeque::new(),
            allow_crash_classification: false,
        };
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            definition("known", ToolPermissionDefault::Auto, catalog_effect),
            |_: &NormalizedToolArguments| Ok(()),
        )])
        .expect("one declaration is unambiguous");
        let executor = RecordingExecutor {
            events: Arc::clone(&events),
            calls: 0,
        };
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );
        let result = service.execute(batch.session(), batch.turn()).await;
        let (_, _, _, executor, _, _) = service.into_parts();
        let transaction_events = events.lock().expect("event lock").clone();
        ExecutionUnderCatalog {
            result,
            executor_calls: executor.calls,
            transaction_events,
        }
    }

    /// The effect class frozen at preparation is what the approval gate
    /// authorized, so a catalog that now declares another class stops the call
    /// before authorization rather than running it under a class no approval
    /// ever covered.
    #[tokio::test]
    async fn drifted_catalog_effect_class_never_reaches_authorization_or_the_executor() {
        let drifted = execute_under_catalog_effect_class(CatalogDrift {
            prepared_effect: ToolEffectClass::EffectFree,
            catalog_effect: ToolEffectClass::ExternalEffect,
        })
        .await;

        assert!(matches!(
            drifted.result,
            Err(ToolExecutionServiceError::CatalogDrift)
        ));
        assert_eq!(drifted.executor_calls, 0);
        assert!(
            drifted.transaction_events.is_empty(),
            "drift stops before authorization, preflight commit, and observation commit"
        );
    }

    /// Catalog drift is an operator-visible caller-or-hub bug carrying its own
    /// stable cause token, so a wedged turn is attributable without formatting
    /// adapter detail.
    #[tokio::test]
    async fn drifted_catalog_effect_class_reports_its_declared_operator_failure() {
        let drifted = execute_under_catalog_effect_class(CatalogDrift {
            prepared_effect: ToolEffectClass::ExternalEffect,
            catalog_effect: ToolEffectClass::EffectFree,
        })
        .await;
        let error = drifted
            .result
            .expect_err("a prepared call cannot execute under a drifted effect class");

        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::CallerOrHubBug
        );
        assert_eq!(error.operator_failure_cause_code(), "tool_catalog_drift");
    }

    /// durable authorization precedes the
    /// executor, and only its exact correlation can commit returned evidence.
    #[tokio::test]
    async fn executor_evidence_is_fenced_and_committed_in_order() {
        let (batch, attempt) = prepared_batch("{}", ToolEffectClass::EffectFree);
        let events = Arc::new(Mutex::new(Vec::new()));
        let prepared = match batch.attempt(batch.requests()[0].id()) {
            Some(signalbox_domain::ReconstitutedToolAttempt::Current(current)) => current.clone(),
            _ => panic!("fixture has one prepared attempt"),
        };
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared,
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 0,
            committed: false,
            load_results: VecDeque::new(),
            allow_crash_classification: false,
        };
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            definition(
                "known",
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            |_: &NormalizedToolArguments| Ok(()),
        )])
        .expect("one declaration is unambiguous");
        let executor = RecordingExecutor {
            events: Arc::clone(&events),
            calls: 0,
        };
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );

        let outcome = service
            .execute(batch.session(), batch.turn())
            .await
            .expect("fenced evidence commits");
        let ToolExecutionServiceOutcome::ObservationCommitted(ended) = outcome else {
            panic!("valid request must execute");
        };
        assert_eq!(ended.attempt(), attempt);
        assert!(matches!(
            ended.end(),
            signalbox_domain::ToolAttemptEnd::Completed {
                result: ToolResultContent::Text(text)
            } if text.as_str() == "exact result"
        ));
        assert_eq!(
            *events.lock().expect("event lock"),
            ["authorize", "execute", "commit"]
        );
    }

    /// S17: a scheduling-aware executor's durable
    /// foreground wait is reread before the service accepts the parked turn.
    #[tokio::test]
    async fn s17_durable_child_wait_is_authenticated_without_second_observation() {
        let (batch, _) = prepared_batch("{}", ToolEffectClass::EffectFree);
        let events = Arc::new(Mutex::new(Vec::new()));
        let prepared = current_attempt_fixture(&batch);
        let wait = foreground_wait_for(&batch.requests()[0]);
        let expected = wait
            .foreground_subject()
            .expect("fixture wait is foreground");
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared,
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 0,
            committed: false,
            load_results: VecDeque::new(),
            allow_crash_classification: false,
        };
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            definition(
                "known",
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            |_: &NormalizedToolArguments| Ok(()),
        )])
        .expect("one declaration is unambiguous");
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            catalog,
            DurableWaitExecutor { wait },
            InProcessToolDispatchGate::default(),
        );

        let outcome = service
            .execute(batch.session(), batch.turn())
            .await
            .expect("durable wait evidence is authenticated");

        assert_eq!(
            outcome,
            ToolExecutionServiceOutcome::ChildWaitParked(expected)
        );
        assert_eq!(
            *events.lock().expect("event lock"),
            ["authorize", "reread_child_wait"]
        );
    }

    /// S17: terminal evidence committed atomically with a
    /// tool effect is authenticated and never sent through a second commit.
    #[tokio::test]
    async fn s17_durable_completion_is_authenticated_without_second_commit() {
        let (batch, attempt) = prepared_batch("{}", ToolEffectClass::EffectFree);
        let events = Arc::new(Mutex::new(Vec::new()));
        let prepared = current_attempt_fixture(&batch);
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared,
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 0,
            committed: true,
            load_results: VecDeque::new(),
            allow_crash_classification: false,
        };
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            definition(
                "known",
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            |_: &NormalizedToolArguments| Ok(()),
        )])
        .expect("one declaration is unambiguous");
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            catalog,
            DurableCompletionExecutor {
                events: Arc::clone(&events),
            },
            InProcessToolDispatchGate::default(),
        );

        let outcome = service
            .execute(batch.session(), batch.turn())
            .await
            .expect("durable completion evidence is authenticated");

        assert_eq!(
            outcome,
            ToolExecutionServiceOutcome::ObservationAlreadyCommitted(attempt)
        );
        assert_eq!(
            *events.lock().expect("event lock"),
            ["authorize", "execute_durable_completion"]
        );
    }

    /// a durable-completion claim cannot authorize an
    /// attempt that storage still reports as pending.
    #[tokio::test]
    async fn durable_completion_fails_closed_when_not_committed() {
        let (batch, _) = prepared_batch("{}", ToolEffectClass::EffectFree);
        let events = Arc::new(Mutex::new(Vec::new()));
        let prepared = current_attempt_fixture(&batch);
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared,
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 0,
            committed: false,
            load_results: VecDeque::new(),
            allow_crash_classification: false,
        };
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            definition(
                "known",
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            |_: &NormalizedToolArguments| Ok(()),
        )])
        .expect("one declaration is unambiguous");
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            catalog,
            DurableCompletionExecutor {
                events: Arc::clone(&events),
            },
            InProcessToolDispatchGate::default(),
        );

        let error = service
            .execute(batch.session(), batch.turn())
            .await
            .expect_err("pending storage cannot authenticate durable completion");

        assert_durable_completion_mismatch(error);
        assert_eq!(
            *events.lock().expect("event lock"),
            ["authorize", "execute_durable_completion"]
        );
    }

    /// a failed durable-completion reread retains
    /// the exact evidence and dispatch permit, then retries only authentication.
    #[tokio::test]
    async fn durable_completion_retries_only_authentication() {
        let (batch, attempt) = prepared_batch("{}", ToolEffectClass::EffectFree);
        let events = Arc::new(Mutex::new(Vec::new()));
        let prepared = current_attempt_fixture(&batch);
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared,
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 1,
            committed: true,
            load_results: VecDeque::new(),
            allow_crash_classification: false,
        };
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            definition(
                "known",
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            |_: &NormalizedToolArguments| Ok(()),
        )])
        .expect("one declaration is unambiguous");
        let gate = InProcessToolDispatchGate::default();
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            catalog,
            DurableCompletionExecutor {
                events: Arc::clone(&events),
            },
            gate.clone(),
        );

        let first = service
            .execute(batch.session(), batch.turn())
            .await
            .expect_err("first durable-completion reread fails transiently");
        let gate_blocked = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            gate.acquire(batch.turn()),
        )
        .await;
        let retried = service
            .execute(batch.session(), batch.turn())
            .await
            .expect("retained durable completion authenticates on retry");

        assert_durable_completion_reconciliation_error(first);
        assert!(service.retained_state().is_none());
        assert!(gate_blocked.is_err());
        assert_eq!(
            retried,
            ToolExecutionServiceOutcome::ObservationAlreadyCommitted(attempt)
        );
        assert_eq!(
            *events.lock().expect("event lock"),
            ["authorize", "execute_durable_completion"]
        );
    }

    /// S17: a transient durable-wait reread failure keeps
    /// the exact evidence and dispatch permit for same-incarnation retry.
    #[tokio::test]
    async fn s17_durable_child_wait_retries_only_its_authentication() {
        let (batch, _) = prepared_batch("{}", ToolEffectClass::EffectFree);
        let events = Arc::new(Mutex::new(Vec::new()));
        let prepared = current_attempt_fixture(&batch);
        let wait = foreground_wait_for(&batch.requests()[0]);
        let expected = wait
            .foreground_subject()
            .expect("fixture wait is foreground");
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared,
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 1,
            committed: false,
            load_results: VecDeque::new(),
            allow_crash_classification: false,
        };
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            definition(
                "known",
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            |_: &NormalizedToolArguments| Ok(()),
        )])
        .expect("one declaration is unambiguous");
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            catalog,
            DurableWaitExecutor { wait },
            InProcessToolDispatchGate::default(),
        );

        let first = service
            .execute(batch.session(), batch.turn())
            .await
            .expect_err("first wait authentication fails transiently");
        let retried = service
            .execute(batch.session(), batch.turn())
            .await
            .expect("retained wait authentication retries");

        assert_child_wait_reconciliation_error(first);
        assert_eq!(
            retried,
            ToolExecutionServiceOutcome::ChildWaitParked(expected)
        );
        assert_eq!(
            *events.lock().expect("event lock"),
            ["authorize", "reread_child_wait", "reread_child_wait"]
        );
    }

    /// an infrastructure executor failure cannot
    /// release the interrupt gate while its durable attempt remains in flight,
    /// and its committed crash classification contains the failure for this
    /// turn.
    #[tokio::test]
    async fn infrastructure_executor_failure_classifies_before_gate_release() {
        let (batch, _) = prepared_batch("{}", ToolEffectClass::EffectFree);
        let events = Arc::new(Mutex::new(Vec::new()));
        let prepared = current_attempt_fixture(&batch);
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared,
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 0,
            committed: false,
            load_results: VecDeque::new(),
            allow_crash_classification: true,
        };
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            definition(
                "known",
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            |_: &NormalizedToolArguments| Ok(()),
        )])
        .expect("one declaration is unambiguous");
        let gate = InProcessToolDispatchGate::default();
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            catalog,
            FailingExecutor {
                events: Arc::clone(&events),
                error: FakeError::Infrastructure,
            },
            gate.clone(),
        );

        assert!(matches!(
            service
                .execute(batch.session(), batch.turn())
                .await
                .expect("committed crash classification contains the executor failure"),
            ToolExecutionServiceOutcome::CrashClassified(_)
        ));
        assert!(service.retained_state().is_none());
        let _released = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            gate.acquire(batch.turn()),
        )
        .await
        .expect("durable crash classification releases the interrupt gate");
        assert_eq!(
            *events.lock().expect("event lock"),
            ["authorize", "execute", "classify"]
        );
    }

    #[test]
    fn fatal_executor_failure_classes_are_not_containable() {
        assert!(is_fatal_executor_failure_class(
            OperatorFailureClass::FailClosedCorruption
        ));
        assert!(is_fatal_executor_failure_class(
            OperatorFailureClass::CallerOrHubBug
        ));
        assert!(!is_fatal_executor_failure_class(
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            }
        ));
        assert!(!is_fatal_executor_failure_class(
            OperatorFailureClass::IdentityCollision
        ));
    }

    /// crash classification closes an authorized
    /// attempt before a fail-closed executor error remains fatal to the daemon.
    #[tokio::test]
    async fn corrupt_executor_failure_remains_fatal_after_classification() {
        let (batch, _) = prepared_batch("{}", ToolEffectClass::EffectFree);
        let events = Arc::new(Mutex::new(Vec::new()));
        let prepared = current_attempt_fixture(&batch);
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared,
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 0,
            committed: false,
            load_results: VecDeque::new(),
            allow_crash_classification: true,
        };
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            definition(
                "known",
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            |_: &NormalizedToolArguments| Ok(()),
        )])
        .expect("one declaration is unambiguous");
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            catalog,
            FailingExecutor {
                events: Arc::clone(&events),
                error: FakeError::Corruption,
            },
            InProcessToolDispatchGate::default(),
        );

        assert!(matches!(
            service.execute(batch.session(), batch.turn()).await,
            Err(ToolExecutionServiceError::Executor(FakeError::Corruption))
        ));
        assert!(service.retained_state().is_none());
        assert_eq!(
            *events.lock().expect("event lock"),
            ["authorize", "execute", "classify"]
        );
    }

    /// failed executor crash classification
    /// retains the exact gate permit until a later pass commits closure.
    #[tokio::test]
    async fn failed_executor_classification_retains_gate() {
        let (batch, _) = prepared_batch("{}", ToolEffectClass::EffectFree);
        let events = Arc::new(Mutex::new(Vec::new()));
        let prepared = current_attempt_fixture(&batch);
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared,
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 1,
            committed: false,
            load_results: VecDeque::new(),
            allow_crash_classification: true,
        };
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            definition(
                "known",
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            |_: &NormalizedToolArguments| Ok(()),
        )])
        .expect("one declaration is unambiguous");
        let gate = InProcessToolDispatchGate::default();
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            catalog,
            FailingExecutor {
                events: Arc::clone(&events),
                error: FakeError::Infrastructure,
            },
            gate.clone(),
        );

        assert!(matches!(
            service.execute(batch.session(), batch.turn()).await,
            Err(ToolExecutionServiceError::ExecutorCrashClassification {
                executor_error: FakeError::Infrastructure,
                classification_error: FakeError::Ordinary,
            })
        ));
        assert_eq!(
            service
                .retained_state()
                .and_then(RetainedToolExecutionState::crash_executor_failure),
            Some(RetainedExecutorFailure {
                failure_class: OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                },
                cause_code: "infrastructure",
            })
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                gate.acquire(batch.turn())
            )
            .await
            .is_err(),
            "failed classification keeps interrupts behind the exact permit"
        );
        assert!(matches!(
            service
                .execute(batch.session(), batch.turn())
                .await
                .expect("retained classification commits"),
            ToolExecutionServiceOutcome::CrashClassified(_)
        ));
        let _released = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            gate.acquire(batch.turn()),
        )
        .await
        .expect("durable closure releases the interrupt gate");
        assert_eq!(
            *events.lock().expect("event lock"),
            ["authorize", "execute", "classify", "classify"]
        );
    }

    /// a failed classification retry retains the
    /// original fatal executor class after durable closure succeeds.
    #[tokio::test]
    async fn recovered_classification_preserves_fatal_executor_failure() {
        let (batch, _) = prepared_batch("{}", ToolEffectClass::EffectFree);
        let events = Arc::new(Mutex::new(Vec::new()));
        let prepared = current_attempt_fixture(&batch);
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared,
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 1,
            committed: false,
            load_results: VecDeque::new(),
            allow_crash_classification: true,
        };
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            definition(
                "known",
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            |_: &NormalizedToolArguments| Ok(()),
        )])
        .expect("one declaration is unambiguous");
        let gate = InProcessToolDispatchGate::default();
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            catalog,
            FailingExecutor {
                events: Arc::clone(&events),
                error: FakeError::Corruption,
            },
            gate.clone(),
        );

        assert!(matches!(
            service.execute(batch.session(), batch.turn()).await,
            Err(ToolExecutionServiceError::ExecutorCrashClassification {
                executor_error: FakeError::Corruption,
                classification_error: FakeError::Ordinary,
            })
        ));
        assert!(service.retained_state().is_some());
        let recovered = service
            .execute(batch.session(), batch.turn())
            .await
            .expect_err("fatal executor failure survives classification recovery");

        assert_eq!(
            recovered.operator_failure_class(),
            OperatorFailureClass::FailClosedCorruption
        );
        assert_eq!(
            recovered.operator_failure_cause_code(),
            "durable_state_corruption"
        );
        assert!(matches!(
            recovered,
            ToolExecutionServiceError::RecoveredFatalExecutorFailure {
                failure_class: OperatorFailureClass::FailClosedCorruption,
                cause_code: "durable_state_corruption",
            }
        ));
        assert!(service.retained_state().is_none());
        let _released = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            gate.acquire(batch.turn()),
        )
        .await
        .expect("durable closure releases the interrupt gate");
        assert_eq!(
            *events.lock().expect("event lock"),
            ["authorize", "execute", "classify", "classify"]
        );
    }

    /// a correlation mismatch retained across failed crash
    /// classification resurfaces only after durable closure releases the gate.
    #[tokio::test]
    async fn recovered_classification_preserves_correlation_mismatch() {
        let (batch, _) = prepared_batch("{}", ToolEffectClass::EffectFree);
        let events = Arc::new(Mutex::new(Vec::new()));
        let prepared = current_attempt_fixture(&batch);
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared,
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 1,
            committed: false,
            load_results: VecDeque::new(),
            allow_crash_classification: true,
        };
        let definition = definition(
            "known",
            ToolPermissionDefault::Auto,
            ToolEffectClass::EffectFree,
        );
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            definition.clone(),
            |_: &NormalizedToolArguments| Ok(()),
        )])
        .expect("one declaration is unambiguous");
        let (foreign_batch, foreign_attempt) = batch_with_attempt_state(
            "{}",
            ToolEffectClass::EffectFree,
            ToolAttemptReconstitutionState::Prepared,
            100,
        );
        let foreign_authorized = foreign_batch
            .authorize_dispatch(foreign_attempt)
            .expect("foreign fixture authorizes");
        let foreign_invocation = ToolExecutionInvocation::try_new(
            foreign_batch.requests()[0].clone(),
            definition,
            foreign_authorized,
        )
        .expect("foreign invocation is internally correlated");
        let executor = FixedEvidenceExecutor {
            evidence: Some(foreign_invocation.bind(ToolExecutorEvidence::CompletedText(
                String::from("foreign result"),
            ))),
        };
        let gate = InProcessToolDispatchGate::default();
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            catalog,
            executor,
            gate.clone(),
        );

        assert!(matches!(
            service.execute(batch.session(), batch.turn()).await,
            Err(
                ToolExecutionServiceError::ExecutorCorrelationMismatchCrashClassification(
                    FakeError::Ordinary
                )
            )
        ));
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                gate.acquire(batch.turn())
            )
            .await
            .is_err(),
            "failed classification keeps the exact dispatch gate"
        );
        assert!(matches!(
            service.execute(batch.session(), batch.turn()).await,
            Err(ToolExecutionServiceError::ExecutorCorrelationMismatch)
        ));
        let _released = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            gate.acquire(batch.turn()),
        )
        .await
        .expect("durable crash classification releases the interrupt gate");
        assert_eq!(
            *events.lock().expect("event lock"),
            ["authorize", "classify", "classify"]
        );
    }

    /// a prepared execution hint is revalidated after the
    /// dispatch gate, so a winning interrupt becomes ordinary no-work.
    #[tokio::test]
    async fn stale_prepared_hint_after_gate_is_no_work() {
        let (batch, _) = prepared_batch("{}", ToolEffectClass::EffectFree);
        let events = Arc::new(Mutex::new(Vec::new()));
        let prepared = match batch.attempt(batch.requests()[0].id()) {
            Some(signalbox_domain::ReconstitutedToolAttempt::Current(current)) => current.clone(),
            _ => panic!("fixture has one prepared attempt"),
        };
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared,
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 0,
            committed: false,
            load_results: [Some(batch.clone()), None].into(),
            allow_crash_classification: false,
        };
        let executor = RecordingExecutor {
            events: Arc::clone(&events),
            calls: 0,
        };
        let gate = InProcessToolDispatchGate::default();
        let blocking_permit = gate.acquire(batch.turn()).await;
        let mut service =
            ToolExecutionService::new(FixedIds::new(), transaction, NoToolCatalog, executor, gate);
        let execution = service.execute(batch.session(), batch.turn());
        tokio::pin!(execution);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut execution)
                .await
                .is_err(),
            "prepared work waits behind the dispatch gate"
        );
        drop(blocking_permit);
        assert_eq!(
            execution.await.expect("stale hint is not an error"),
            ToolExecutionServiceOutcome::NoWork
        );
        assert!(events.lock().expect("event lock").is_empty());
    }

    /// an all-resolved continuation hint is revalidated
    /// under the dispatch gate, so a winning interrupt is ordinary no-work.
    #[tokio::test]
    async fn vanished_continuation_batch_is_no_work() {
        let (batch, _) = batch_with_attempt_state(
            "{}",
            ToolEffectClass::EffectFree,
            ToolAttemptReconstitutionState::Ended(signalbox_domain::ToolAttemptEnd::KnownFailed {
                error: ToolExecutionError::new(ToolExecutionErrorKind::ExecutionFailed, None),
            }),
            0,
        );
        let (prepared_batch, _) = prepared_batch("{}", ToolEffectClass::EffectFree);
        let prepared = match prepared_batch.attempt(prepared_batch.requests()[0].id()) {
            Some(signalbox_domain::ReconstitutedToolAttempt::Current(current)) => current.clone(),
            _ => panic!("fixture has one prepared attempt"),
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared,
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 0,
            committed: false,
            load_results: [Some(batch.clone()), None].into(),
            allow_crash_classification: false,
        };
        let executor = RecordingExecutor {
            events: Arc::clone(&events),
            calls: 0,
        };
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            NoToolCatalog,
            executor,
            InProcessToolDispatchGate::default(),
        );

        assert_eq!(
            service
                .execute(batch.session(), batch.turn())
                .await
                .expect("a winning interrupt makes continuation stale"),
            ToolExecutionServiceOutcome::NoWork
        );
        assert!(events.lock().expect("event lock").is_empty());
    }

    /// an in-flight attempt is not classified as
    /// prior-process loss until the same-turn dispatch permit is available and
    /// authoritative state has been reloaded.
    #[tokio::test]
    async fn crash_classification_waits_for_dispatch_gate() {
        let (batch, _) = batch_with_attempt_state(
            "{}",
            ToolEffectClass::EffectFree,
            ToolAttemptReconstitutionState::InFlight,
            0,
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let current = match batch.attempt(batch.requests()[0].id()) {
            Some(signalbox_domain::ReconstitutedToolAttempt::Current(current)) => current.clone(),
            _ => panic!("fixture has one in-flight attempt"),
        };
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared: current,
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 0,
            committed: false,
            load_results: VecDeque::new(),
            allow_crash_classification: true,
        };
        let executor = RecordingExecutor {
            events: Arc::clone(&events),
            calls: 0,
        };
        let gate = InProcessToolDispatchGate::default();
        let blocking_permit = gate.acquire(batch.turn()).await;
        let mut service =
            ToolExecutionService::new(FixedIds::new(), transaction, NoToolCatalog, executor, gate);
        let execution = service.execute(batch.session(), batch.turn());
        tokio::pin!(execution);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut execution)
                .await
                .is_err(),
            "live executor ownership must block crash classification"
        );
        assert!(events.lock().expect("event lock").is_empty());
        drop(blocking_permit);
        assert!(matches!(
            execution
                .await
                .expect("released gate permits classification"),
            ToolExecutionServiceOutcome::CrashClassified(_)
        ));
        assert_eq!(*events.lock().expect("event lock"), ["classify"]);
    }

    #[track_caller]
    fn assert_ambiguity_admission(effect_class: ToolEffectClass, expected: ToolAttemptObservation) {
        let (batch, _) = prepared_batch("{}", effect_class);
        let current = match batch.attempt(batch.requests()[0].id()) {
            Some(signalbox_domain::ReconstitutedToolAttempt::Current(current)) => current,
            _ => panic!("fixture has one prepared attempt"),
        };
        let authorized = batch
            .authorize_dispatch(current.attempt())
            .expect("prepared fixture authorizes exactly once");
        let expected_correlation = authorized.correlation();
        let invocation = ToolExecutionInvocation::try_new(
            batch.requests()[0].clone(),
            definition("known", ToolPermissionDefault::Auto, effect_class),
            authorized,
        )
        .expect("fixture invocation matches durable authority");
        let observation = admit_executor_evidence(
            invocation.bind(ToolExecutorEvidence::Ambiguous),
            effect_class,
        );

        assert_eq!(observation.observation(), &expected);
        assert_eq!(observation.correlation(), &expected_correlation);
    }

    /// effect-free ambiguity becomes a known failure.
    #[test]
    fn effect_free_ambiguity_becomes_known_failure() {
        assert_ambiguity_admission(
            ToolEffectClass::EffectFree,
            ToolAttemptObservation::KnownFailed {
                error: ToolExecutionError::new(ToolExecutionErrorKind::ExecutionFailed, None),
            },
        );
    }

    /// external-effect ambiguity retains its recovery distinction.
    #[test]
    fn external_effect_ambiguity_is_preserved() {
        assert_ambiguity_admission(
            ToolEffectClass::ExternalEffect,
            ToolAttemptObservation::Ambiguous,
        );
    }

    /// The smallest executor result that exceeds the domain's 1 MiB
    /// `ToolResultText` admission bound; any larger result is admitted
    /// identically.
    const OVERSIZED_RESULT_BYTES: usize = 1024 * 1024 + 1;

    /// Admits one executor-completed text through the shared admission seam and
    /// returns the durable observation the hub would commit for it.
    #[track_caller]
    fn completed_text_admission(text: String) -> CorrelatedToolAttemptObservation {
        let effect_class = ToolEffectClass::EffectFree;
        let (batch, _) = prepared_batch("{}", effect_class);
        let current = match batch.attempt(batch.requests()[0].id()) {
            Some(signalbox_domain::ReconstitutedToolAttempt::Current(current)) => current,
            _ => panic!("fixture has one prepared attempt"),
        };
        let authorized = batch
            .authorize_dispatch(current.attempt())
            .expect("prepared fixture authorizes exactly once");
        let invocation = ToolExecutionInvocation::try_new(
            batch.requests()[0].clone(),
            definition("known", ToolPermissionDefault::Auto, effect_class),
            authorized,
        )
        .expect("fixture invocation matches durable authority");

        admit_executor_evidence(
            invocation.bind(ToolExecutorEvidence::CompletedText(text)),
            effect_class,
        )
    }

    /// S15: a result past the admission bound is replaced by the
    /// typed `ResultTooLarge` error. The observation compared here is the whole
    /// value handed to the commit boundary, so equality with a detail-less
    /// typed failure is also the proof that no oversized byte survives into it.
    #[test]
    fn s15_oversized_result_is_replaced_by_result_too_large() {
        let observation = completed_text_admission("r".repeat(OVERSIZED_RESULT_BYTES));

        assert_eq!(
            observation.observation(),
            &ToolAttemptObservation::KnownFailed {
                error: ToolExecutionError::new(ToolExecutionErrorKind::ResultTooLarge, None),
            }
        );
    }

    /// S15: a result carrying U+0000 is admitted as a detail-less
    /// `ExecutionFailed`. The tool-loop specification names a replacement kind
    /// for the size bound only, so this test pins the implemented mapping for
    /// the null-bearing arm rather than a specified one.
    #[test]
    fn s15_result_containing_null_is_replaced_by_execution_failed() {
        let observation = completed_text_admission(String::from("head\0tail"));

        assert_eq!(
            observation.observation(),
            &ToolAttemptObservation::KnownFailed {
                error: ToolExecutionError::new(ToolExecutionErrorKind::ExecutionFailed, None),
            }
        );
    }

    /// A definitively failed attempt is the one admitted observation an
    /// operator cannot otherwise see, so it is the one that signals, carrying
    /// the closed error kind that says which failure it was.
    #[test]
    fn failed_attempt_signals_its_error_kind() {
        let observation = ToolAttemptObservation::KnownFailed {
            error: ToolExecutionError::new(ToolExecutionErrorKind::ExecutionFailed, None),
        };

        assert_eq!(
            tool_attempt_signal(&observation),
            ToolAttemptSignal::Failed(ToolExecutionErrorKind::ExecutionFailed)
        );
    }

    /// A completed attempt is ordinary progress; signalling it would turn every
    /// working tool round into operator noise.
    #[test]
    fn completed_attempt_is_silent() {
        let observation = ToolAttemptObservation::Completed {
            result: ToolResultContent::Text(
                ToolResultText::try_new(String::from("result"))
                    .expect("fixture result is admitted"),
            ),
        };

        assert_eq!(tool_attempt_signal(&observation), ToolAttemptSignal::Silent);
    }

    /// An ambiguous external effect is not a definitive failure: the loop parks
    /// it for reconciliation and its own outcome carries it, so this site stays
    /// quiet rather than reporting an outcome nobody has established.
    #[test]
    fn ambiguous_attempt_is_silent() {
        assert_eq!(
            tool_attempt_signal(&ToolAttemptObservation::Ambiguous),
            ToolAttemptSignal::Silent
        );
    }

    /// S15: the substitution is what the hub durably commits — the
    /// ended attempt carries the typed `ResultTooLarge` failure, so oversized
    /// executor bytes never become durable result evidence.
    #[tokio::test]
    async fn s15_committed_oversized_result_ends_the_attempt_known_failed() {
        let effect_class = ToolEffectClass::EffectFree;
        let (batch, _) = prepared_batch("{}", effect_class);
        let events = Arc::new(Mutex::new(Vec::new()));
        let prepared = current_attempt_fixture(&batch);
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared: prepared.clone(),
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 0,
            committed: false,
            load_results: VecDeque::new(),
            allow_crash_classification: false,
        };
        let definition = definition("known", ToolPermissionDefault::Auto, effect_class);
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            definition.clone(),
            |_: &NormalizedToolArguments| Ok(()),
        )])
        .expect("one declaration is unambiguous");
        let authorized = batch
            .authorize_dispatch(prepared.attempt())
            .expect("prepared fixture authorizes exactly once");
        let invocation =
            ToolExecutionInvocation::try_new(batch.requests()[0].clone(), definition, authorized)
                .expect("fixture invocation matches durable authority");
        let executor = FixedEvidenceExecutor {
            evidence: Some(invocation.bind(ToolExecutorEvidence::CompletedText(
                "r".repeat(OVERSIZED_RESULT_BYTES),
            ))),
        };
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );

        let outcome = service
            .execute(batch.session(), batch.turn())
            .await
            .expect("an oversized result is an ordinary typed failure, not an error");
        let ToolExecutionServiceOutcome::ObservationCommitted(ended) = outcome else {
            panic!("an admitted observation commits");
        };

        assert_eq!(
            ended.end(),
            &signalbox_domain::ToolAttemptEnd::KnownFailed {
                error: ToolExecutionError::new(ToolExecutionErrorKind::ResultTooLarge, None),
            }
        );
        assert_eq!(*events.lock().expect("event lock"), ["authorize", "commit"]);
    }

    /// the definition selected by successful preflight is
    /// the exact same-incarnation declaration carried across authorization.
    #[tokio::test]
    async fn authorization_uses_preflight_definition_snapshot() {
        let (batch, _) = prepared_batch("{}", ToolEffectClass::EffectFree);
        let events = Arc::new(Mutex::new(Vec::new()));
        let prepared = match batch.attempt(batch.requests()[0].id()) {
            Some(signalbox_domain::ReconstitutedToolAttempt::Current(current)) => current.clone(),
            _ => panic!("fixture has one prepared attempt"),
        };
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared,
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 0,
            committed: false,
            load_results: VecDeque::new(),
            allow_crash_classification: false,
        };
        let definition_reads = Arc::new(AtomicUsize::new(0));
        let catalog = OneShotCatalog {
            definition: definition(
                "known",
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            definition_reads: Arc::clone(&definition_reads),
        };
        let executor = RecordingExecutor {
            events: Arc::clone(&events),
            calls: 0,
        };
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );

        assert!(matches!(
            service.execute(batch.session(), batch.turn()).await,
            Ok(ToolExecutionServiceOutcome::ObservationCommitted(_))
        ));
        assert_eq!(definition_reads.load(Ordering::SeqCst), 1);
        assert_eq!(
            *events.lock().expect("event lock"),
            ["authorize", "execute", "commit"]
        );
    }

    /// a lost authorization acknowledgement is reread
    /// while the dispatch gate remains held, and committed authority enters
    /// the executor exactly once.
    #[tokio::test]
    async fn ambiguous_authorization_resumes_committed_fence() {
        let (batch, attempt) = prepared_batch("{}", ToolEffectClass::EffectFree);
        let events = Arc::new(Mutex::new(Vec::new()));
        let prepared = match batch.attempt(batch.requests()[0].id()) {
            Some(signalbox_domain::ReconstitutedToolAttempt::Current(current)) => current.clone(),
            _ => panic!("fixture has one prepared attempt"),
        };
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared,
            events: Arc::clone(&events),
            ambiguous_authorization: true,
            authorization_committed: false,
            commit_failures: 0,
            committed: false,
            load_results: VecDeque::new(),
            allow_crash_classification: false,
        };
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            definition(
                "known",
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            |_: &NormalizedToolArguments| Ok(()),
        )])
        .expect("one declaration is unambiguous");
        let executor = RecordingExecutor {
            events: Arc::clone(&events),
            calls: 0,
        };
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );

        let outcome = service
            .execute(batch.session(), batch.turn())
            .await
            .expect("committed ambiguous authorization resumes");
        let ToolExecutionServiceOutcome::ObservationCommitted(ended) = outcome else {
            panic!("resumed authority must commit executor evidence");
        };
        assert_eq!(ended.attempt(), attempt);
        assert_eq!(
            *events.lock().expect("event lock"),
            ["authorize", "reread", "execute", "commit"]
        );
    }

    /// a failed result commit retains exact executor
    /// evidence and retries only that commit after an authoritative reread.
    #[tokio::test]
    async fn failed_commit_does_not_repeat_executor_work() {
        let (batch, _) = prepared_batch("{}", ToolEffectClass::EffectFree);
        let events = Arc::new(Mutex::new(Vec::new()));
        let prepared = match batch.attempt(batch.requests()[0].id()) {
            Some(signalbox_domain::ReconstitutedToolAttempt::Current(current)) => current.clone(),
            _ => panic!("fixture has one prepared attempt"),
        };
        let transaction = FakeTransaction {
            batch: batch.clone(),
            prepared,
            events: Arc::clone(&events),
            ambiguous_authorization: false,
            authorization_committed: false,
            commit_failures: 1,
            committed: false,
            load_results: VecDeque::new(),
            allow_crash_classification: false,
        };
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            definition(
                "known",
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            |_: &NormalizedToolArguments| Ok(()),
        )])
        .expect("one declaration is unambiguous");
        let executor = RecordingExecutor {
            events: Arc::clone(&events),
            calls: 0,
        };
        let gate = InProcessToolDispatchGate::default();
        let mut service = ToolExecutionService::new(
            FixedIds::new(),
            transaction,
            catalog,
            executor,
            gate.clone(),
        );

        assert!(matches!(
            service.execute(batch.session(), batch.turn()).await,
            Err(ToolExecutionServiceError::ObservationCommit(
                FakeError::Ordinary
            ))
        ));
        assert!(service.retained_state().is_some());
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                gate.acquire(batch.turn())
            )
            .await
            .is_err(),
            "retained executor evidence must keep interrupts behind its dispatch permit"
        );
        assert!(matches!(
            service
                .execute(batch.session(), batch.turn())
                .await
                .expect("retained observation recommits"),
            ToolExecutionServiceOutcome::ObservationCommitted(_)
        ));
        let _released = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            gate.acquire(batch.turn()),
        )
        .await
        .expect("committed evidence releases the dispatch permit");

        let (_, _, _, executor, _, retained) = service.into_parts();
        assert_eq!(executor.calls, 1);
        assert!(retained.is_none());
        assert_eq!(
            *events.lock().expect("event lock"),
            ["authorize", "execute", "commit", "commit"]
        );
    }
}
