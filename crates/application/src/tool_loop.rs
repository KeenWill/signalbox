//! Application orchestration for tool approval, execution, and continuation.
//!
//! `docs/spec/tool-loop.md` owns the behavior. The application selects
//! catalog policy, mints every durable identity candidate, keeps executor work
//! outside transactions, and submits only correlated evidence to persistence.

use std::{collections::BTreeMap, error::Error, fmt, future::Future, sync::Arc};

#[cfg(test)]
use signalbox_domain::AcceptedInputId;
use signalbox_domain::{
    AuthorizedToolAttempt, CorrelatedToolAttemptObservation, CurrentToolAttemptState,
    DangerousToolAutoApproval, DecideToolRequest, EndedToolAttempt, FailedModelCallTurn,
    FailedModelCallTurnIdentities, InitialToolApproval, ModelCallId, NormalizedToolArguments,
    PreparedDecideToolRequest, SemanticTranscriptEntryId, SessionId, ToolArgumentsKind,
    ToolAttemptCrashOutcome, ToolAttemptDispatchCorrelation, ToolAttemptId, ToolAttemptObservation,
    ToolBatch, ToolBatchPhase, ToolEffectClass, ToolExecutionError, ToolExecutionErrorDetail,
    ToolExecutionErrorKind, ToolName, ToolPermissionDefault, ToolRequest, ToolRequestId,
    ToolResultContent, ToolResultText, ToolResultTextFailure, TurnAttemptId, TurnId,
};

use crate::{
    ClassifyOperatorFailure, DecideToolRequestTransaction, InProcessToolDispatchGate,
    InProcessToolDispatchPermit, OperatorFailureClass, PrepareToolContinuationOutcome,
    RetainedToolAttemptObservationStatus, ToolAttemptAuthorizationStatus,
    ToolContinuationIdentities, ToolCrashClosureIdentities, ToolExecutionTransaction,
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

/// Exact checked content and authorization supplied to one executor effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolExecutionInvocation {
    request: ToolRequest,
    definition: ToolDefinition,
    correlation: ToolAttemptDispatchCorrelation,
}

impl ToolExecutionInvocation {
    fn try_new(
        request: ToolRequest,
        definition: ToolDefinition,
        authorized: &AuthorizedToolAttempt,
    ) -> Option<Self> {
        let correlation = authorized.correlation();
        (request.id() == correlation.request()
            && request.session() == correlation.session()
            && request.turn() == correlation.turn()
            && request.name() == definition.name()
            && authorized.attempt().effect_class() == definition.effect_class())
        .then_some(Self {
            request,
            definition,
            correlation,
        })
    }

    /// Borrows the immutable request content authority.
    pub const fn request(&self) -> &ToolRequest {
        &self.request
    }

    /// Borrows the exact declaration selected by preflight.
    pub const fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    /// Returns the complete durable dispatch fence.
    pub const fn correlation(&self) -> ToolAttemptDispatchCorrelation {
        self.correlation
    }

    /// Binds returned executor evidence to the exact dispatch fence.
    pub fn bind(self, evidence: ToolExecutorEvidence) -> CorrelatedToolExecutorEvidence {
        CorrelatedToolExecutorEvidence {
            correlation: self.correlation,
            evidence,
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
    correlation: ToolAttemptDispatchCorrelation,
    evidence: ToolExecutorEvidence,
}

impl CorrelatedToolExecutorEvidence {
    /// Returns the executor-supplied correlation.
    pub const fn correlation(&self) -> ToolAttemptDispatchCorrelation {
        self.correlation
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
}

/// Supplies UUIDv7 candidates for approval progression.
pub trait ToolApprovalIdGenerator {
    /// Generates a fresh continuation turn-attempt candidate.
    fn next_tool_turn_attempt_id(&mut self) -> TurnAttemptId;
}

/// Supplies UUIDv7 candidates for tool dispatch and continuation.
pub trait ToolExecutionIdGenerator {
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

/// Application service for one durable owner approval/denial command.
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
        dispatch_permit: InProcessToolDispatchPermit,
    },
    Observation {
        observation: CorrelatedToolAttemptObservation,
        dispatch_permit: InProcessToolDispatchPermit,
    },
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
                },
            )
            .field("holds_dispatch_permit", &true)
            .finish()
    }
}

/// One completed stage of serialized tool execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolExecutionServiceOutcome {
    /// No active tool batch matches the hint.
    NoWork,
    /// The batch remains parked on its earliest undecided request.
    AwaitingApproval(ToolRequestId),
    /// Exact ambiguity remains parked for owner recovery.
    AwaitingRecovery(ToolAttemptId),
    /// A fresh attempt checkpoint committed; execution waits for another pass.
    AttemptCheckpointed(ToolAttemptId),
    /// Pure preflight closed one attempt with typed error evidence.
    PreflightFailed(Box<EndedToolAttempt>),
    /// One executor observation committed durably.
    ObservationCommitted(Box<EndedToolAttempt>),
    /// The retained executor observation was already represented durably.
    ObservationAlreadyCommitted(ToolAttemptId),
    /// A prior-process live attempt was classified without retry.
    CrashClassified(Box<ToolAttemptCrashOutcome>),
    /// The all-resolved continuation call committed atomically.
    ContinuationCheckpointed(ModelCallId),
    /// Continuation target resolution closed the turn atomically.
    ContinuationTargetUnavailable(Box<FailedModelCallTurn>),
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
    /// Executor evidence could not commit.
    ObservationCommit(TransactionError),
    /// Retained executor evidence could not be reconciled with durable state.
    ObservationReconciliation(TransactionError),
    /// Crash classification failed.
    CrashClassification(TransactionError),
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
            Self::ObservationCommit(error) => {
                write!(formatter, "tool observation commit failed: {error}")
            }
            Self::ObservationReconciliation(error) => {
                write!(formatter, "tool observation reconciliation failed: {error}")
            }
            Self::CrashClassification(error) => {
                write!(formatter, "tool crash classification failed: {error}")
            }
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
            | Self::CrashClassification(error)
            | Self::Continuation(error) => error.operator_failure_class(),
            Self::AuthorizationReread { reread_error, .. } => reread_error.operator_failure_class(),
            Self::Executor(error) => error.operator_failure_class(),
            Self::CatalogDrift => OperatorFailureClass::CallerOrHubBug,
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
    Executor: ToolExecutor,
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
                            .execute_authorized(request, definition, authorized, dispatch_permit)
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
                    CurrentToolAttemptState::InFlight => loop {
                        let identities = ToolCrashClosureIdentities::new(
                            (0..batch.requests().len())
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
                            .classify_crash_loss(
                                current.session(),
                                current.turn(),
                                current.attempt(),
                                identities,
                                |_| ids.next_tool_turn_id(),
                            )
                            .await
                        {
                            Err(error)
                                if error.operator_failure_class()
                                    == OperatorFailureClass::IdentityCollision =>
                            {
                                continue;
                            }
                            Ok(outcome) => {
                                break Ok(ToolExecutionServiceOutcome::CrashClassified(Box::new(
                                    outcome,
                                )));
                            }
                            Err(error) => {
                                break Err(ToolExecutionServiceError::CrashClassification(error));
                            }
                        }
                    },
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
        let definition = definition.ok_or(ToolExecutionServiceError::CatalogDrift)?;
        let dispatch_permit = self.gate.acquire(prepared.turn()).await;
        let authorized = match self
            .transaction
            .authorize_attempt(prepared.session(), prepared.turn(), prepared.attempt())
            .await
        {
            Ok(authorized) => authorized,
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
        self.execute_authorized(request, definition, authorized, dispatch_permit)
            .await
    }

    async fn execute_authorized(
        &mut self,
        request: ToolRequest,
        definition: ToolDefinition,
        authorized: AuthorizedToolAttempt,
        dispatch_permit: InProcessToolDispatchPermit,
    ) -> Result<
        ToolExecutionServiceOutcome,
        ToolExecutionServiceError<Transaction::Error, Executor::Error>,
    > {
        let effect_class = definition.effect_class();
        let invocation = ToolExecutionInvocation::try_new(request, definition, &authorized)
            .ok_or(ToolExecutionServiceError::CatalogDrift)?;
        let evidence = self
            .executor
            .execute(invocation)
            .await
            .map_err(ToolExecutionServiceError::Executor)?;
        let observation = admit_executor_evidence(evidence, effect_class);
        self.commit_executor_observation(observation, dispatch_permit)
            .await
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

    async fn prepare_continuation(
        &mut self,
        batch: &ToolBatch,
    ) -> Result<
        ToolExecutionServiceOutcome,
        ToolExecutionServiceError<Transaction::Error, Executor::Error>,
    > {
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
                    return Ok(ToolExecutionServiceOutcome::ContinuationTargetUnavailable(
                        failed,
                    ));
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
    evidence.correlation.bind(observation)
}

/// Selects initial approval for one proposal from frozen posture and catalog.
pub(crate) fn initial_tool_approval(
    posture: DangerousToolAutoApproval,
    definition: Option<&ToolDefinition>,
) -> InitialToolApproval {
    match posture {
        DangerousToolAutoApproval::ApproveAll => InitialToolApproval::SessionBlanket,
        DangerousToolAutoApproval::Disabled => match definition
            .map(ToolDefinition::permission_default)
            .unwrap_or(ToolPermissionDefault::Confirm)
        {
            ToolPermissionDefault::Auto => InitialToolApproval::PolicyAuto,
            ToolPermissionDefault::Confirm => InitialToolApproval::Confirm,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;
    use signalbox_domain::{
        ResolvedContextFrontierReconstitutionInput, ToolApprovalDecision,
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

    fn request(arguments: &str) -> ToolRequest {
        ToolRequestReconstitutionInput::new(
            ToolRequestId::from_uuid(Uuid::from_u128(4)),
            SessionId::from_uuid(Uuid::from_u128(1)),
            TurnId::from_uuid(Uuid::from_u128(2)),
            ModelCallId::from_uuid(Uuid::from_u128(3)),
            ToolRequestOrdinal::from_u32(0),
            tool_name("known"),
            NormalizedToolArguments::try_from_provider_text(arguments.to_owned())
                .expect("fixture arguments fit the admission bound"),
        )
        .into_request()
    }

    fn prepared_batch(arguments: &str, effect: ToolEffectClass) -> (ToolBatch, ToolAttemptId) {
        let request = request(arguments);
        let attempt_id = ToolAttemptId::from_uuid(Uuid::from_u128(6));
        let turn_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(5));
        let approval = ToolApprovalResolutionReconstitutionInput::new(
            request.id(),
            ToolApprovalDecision::Approve,
            ToolDecisionSource::PolicyAuto,
        )
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
            ToolAttemptReconstitutionState::Prepared,
        )
        .reconstitute();
        let snapshot = ResolvedContextFrontierReconstitutionInput::new(
            request.session(),
            signalbox_domain::ContextFrontierId::from_uuid(Uuid::from_u128(7)),
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
        .expect("prepared fixture batch is correlated");
        (batch, attempt_id)
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeError {
        Ordinary,
        CommitAmbiguous,
    }

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(match self {
                Self::Ordinary => "fake tool-loop failure",
                Self::CommitAmbiguous => "fake commit-ambiguous tool-loop failure",
            })
        }
    }

    impl Error for FakeError {}

    impl ClassifyOperatorFailure for FakeError {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            match self {
                Self::Ordinary => OperatorFailureClass::CallerOrHubBug,
                Self::CommitAmbiguous => OperatorFailureClass::Infrastructure {
                    commit_ambiguous: true,
                },
            }
        }
    }

    struct FakeTransaction {
        batch: ToolBatch,
        prepared: signalbox_domain::CurrentToolAttempt,
        events: Arc<Mutex<Vec<&'static str>>>,
        ambiguous_authorization: bool,
        authorization_committed: bool,
        commit_failures: usize,
        committed: bool,
    }

    impl ToolExecutionTransaction for FakeTransaction {
        type Error = FakeError;

        async fn load_active_batch(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
        ) -> Result<Option<ToolBatch>, Self::Error> {
            Ok(Some(self.batch.clone()))
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
            _attempt: ToolAttemptId,
        ) -> Result<AuthorizedToolAttempt, Self::Error> {
            self.events.lock().expect("event lock").push("authorize");
            if self.ambiguous_authorization {
                self.authorization_committed = true;
                return Err(FakeError::CommitAmbiguous);
            }
            self.prepared
                .clone()
                .authorize()
                .map_err(|_| FakeError::Ordinary)
        }

        async fn reread_ambiguous_authorization(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            _attempt: ToolAttemptId,
        ) -> Result<ToolAttemptAuthorizationStatus, Self::Error> {
            self.events.lock().expect("event lock").push("reread");
            if self.authorization_committed {
                Ok(ToolAttemptAuthorizationStatus::InFlight(
                    self.prepared
                        .clone()
                        .authorize()
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
                .prepared
                .clone()
                .authorize()
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
            panic!("prepared fixture is not a restart loss")
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

    /// INV-020: registry automation records policy provenance, while blanket
    /// automation remains explicitly distinct from owner agency.
    #[test]
    fn inv020_initial_policy_preserves_automation_provenance() {
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
            ToolDecisionSource::OwnerCommand
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

    /// INV-024 / INV-027: an approved unknown request closes with typed
    /// preflight evidence before authorization or executor entry.
    #[tokio::test]
    async fn inv024_inv027_unknown_tool_never_crosses_executor_boundary() {
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

    /// INV-011 / INV-021 / INV-024: durable authorization precedes the
    /// executor, and only its exact correlation can commit returned evidence.
    #[tokio::test]
    async fn inv011_inv021_inv024_executor_evidence_is_fenced_and_committed_in_order() {
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

    /// INV-024: an effect-free executor cannot strand an impossible ambiguity;
    /// external-effect evidence retains its recovery-significant distinction.
    #[test]
    fn inv024_effect_class_admits_only_external_effect_ambiguity() {
        for (effect_class, expected) in [
            (
                ToolEffectClass::EffectFree,
                ToolAttemptObservation::KnownFailed {
                    error: ToolExecutionError::new(ToolExecutionErrorKind::ExecutionFailed, None),
                },
            ),
            (
                ToolEffectClass::ExternalEffect,
                ToolAttemptObservation::Ambiguous,
            ),
        ] {
            let (batch, _) = prepared_batch("{}", effect_class);
            let current = match batch.attempt(batch.requests()[0].id()) {
                Some(signalbox_domain::ReconstitutedToolAttempt::Current(current)) => {
                    current.clone()
                }
                _ => panic!("fixture has one prepared attempt"),
            };
            let authorized = current
                .authorize()
                .expect("prepared fixture authorizes exactly once");
            let expected_correlation = authorized.correlation();
            let invocation = ToolExecutionInvocation::try_new(
                batch.requests()[0].clone(),
                definition("known", ToolPermissionDefault::Auto, effect_class),
                &authorized,
            )
            .expect("fixture invocation matches durable authority");
            let observation = admit_executor_evidence(
                invocation.bind(ToolExecutorEvidence::Ambiguous),
                effect_class,
            );

            assert_eq!(observation.observation(), &expected);
            assert_eq!(observation.correlation(), &expected_correlation);
        }
    }

    /// INV-011 / INV-024: the definition selected by successful preflight is
    /// the exact same-incarnation declaration carried across authorization.
    #[tokio::test]
    async fn inv011_inv024_authorization_uses_preflight_definition_snapshot() {
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

    /// INV-011 / INV-024: a lost authorization acknowledgement is reread
    /// while the dispatch gate remains held, and committed authority enters
    /// the executor exactly once.
    #[tokio::test]
    async fn inv011_inv024_ambiguous_authorization_resumes_committed_fence() {
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

    /// INV-011 / INV-024: a failed result commit retains exact executor
    /// evidence and retries only that commit after an authoritative reread.
    #[tokio::test]
    async fn inv011_inv024_failed_commit_does_not_repeat_executor_work() {
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
