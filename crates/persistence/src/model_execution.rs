//! PostgreSQL transactions surrounding the first text-only model call.
//!
//! The three transaction roles in docs/spec/model-call-execution.md stay
//! explicit here: a durable `Prepared` checkpoint, a separate
//! send-authorization commit, and a fresh post-effect observation commit. No
//! method holds a database transaction across provider work.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    error::Error,
    fmt,
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
    sync::Arc,
    time::Duration,
};

use rust_decimal::Decimal;
use signalbox_application::{
    AttachmentPreparationFailure, AuthorizeModelCallOutcome, AuthorizeModelCallTransaction,
    AvailabilitySuccessorOutcome, ClassifyOperatorFailure, CommitModelCallObservationTransaction,
    CredentialPoolExhaustedOutcome, FailPreparedModelCallTransaction, ModelCallAuthorizationReread,
    ModelCallCredentialReference, ModelCallObservationCommitOutcome,
    ModelCallTerminalIdentityCandidates, OperatorFailureClass, PrepareModelCallOutcome,
    PrepareModelCallTransaction, PrepareToolContinuationOutcome, PreparedModelCallFailureCause,
    ResolvedToolConversationEntry, RetainedModelCallObservationStatus,
    RetainedPreparedFailureStatus,
};
use signalbox_domain::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputLifecycle, ActiveTurnPhase,
    ActiveTurnSchedulingReconstitutionInput, AmbiguousModelCallTurn, AssistantResponsePart,
    AttachmentBlobFact, AuthorizedModelCall, AvailabilitySuccessorModelCallTurn, BlobDigest,
    CancelledModelCallTurn, CancelledToolRoundModelCallTurn, CompletedModelCallTurn,
    ConsumedSteeringReconstitutionInput, ContextFrontierId, ContextHeadroomExhaustedModelCallTurn,
    CorrelatedModelCallTerminalObservation, CredentialPoolExhaustedModelCallTurn,
    DelegatedModelCallRecoveryReconstitutionInput, DelegatedTurnActivationInput,
    DelegatedWakeTurnActivationInput, DelegationContent, DelegationOutcome, DelegationOutcomeKind,
    DelegationOutcomeReason, DirectModelSelection, DurableCommandId,
    EmptyTurnInstructionManifestEvidence, FailedModelCallTurn, FailedModelCallTurnIdentities,
    FastMode, FrozenAliasDefinition, FrozenModelSelection, InstructionDigest, ModelAlias,
    ModelCallDisposition, ModelCallExecution, ModelCallExecutionReconstitutionFailure,
    ModelCallExecutionReconstitutionInput, ModelCallId, ModelCallOriginContent,
    ModelCallPreparationFailure, ModelCallReconstitutionInput, ModelCallReconstitutionState,
    ModelCallTerminalIdentities, ModelCallTerminalObservation, ModelCallTerminalOutcome,
    ModelTargetCatalog, ModelTargetDefinition, PendingSteeringInput,
    PendingSteeringReclassificationIdentity, PinnedProviderTargetReconstitutionInput,
    PreparedDelegatedTurnActivation, PreparedModelCallRequest, PreparedToolResultProjection,
    ProviderModelCallFailureCause, ProviderModelIdentity, ProviderReportedTokenUsage,
    ReclassifiedPendingSteeringTurn, ReconciliationRequiredModelCallTurn,
    ReconciliationRequiredToolTurn, RefusedModelCallTurn,
    ResolvedContextFrontierReconstitutionInput, ResolvedContextFrontierSnapshot,
    ResolvedProviderTarget, SemanticTranscriptEntry, SemanticTranscriptEntryId,
    SemanticTranscriptEntryPayload, SemanticTranscriptEntryReconstitutionInput,
    SemanticTranscriptEntryRef, SessionId, StopRequestedModelCallTurn, ToolApprovalDecision,
    ToolApprovalResolution, ToolDecisionSource, ToolRequest, ToolResultAttemptCorrelation,
    ToolRoundModelCallTurn, TurnAttemptId, TurnId, TurnInstructionManifest,
    TurnInstructionManifestId, TurnTerminalCause, UserContent,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    commit_failure_is_ambiguous,
    mapping::{
        DelegationUpdateStorageKind, DelegationWakeStorageKind,
        ToolApprovalDecisionSourceStorageKind, accepted_input_id_from_uuid,
        dangerous_tool_auto_approval_to_str, defaults_version_to_numeric,
        delegation_outcome_kind_to_str, delegation_outcome_reason_to_str,
        delegation_update_kind_to_str, delegation_wake_subject_to_str,
        durable_command_id_from_uuid, durable_command_id_to_uuid, input_position_from_numeric,
        positive_u64_from_numeric, session_id_from_uuid, session_id_to_uuid,
        tool_approval_decision_source_to_str, tool_approval_posture_to_str,
        tool_request_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid, turn_terminal_cause_to_str,
    },
    outbox::{
        self, InjectionOutcomeOutbox, ModelCallOutboxState, OutboxEvent, ToolBatchOutboxState,
        TurnTerminalOutboxDisposition,
    },
    session::{SessionCorruption, SessionRepositoryError, load_session_from_connection},
    submit_input::{
        SubmitInputCorruption, SubmitInputRepositoryError, decode_goal_origin_configuration,
        load_scheduling_projection, require_applied_interrupt_from_attempt, require_recorded_batch,
    },
};

/// Immutable usage boundary for one resolved continuation mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolContinuationUsageLimit {
    target: ResolvedProviderTarget,
    fast_mode: FastMode,
    max_output_tokens: u64,
    context_window_tokens: u64,
}

impl ToolContinuationUsageLimit {
    /// Defines one deployment-owned continuation boundary.
    pub const fn new(
        target: ResolvedProviderTarget,
        fast_mode: FastMode,
        max_output_tokens: u64,
        context_window_tokens: u64,
    ) -> Self {
        Self {
            target,
            fast_mode,
            max_output_tokens,
            context_window_tokens,
        }
    }

    pub(crate) const fn max_output_tokens(self) -> u64 {
        self.max_output_tokens
    }

    pub(crate) const fn context_window_tokens(self) -> u64 {
        self.context_window_tokens
    }
}

/// Exact continuation limits derived from immutable model configuration.
pub type ToolContinuationUsageLimitCatalog =
    HashMap<(ResolvedProviderTarget, FastMode), ToolContinuationUsageLimit>;

/// Exact prospective first-call material derived from one activation preview.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProspectiveModelCall {
    prepared: signalbox_domain::PreparedInitialModelCall,
    request: PreparedModelCallRequest,
    credential_reference: ModelCallCredentialReference,
    system_prompt: Option<signalbox_domain::SessionSystemPrompt>,
    tool_entries: Box<[ResolvedToolConversationEntry]>,
    projected_members: Box<[SemanticTranscriptEntryRef]>,
    uncommitted_content_bytes: u64,
}

/// The model-visible input whose unreported content one usage read scores.
///
/// Membership is the canonical projection the renderer sends, not physical
/// frontier order: entries a compaction summarized away are no longer model
/// visible and are not part of the next request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProspectiveModelInput<'a> {
    /// One committed frontier, projected from its durable membership.
    Committed(ContextFrontierId),
    /// One uncommitted activation preview.
    ///
    /// A preview's starting frontier and the entries it mints exist only in
    /// memory — its transaction is discarded before any caller can read them —
    /// so the preview carries its own projected membership and the exact
    /// content bytes of the entries no durable row can score.
    Preview {
        /// Model-visible members in projected order.
        projected_members: &'a [SemanticTranscriptEntryRef],
        /// UTF-8 content bytes of the members the preview minted.
        uncommitted_content_bytes: u64,
    },
}

impl From<ContextFrontierId> for ProspectiveModelInput<'_> {
    fn from(frontier: ContextFrontierId) -> Self {
        Self::Committed(frontier)
    }
}

/// Latest terminal-call usage usable as a conservative next-call lower bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReportedModelCallUsage {
    usage: ProviderReportedTokenUsage,
    input_includes_cache_tokens: bool,
    input_is_retained: bool,
    output_is_retained: bool,
    projected_unreported_content_bytes: u64,
}

impl ReportedModelCallUsage {
    /// Returns the exact provider-reported fields retained for the call.
    pub const fn usage(self) -> ProviderReportedTokenUsage {
        self.usage
    }

    /// Whether the stored input field already includes the cache axes.
    pub const fn input_includes_cache_tokens(self) -> bool {
        self.input_includes_cache_tokens
    }

    /// Whether the reported input is still model-visible for the next call.
    ///
    /// An ordinary call's input is the transcript prefix its successor resends.
    /// A dedicated compaction call's input is the source text its summary
    /// replaced, so none of it survives into the next request; that call's
    /// retained material is its summary output plus the content the compaction
    /// did not summarize, which the projected-content allowance counts.
    pub const fn input_is_retained(self) -> bool {
        self.input_is_retained
    }

    /// Whether reported output became assistant transcript for the next call.
    pub const fn output_is_retained(self) -> bool {
        self.output_is_retained
    }

    /// Returns a conservative byte allowance for model-visible transcript
    /// material appended after the reported call's input.
    pub const fn projected_unreported_content_bytes(self) -> u64 {
        self.projected_unreported_content_bytes
    }
}

impl ProspectiveModelCall {
    /// Applies the canonical application frontier renderer with the supplied tool catalog.
    pub fn render(
        &self,
        tools: Box<[signalbox_application::ToolDefinition]>,
    ) -> Result<
        signalbox_application::PreparedModelOperation,
        signalbox_application::ModelFrontierRenderingError,
    > {
        signalbox_application::PreparedModelOperation::render(
            self.request.clone(),
            self.credential_reference.clone(),
            self.system_prompt.clone(),
            tools,
            &self.tool_entries,
        )
    }

    /// Names the model-visible input this preview would send.
    ///
    /// The preview's own starting frontier is never committed, so a usage read
    /// scores this membership and content instead of a frontier identity that
    /// resolves to no durable rows.
    pub fn prospective_input(&self) -> ProspectiveModelInput<'_> {
        ProspectiveModelInput::Preview {
            projected_members: &self.projected_members,
            uncommitted_content_bytes: self.uncommitted_content_bytes,
        }
    }

    const fn prepared(&self) -> &signalbox_domain::PreparedInitialModelCall {
        &self.prepared
    }
}

/// Which fresh execution identity collided with an existing durable record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallIdentityCollision {
    /// The proposed model-call identity already exists.
    ModelCall,
    /// A proposed semantic-entry identity already exists.
    SemanticEntry,
    /// The proposed terminal-frontier identity already exists.
    TerminalFrontier,
    /// A proposed reclassified successor-turn identity already exists.
    ReclassifiedTurn,
}

impl fmt::Display for ModelCallIdentityCollision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let identity = match self {
            Self::ModelCall => "model-call",
            Self::SemanticEntry => "semantic-entry",
            Self::TerminalFrontier => "context-frontier",
            Self::ReclassifiedTurn => "reclassified successor-turn",
        };
        write!(formatter, "{identity} identity already exists")
    }
}

impl Error for ModelCallIdentityCollision {}

/// A durable shape that cannot reconstruct the execution aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallCorruption {
    /// One required durable record or field is absent.
    Missing(&'static str),
    /// Stored records disagree about an exact relationship.
    Inconsistent(&'static str),
    /// A closed durable discriminator is unsupported.
    Unsupported {
        /// The field whose spelling is unsupported.
        field: &'static str,
        /// The exact durable spelling.
        value: String,
    },
    /// The current session projection is invalid.
    CurrentSession(SessionCorruption),
    /// Complete scheduling records are invalid.
    Scheduling(SubmitInputCorruption),
    /// Complete live facts fail domain reconstitution.
    Execution(ModelCallExecutionReconstitutionFailure),
}

impl fmt::Display for ModelCallCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(record) => write!(formatter, "missing model-call execution {record}"),
            Self::Inconsistent(relationship) => {
                write!(
                    formatter,
                    "inconsistent model-call execution {relationship}"
                )
            }
            Self::Unsupported { field, value } => {
                write!(
                    formatter,
                    "unsupported model-call execution {field}: {value}"
                )
            }
            Self::CurrentSession(error) => {
                write!(formatter, "model-call current Session is invalid: {error}")
            }
            Self::Scheduling(error) => {
                write!(
                    formatter,
                    "model-call scheduling projection is invalid: {error}"
                )
            }
            Self::Execution(failure) => {
                write!(
                    formatter,
                    "model-call execution reconstitution failed: {failure:?}"
                )
            }
        }
    }
}

impl Error for ModelCallCorruption {}

/// Database, integrity, identity, or caller failure at the execution boundary.
#[derive(Debug)]
pub enum ModelCallRepositoryError {
    /// PostgreSQL could not complete the operation.
    Database {
        /// The underlying SQLx failure.
        source: sqlx::Error,
        /// Whether failure occurred while awaiting commit.
        commit_ambiguous: bool,
    },
    /// Committed rows cannot form the accepted aggregate.
    Corruption(ModelCallCorruption),
    /// A fresh identity collided durably.
    IdentityCollision(ModelCallIdentityCollision),
    /// The application invoked an execution transition without a live turn.
    NoLiveExecution,
    /// A checked transition rejected an application-supplied operation.
    InvalidTransition(&'static str),
}

impl fmt::Display for ModelCallRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database { source, .. } => {
                write!(formatter, "model-call database failure: {source}")
            }
            Self::Corruption(error) => error.fmt(formatter),
            Self::IdentityCollision(error) => error.fmt(formatter),
            Self::NoLiveExecution => formatter.write_str("no live model-call execution exists"),
            Self::InvalidTransition(operation) => {
                write!(formatter, "model-call transition rejected: {operation}")
            }
        }
    }
}

impl Error for ModelCallRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::Corruption(error) => Some(error),
            Self::IdentityCollision(error) => Some(error),
            Self::NoLiveExecution | Self::InvalidTransition(_) => None,
        }
    }
}

impl ClassifyOperatorFailure for ModelCallRepositoryError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Database {
                commit_ambiguous, ..
            } => OperatorFailureClass::Infrastructure {
                commit_ambiguous: *commit_ambiguous,
            },
            Self::Corruption(_) => OperatorFailureClass::FailClosedCorruption,
            Self::IdentityCollision(_) => OperatorFailureClass::IdentityCollision,
            Self::NoLiveExecution | Self::InvalidTransition(_) => {
                OperatorFailureClass::CallerOrHubBug
            }
        }
    }
}

impl From<ModelCallCorruption> for ModelCallRepositoryError {
    fn from(error: ModelCallCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl From<sqlx::Error> for ModelCallRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::from_database(error, false)
    }
}

impl ModelCallRepositoryError {
    fn from_database(error: sqlx::Error, commit_ambiguous: bool) -> Self {
        if let Some(collision) = identity_collision(&error) {
            Self::IdentityCollision(collision)
        } else {
            Self::Database {
                source: error,
                commit_ambiguous,
            }
        }
    }
}

/// Compatibility spelling for the application-owned prepare result.
pub use signalbox_application::PrepareModelCallOutcome as PrepareInitialModelCallOutcome;

/// Runtime action frozen for one classified availability trigger.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialPoolRuntimeAction {
    /// Leave selection unchanged and terminalize normally.
    Stay,
    /// Exclude the member from the session's next distinct turn.
    SwitchNextTurn,
    /// Continue this turn on the next admitted member.
    SwitchNow,
    /// Exclude the membership for sessions without prior success.
    AvoidNewSessions,
    /// Exclude the profile across every pool until cleared.
    Quarantine,
}

/// Frozen exhaustion policy for one selected credential pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialPoolRuntimeExhaustion {
    /// Wait only when durable exclusion evidence carries a wake condition.
    Park,
    /// Terminalize immediately with the typed pool-wide cause.
    Fail,
}

impl CredentialPoolRuntimeExhaustion {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Park => "park",
            Self::Fail => "fail",
        }
    }

    fn parse(value: &str) -> Result<Self, ModelCallRepositoryError> {
        match value {
            "park" => Ok(Self::Park),
            "fail" => Ok(Self::Fail),
            _ => Err(ModelCallCorruption::Unsupported {
                field: "model_call_credential_pool_policy on_pool_exhausted",
                value: value.to_owned(),
            }
            .into()),
        }
    }
}

impl CredentialPoolRuntimeAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Stay => "stay",
            Self::SwitchNextTurn => "switch_next_turn",
            Self::SwitchNow => "switch_now",
            Self::AvoidNewSessions => "avoid_new_sessions",
            Self::Quarantine => "quarantine",
        }
    }

    fn parse(value: &str) -> Result<Self, ModelCallRepositoryError> {
        match value {
            "stay" => Ok(Self::Stay),
            "switch_next_turn" => Ok(Self::SwitchNextTurn),
            "switch_now" => Ok(Self::SwitchNow),
            "avoid_new_sessions" => Ok(Self::AvoidNewSessions),
            "quarantine" => Ok(Self::Quarantine),
            _ => Err(ModelCallCorruption::Unsupported {
                field: "model_call_credential_pool_policy action",
                value: value.to_owned(),
            }
            .into()),
        }
    }
}

/// Runtime pool member in immutable policy order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialPoolRuntimeMember {
    credential_reference: Arc<str>,
    priority: NonZeroU32,
}

impl CredentialPoolRuntimeMember {
    /// Binds one non-secret profile reference to its membership priority.
    ///
    /// The priority is non-zero by type because persistence stores membership
    /// under `CHECK (priority > 0)`; no admissible caller can construct a
    /// member the schema would reject.
    pub fn new(credential_reference: impl Into<Arc<str>>, priority: NonZeroU32) -> Self {
        Self {
            credential_reference: credential_reference.into(),
            priority,
        }
    }

    /// Borrows the deployment-owned profile reference.
    pub fn credential_reference(&self) -> &str {
        &self.credential_reference
    }

    /// Returns the membership priority.
    pub const fn priority(&self) -> NonZeroU32 {
        self.priority
    }
}

/// Immutable credential-pool policy supplied by admitted daemon configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialPoolRuntimePolicy {
    name: Arc<str>,
    members: Arc<[CredentialPoolRuntimeMember]>,
    on_pool_exhausted: CredentialPoolRuntimeExhaustion,
    quota_exhausted: CredentialPoolRuntimeAction,
    rate_limited: CredentialPoolRuntimeAction,
    overloaded: CredentialPoolRuntimeAction,
    credential_rejected: CredentialPoolRuntimeAction,
}

impl CredentialPoolRuntimePolicy {
    /// Creates one policy in its already-resolved traversal order.
    pub fn new(
        name: impl Into<Arc<str>>,
        members: impl Into<Arc<[CredentialPoolRuntimeMember]>>,
        on_pool_exhausted: CredentialPoolRuntimeExhaustion,
        quota_exhausted: CredentialPoolRuntimeAction,
        rate_limited: CredentialPoolRuntimeAction,
        overloaded: CredentialPoolRuntimeAction,
        credential_rejected: CredentialPoolRuntimeAction,
    ) -> Self {
        Self {
            name: name.into(),
            members: members.into(),
            on_pool_exhausted,
            quota_exhausted,
            rate_limited,
            overloaded,
            credential_rejected,
        }
    }

    /// Borrows the exact pool name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Borrows members in deterministic selection order.
    pub fn members(&self) -> &[CredentialPoolRuntimeMember] {
        &self.members
    }

    const fn action(&self, cause: ProviderModelCallFailureCause) -> CredentialPoolRuntimeAction {
        match cause {
            ProviderModelCallFailureCause::QuotaExhausted => self.quota_exhausted,
            ProviderModelCallFailureCause::RateLimited => self.rate_limited,
            ProviderModelCallFailureCause::Overloaded => self.overloaded,
            ProviderModelCallFailureCause::CredentialRejected => self.credential_rejected,
            ProviderModelCallFailureCause::PermissionDenied
            | ProviderModelCallFailureCause::InvalidRequest
            | ProviderModelCallFailureCause::TargetNotFound
            | ProviderModelCallFailureCause::RequestTooLarge
            | ProviderModelCallFailureCause::ProviderInternal
            | ProviderModelCallFailureCause::Unrecognized => CredentialPoolRuntimeAction::Stay,
        }
    }
}

/// Pool policies indexed by exact resolved target.
pub type CredentialPoolRuntimeCatalog =
    HashMap<ResolvedProviderTarget, CredentialPoolRuntimePolicy>;

/// PostgreSQL adapter for the initial model-call execution transactions.
#[derive(Clone, Debug)]
pub struct PostgresModelCallRepository {
    pool: PgPool,
    targets: ModelTargetCatalog,
    credential_reference: ModelCallCredentialReference,
    credential_families: Option<crate::ModelCredentialFamilyCatalog>,
    credential_pools: CredentialPoolRuntimeCatalog,
    same_credential_attempt_bound: NonZeroUsize,
    cache_inclusive_input_targets: HashSet<ResolvedProviderTarget>,
    continuation_usage_limits: ToolContinuationUsageLimitCatalog,
}

/// Proof that one model-call transaction serialized before either shared lock class.
pub(crate) struct ModelCallOutboxOrderGuard {
    _private: (),
}

const MODEL_CALL_OUTBOX_ORDER_GUARD: &str = "model_call_outbox_order_guard:v1";

impl PostgresModelCallRepository {
    /// Uses the shared pool, immutable target catalog, and current non-secret
    /// credential reference for calls first pinned by this repository.
    pub fn new(
        pool: PgPool,
        targets: ModelTargetCatalog,
        credential_reference: ModelCallCredentialReference,
    ) -> Self {
        Self {
            pool,
            targets,
            credential_reference,
            credential_families: None,
            credential_pools: HashMap::new(),
            same_credential_attempt_bound: NonZeroUsize::MIN,
            cache_inclusive_input_targets: HashSet::new(),
            continuation_usage_limits: HashMap::new(),
        }
    }

    /// Selects credentials from each session's latest append-only snapshot.
    pub fn with_session_credentials(
        mut self,
        credential_families: crate::ModelCredentialFamilyCatalog,
    ) -> Self {
        self.credential_families = Some(credential_families);
        self
    }

    /// Enables per-call credential-pool selection and trigger observation.
    pub fn with_credential_pools(mut self, credential_pools: CredentialPoolRuntimeCatalog) -> Self {
        self.credential_pools = credential_pools;
        self
    }

    /// Bounds recorded attempts on one credential within a turn.
    pub fn with_same_credential_attempt_bound(mut self, bound: NonZeroUsize) -> Self {
        self.same_credential_attempt_bound = bound;
        self
    }

    /// Pins which configured targets report input totals inclusive of cache.
    pub fn with_cache_inclusive_input_targets(
        mut self,
        targets: HashSet<ResolvedProviderTarget>,
    ) -> Self {
        self.cache_inclusive_input_targets = targets;
        self
    }

    /// Pins configured usage headroom for same-turn tool continuations.
    pub fn with_continuation_usage_limits(
        mut self,
        limits: impl IntoIterator<Item = ToolContinuationUsageLimit>,
    ) -> Self {
        self.continuation_usage_limits = limits
            .into_iter()
            .map(|limit| ((limit.target, limit.fast_mode), limit))
            .collect();
        self
    }

    /// Borrows the shared pool for composition-owned adjacent transactions.
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Reads the newest ordinary or dedicated-compaction call with reported input
    /// usage for one exact target.
    ///
    /// A later failed call with no usage does not erase the last provider-confirmed
    /// context size. Callers may use this only as a lower bound: later transcript
    /// entries can make the next request larger, never smaller absent compaction.
    ///
    /// The prospective input names the model-visible entries the next request
    /// would carry. Membership is compared against the reported call's own
    /// frontier, so the allowance covers exactly the content appended after the
    /// provider counted its input.
    pub async fn latest_reported_usage<'a>(
        &self,
        session: SessionId,
        target: ResolvedProviderTarget,
        prospective: impl Into<ProspectiveModelInput<'a>>,
    ) -> Result<Option<ReportedModelCallUsage>, ModelCallRepositoryError> {
        let (projected_members, uncommitted_content_bytes) = match prospective.into() {
            ProspectiveModelInput::Committed(frontier) => {
                let mut connection = self.pool.acquire().await?;
                let members = crate::context_compaction::projected_frontier_membership(
                    &mut connection,
                    session,
                    frontier,
                )
                .await
                .map_err(map_projected_membership_error)?;
                (members, 0)
            }
            ProspectiveModelInput::Preview {
                projected_members,
                uncommitted_content_bytes,
            } => (projected_members.to_vec(), uncommitted_content_bytes),
        };
        let member_sessions = projected_members
            .iter()
            .map(|member| session_id_to_uuid(member.source_session()))
            .collect::<Vec<_>>();
        let member_entries = projected_members
            .iter()
            .map(|member| member.entry().into_uuid())
            .collect::<Vec<_>>();
        // Calls no newer than the latest compaction cannot win the final call-ID
        // ordering, so discard them before the exact summary-membership probe.
        let row = sqlx::query(
            "WITH latest_compaction AS MATERIALIZED (
                SELECT compaction.source_frontier_id,
                       compaction.summary_entry_id,
                       call.model_call_id,
                       call.resolved_provider_model_identity_id,
                       call.state_kind,
                       call.terminal_disposition_kind,
                       COALESCE(call.usage_input_includes_cache_tokens, false) AS
                           usage_input_includes_cache_tokens,
                       call.input_tokens AS usage_input_tokens,
                       call.output_tokens AS usage_output_tokens,
                       call.cache_creation_input_tokens AS
                           usage_cache_creation_input_tokens,
                       call.cache_read_input_tokens AS usage_cache_read_input_tokens
                  FROM context_compaction AS compaction
                  JOIN context_compaction_model_call AS call
                    ON call.session_id = compaction.session_id
                   AND call.model_call_id = compaction.producing_call_id
                 WHERE compaction.session_id = $1
                   AND NOT EXISTS (
                       SELECT 1
                         FROM context_compaction AS successor
                        WHERE successor.session_id = compaction.session_id
                          AND successor.predecessor_compaction_id =
                              compaction.context_compaction_id
                   )
             ), ordinary_candidate AS (
                SELECT 'ordinary'::text AS call_kind,
                       model_call.model_call_id,
                       model_call.context_frontier_id,
                       model_call.usage_input_includes_cache_tokens,
                       NOT EXISTS (
                           SELECT 1
                             FROM semantic_transcript_entry AS compacted
                            WHERE compacted.source_session_id = model_call.session_id
                              AND compacted.producing_model_call_id = model_call.model_call_id
                              AND compacted.payload_kind = 'provider_compaction'
                              AND compacted.assistant_text_value::jsonb ->> 'content'
                                  IS NOT NULL
                       ) AS input_is_retained,
                       model_call.terminal_disposition_kind = 'completed' AS output_is_retained,
                       model_call.usage_input_tokens,
                       model_call.usage_output_tokens,
                       model_call.usage_cache_creation_input_tokens,
                       model_call.usage_cache_read_input_tokens,
                       NULL::uuid AS reported_summary_entry_id,
                       EXISTS (
                           SELECT 1
                             FROM semantic_transcript_entry AS compacted
                            WHERE compacted.source_session_id = model_call.session_id
                              AND compacted.producing_model_call_id = model_call.model_call_id
                              AND compacted.payload_kind = 'provider_compaction'
                       ) AS has_provider_compaction,
                       headroom.projected_result_content_bytes AS
                           proven_unreported_content_bytes
                  FROM model_call
                  LEFT JOIN tool_continuation_context_headroom AS headroom
                    ON headroom.session_id = model_call.session_id
                   AND headroom.producing_model_call_id = model_call.model_call_id
                 WHERE model_call.session_id = $1
                   AND model_call.resolved_provider_model_identity_id = $2
                   AND model_call.state_kind = 'terminal'
                   AND model_call.usage_input_tokens IS NOT NULL
                   AND NOT EXISTS (
                       SELECT 1
                         FROM latest_compaction AS latest
                        WHERE model_call.model_call_id <= latest.model_call_id
                   )
                   AND NOT EXISTS (
                       SELECT 1
                         FROM latest_compaction AS latest
                        WHERE NOT EXISTS (
                            SELECT 1
                              FROM context_frontier_member AS member
                             WHERE member.owning_session_id = model_call.session_id
                               AND member.context_frontier_id =
                                   model_call.context_frontier_id
                               AND member.source_session_id = model_call.session_id
                               AND member.semantic_entry_id = latest.summary_entry_id
                        )
                   )
             ), compaction_candidate AS (
                -- A dedicated compaction call's reported input is the source
                -- text its summary replaced: the summary removed exactly that
                -- material from model visibility, so none of it bounds the next
                -- request. Its reported output is the retained summary, and the
                -- content the compaction did not summarize stays in the
                -- projected membership below.
                SELECT 'context_compaction'::text AS call_kind,
                       latest.model_call_id,
                       latest.source_frontier_id AS context_frontier_id,
                       latest.usage_input_includes_cache_tokens,
                       false AS input_is_retained,
                       true AS output_is_retained,
                       latest.usage_input_tokens,
                       latest.usage_output_tokens,
                       latest.usage_cache_creation_input_tokens,
                       latest.usage_cache_read_input_tokens,
                       latest.summary_entry_id AS reported_summary_entry_id,
                       false AS has_provider_compaction,
                       NULL::numeric AS proven_unreported_content_bytes
                  FROM latest_compaction AS latest
                 WHERE latest.resolved_provider_model_identity_id = $2
                   AND latest.state_kind = 'terminal'
                   AND latest.terminal_disposition_kind = 'completed'
                   AND latest.usage_input_tokens IS NOT NULL
             ), latest_call AS (
                SELECT *
                  FROM (
                      SELECT * FROM ordinary_candidate
                      UNION ALL
                      SELECT * FROM compaction_candidate
                  ) AS candidate
                 ORDER BY model_call_id DESC
                 LIMIT 1
             ), unreported_member AS MATERIALIZED (
                -- An ordinary call's reported input is its own frontier, so
                -- only projected members outside that membership are new. A
                -- compaction call reports no retained input at all: every
                -- projected member except its summary is content the next
                -- request adds to that summary.
                SELECT prospective.source_session_id, prospective.semantic_entry_id
                  FROM UNNEST($3::uuid[], $4::uuid[])
                       AS prospective(source_session_id, semantic_entry_id)
                EXCEPT
                SELECT reported.source_session_id, reported.semantic_entry_id
                  FROM latest_call
                  JOIN context_frontier_member AS reported
                    ON reported.owning_session_id = $1
                   AND reported.context_frontier_id =
                       latest_call.context_frontier_id
                 WHERE latest_call.call_kind = 'ordinary'
             )
             SELECT usage_input_includes_cache_tokens, input_is_retained,
                    output_is_retained,
                    usage_input_tokens, usage_output_tokens,
                    usage_cache_creation_input_tokens,
                    usage_cache_read_input_tokens,
                    COALESCE(latest_call.proven_unreported_content_bytes, 0)
                    -- Entries an uncommitted preview minted have no durable row
                    -- to score; the preview measured their content itself.
                    + $5::numeric
                    + (
                        SELECT COALESCE(SUM(
                            CASE
                                -- Aggregated provider output usage already
                                -- includes every response part from the call
                                -- that performed server-side compaction.
                                WHEN latest_call.has_provider_compaction
                                     AND entry.producing_model_call_id =
                                         latest_call.model_call_id
                                     AND entry.payload_kind IN (
                                         'assistant_text',
                                         'provider_compaction',
                                         'assistant_tool_use'
                                     )
                                THEN 0
                                -- The durable proof already measured every
                                -- result the producing call's round projected,
                                -- including a returning foreground delegation's
                                -- child result. Each correlates to that call
                                -- through the request that produced it.
                                WHEN latest_call.proven_unreported_content_bytes IS NOT NULL
                                     AND (
                                         (
                                             entry.payload_kind IN (
                                                 'tool_execution_result',
                                                 'tool_denied'
                                             )
                                             AND result_request.producing_model_call_id =
                                                 latest_call.model_call_id
                                         )
                                         OR (
                                             entry.payload_kind = 'delegation_result'
                                             AND awaiting_request.producing_model_call_id =
                                                 latest_call.model_call_id
                                         )
                                     )
                                THEN 0
                                ELSE CASE entry.payload_kind
                                    WHEN 'imported_entry' THEN
                                        COALESCE(octet_length(imported.content_encoding), 0)
                                    -- Accepted-input content is an ordered part
                                    -- array, so its context cost is the sum of
                                    -- the text parts; attachment parts carry
                                    -- their own rendered-stub accounting.
                                    WHEN 'origin_accepted_input' THEN
                                        COALESCE((
                                            SELECT SUM(COALESCE(
                                                octet_length(part.text_value), 0
                                            ))
                                              FROM accepted_input_content_part AS part
                                             WHERE part.accepted_input_id =
                                                   input.accepted_input_id
                                        ), 0)
                                    WHEN 'steering_accepted_input' THEN
                                        COALESCE((
                                            SELECT SUM(COALESCE(
                                                octet_length(part.text_value), 0
                                            ))
                                              FROM accepted_input_content_part AS part
                                             WHERE part.accepted_input_id =
                                                   input.accepted_input_id
                                        ), 0)
                                    WHEN 'context_summary' THEN
                                        COALESCE(octet_length(entry.context_summary_value), 0)
                                    WHEN 'assistant_text' THEN
                                        COALESCE(octet_length(entry.assistant_text_value), 0)
                                    WHEN 'provider_compaction' THEN
                                        COALESCE(octet_length(entry.assistant_text_value), 0)
                                    WHEN 'assistant_tool_use' THEN
                                        COALESCE(octet_length(request.tool_name), 0)
                                        + COALESCE(octet_length(request.arguments_text), 0)
                                    WHEN 'tool_execution_result' THEN
                                        COALESCE(octet_length(attempt.result_text), 0)
                                        + COALESCE(octet_length(attempt.error_detail), 0)
                                    WHEN 'tool_denied' THEN
                                        COALESCE(octet_length(decision.denial_reason), 0)
                                    WHEN 'delegated_task' THEN
                                        COALESCE(octet_length(task.task_content), 0)
                                    WHEN 'delegation_message' THEN
                                        COALESCE(octet_length(message.content_text), 0)
                                    WHEN 'delegation_result' THEN
                                        COALESCE(octet_length(child_result.content_text), 0)
                                    ELSE 0
                                END
                            END
                        ), 0)::numeric
                          FROM unreported_member AS prospective
                          JOIN semantic_transcript_entry AS entry
                            ON entry.source_session_id = prospective.source_session_id
                           AND entry.semantic_entry_id = prospective.semantic_entry_id
                          LEFT JOIN accepted_input AS input
                            ON input.accepted_input_id = entry.origin_accepted_input_id
                           AND input.session_id = entry.source_session_id
                          LEFT JOIN imported_transcript_entry AS imported
                            ON imported.imported_conversation_id = entry.imported_conversation_id
                           AND imported.imported_transcript_entry_id =
                               entry.imported_transcript_entry_id
                          LEFT JOIN tool_request AS request
                            ON request.request_id = entry.assistant_tool_request_id
                           AND request.session_id = entry.source_session_id
                          LEFT JOIN tool_attempt AS attempt
                            ON attempt.attempt_id = entry.tool_result_attempt_id
                           AND attempt.session_id = entry.source_session_id
                          LEFT JOIN tool_request AS result_request
                            ON result_request.request_id = COALESCE(
                                   attempt.request_id,
                                   entry.tool_result_request_id
                               )
                           AND result_request.session_id = entry.source_session_id
                          LEFT JOIN tool_request AS awaiting_request
                            ON awaiting_request.request_id =
                               entry.delegation_result_awaiting_tool_request_id
                           AND awaiting_request.session_id = entry.source_session_id
                          LEFT JOIN tool_approval_decision AS decision
                            ON decision.request_id = entry.tool_result_request_id
                          LEFT JOIN session_delegation_initial_task AS task
                            ON task.child_session_id = entry.source_session_id
                           AND task.semantic_entry_id = entry.semantic_entry_id
                          LEFT JOIN session_message AS message
                            ON message.message_id = entry.delegation_message_id
                          LEFT JOIN session_child_result AS child_result
                            ON child_result.spawning_tool_request_id =
                               entry.delegation_result_spawning_tool_request_id
                         WHERE NOT (
                                   latest_call.usage_output_tokens IS NOT NULL
                               AND (
                                      (
                                          latest_call.call_kind = 'ordinary'
                                          AND entry.payload_kind IN (
                                              'assistant_text',
                                              'assistant_tool_use'
                                          )
                                          AND entry.producing_model_call_id =
                                              latest_call.model_call_id
                                      )
                                      OR (
                                          latest_call.call_kind = 'context_compaction'
                                          AND entry.source_session_id = $1
                                          AND entry.semantic_entry_id =
                                              latest_call.reported_summary_entry_id
                                      )
                               )
                           )
                    ) AS projected_unreported_content_bytes
               FROM latest_call",
        )
        .bind(session_id_to_uuid(session))
        .bind(target.identity().into_uuid())
        .bind(&member_sessions)
        .bind(&member_entries)
        .bind(Decimal::from(uncommitted_content_bytes))
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let decode = |field: &'static str| -> Result<Option<u64>, ModelCallRepositoryError> {
            row.try_get::<Option<Decimal>, _>(field)?
                .map(|value| {
                    if !value.fract().is_zero() || value.is_sign_negative() {
                        return Err(ModelCallCorruption::Inconsistent(
                            "completed model-call token usage",
                        )
                        .into());
                    }
                    u64::try_from(value).map_err(|_| {
                        ModelCallCorruption::Inconsistent("completed model-call token usage").into()
                    })
                })
                .transpose()
        };
        Ok(Some(ReportedModelCallUsage {
            usage: ProviderReportedTokenUsage::unreported()
                .with_input_tokens(decode("usage_input_tokens")?)
                .with_output_tokens(decode("usage_output_tokens")?)
                .with_cache_creation_input_tokens(decode("usage_cache_creation_input_tokens")?)
                .with_cache_read_input_tokens(decode("usage_cache_read_input_tokens")?),
            input_includes_cache_tokens: row.try_get("usage_input_includes_cache_tokens")?,
            input_is_retained: row.try_get("input_is_retained")?,
            output_is_retained: row.try_get("output_is_retained")?,
            projected_unreported_content_bytes: decode("projected_unreported_content_bytes")?
                .ok_or(ModelCallCorruption::Missing(
                    "projected unreported transcript content byte count",
                ))?,
        }))
    }

    /// Whether a preserved request-size failure still lacks later evidence that
    /// the same target accepted a call after it or that compaction replaced it.
    ///
    /// The provider may reject an oversized call without reporting token usage.
    /// A successor can use that typed terminal evidence to compact once, but a
    /// later provider-accepted ordinary call or completed compaction on the
    /// prospective lineage supersedes the failure so it cannot trigger forever.
    /// The supplied frontier is the durable immediate prefix of an uncommitted
    /// activation preview, so lineage checks never depend on a speculative ID.
    pub async fn request_too_large_requires_compaction(
        &self,
        session: SessionId,
        target: ResolvedProviderTarget,
        persisted_prospective_prefix: ContextFrontierId,
    ) -> Result<bool, ModelCallRepositoryError> {
        let requires_compaction = sqlx::query_scalar(
            "WITH latest_failure AS MATERIALIZED (
                SELECT failed.model_call_id
                  FROM model_call AS failed
                 WHERE failed.session_id = $1
                   AND failed.resolved_provider_model_identity_id = $2
                   AND failed.state_kind = 'terminal'
                   AND failed.terminal_provider_failure_cause =
                       'request_too_large'
                   AND context_frontier_preserves_prefix(
                           $1,
                           failed.context_frontier_id,
                           $3
                       )
                 ORDER BY failed.model_call_id DESC
                 LIMIT 1
             )
             SELECT EXISTS (
                 SELECT 1
                   FROM latest_failure AS failed
                  WHERE NOT EXISTS (
                      SELECT 1
                        FROM model_call AS accepted
                       WHERE accepted.session_id = $1
                         AND accepted.resolved_provider_model_identity_id = $2
                         AND accepted.model_call_id > failed.model_call_id
                         AND accepted.state_kind = 'terminal'
                         AND (
                                accepted.terminal_disposition_kind = 'completed'
                             OR accepted.usage_input_tokens IS NOT NULL
                         )
                         AND context_frontier_preserves_prefix(
                                 $1,
                                 accepted.context_frontier_id,
                                 $3
                             )
                      UNION ALL
                      SELECT 1
                        FROM context_compaction AS compaction
                        JOIN context_compaction_model_call AS accepted
                          ON accepted.session_id = compaction.session_id
                         AND accepted.model_call_id =
                             compaction.producing_call_id
                       WHERE compaction.session_id = $1
                         AND accepted.resolved_provider_model_identity_id = $2
                         AND accepted.model_call_id > failed.model_call_id
                         AND accepted.state_kind = 'terminal'
                         AND accepted.terminal_disposition_kind = 'completed'
                         AND context_frontier_preserves_prefix(
                                 $1,
                                 compaction.result_frontier_id,
                                 $3
                             )
                  )
             )",
        )
        .bind(session_id_to_uuid(session))
        .bind(target.identity().into_uuid())
        .bind(persisted_prospective_prefix.into_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(requires_compaction)
    }

    /// Resolves the credential currently pinned for one session and exact
    /// provider target through the same family catalog used by model calls.
    pub async fn resolve_session_credential_reference(
        &self,
        session: SessionId,
        target: ResolvedProviderTarget,
    ) -> Result<ModelCallCredentialReference, ModelCallRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        resolve_session_credential(
            &mut connection,
            session,
            target,
            FastMode::Disabled,
            &self.credential_reference,
            self.credential_families.as_ref(),
        )
        .await
    }

    /// Derives tool-loop storage from this repository's exact database and
    /// continuation configuration.
    pub fn tool_loop_repository(&self) -> crate::tool_loop::PostgresToolLoopRepository {
        crate::tool_loop::PostgresToolLoopRepository::with_model_calls(
            self.pool.clone(),
            self.targets.clone(),
            self.credential_reference.clone(),
        )
        .with_cache_inclusive_input_targets(self.cache_inclusive_input_targets.clone())
        .with_continuation_usage_limits(self.continuation_usage_limits.clone())
        .with_session_credentials(self.credential_families.clone())
        .with_credential_pools(self.credential_pools.clone())
    }

    /// Derives approval-judge storage from this repository's exact database
    /// and model configuration.
    pub fn approval_judge_repository(
        &self,
    ) -> crate::approval_judge::PostgresApprovalJudgeRepository {
        crate::approval_judge::PostgresApprovalJudgeRepository::new(
            self.pool.clone(),
            self.targets.clone(),
            self.credential_reference.clone(),
            self.credential_families.clone(),
            self.cache_inclusive_input_targets.clone(),
        )
    }

    /// Reconstitutes the exact first-call operation for one read-only activation preview.
    pub async fn preview_activation_operation(
        &self,
        preview: &signalbox_domain::PreparedTurnActivation,
        call: ModelCallId,
    ) -> Result<Option<ProspectiveModelCall>, ModelCallRepositoryError> {
        let session_id = preview.turn().session();
        let mut transaction = self.pool.begin().await?;
        let session = match load_session_from_connection(&mut transaction, session_id).await {
            Ok(Some(session)) => session,
            Ok(None) => return Err(ModelCallRepositoryError::NoLiveExecution),
            Err(SessionRepositoryError::Database(error)) => return Err(error.into()),
            Err(SessionRepositoryError::Corruption(error)) => {
                return Err(ModelCallCorruption::CurrentSession(error).into());
            }
        };
        let scheduling = load_scheduling_projection(&mut transaction, session)
            .await
            .map_err(map_scheduling_error)?;
        let starting_entries = preview
            .starting_entries()
            .iter()
            .map(|entry| (entry.reference(), entry))
            .collect::<BTreeMap<_, _>>();
        let frontier_entries = preview
            .starting_snapshot()
            .ordered_entries()
            .map(|reference| {
                starting_entries
                    .get(&reference)
                    .copied()
                    .or_else(|| scheduling.semantic_entry(reference))
                    .cloned()
                    .ok_or_else(|| {
                        ModelCallCorruption::Missing("preview frontier semantic entry").into()
                    })
            })
            .collect::<Result<Vec<_>, ModelCallRepositoryError>>()?;
        let origin_contents =
            load_origin_contents(&mut transaction, &frontier_entries, &[], &[]).await?;
        let attachment_blob_facts =
            load_attachment_blob_facts(&mut transaction, &origin_contents).await?;
        let tool_result_correlations =
            load_tool_result_correlations(&mut transaction, &frontier_entries).await?;
        let tool_denial_correlations =
            load_tool_denial_correlations(&mut transaction, &frontier_entries).await?;
        // The canonical projection the renderer sends. A preview never commits
        // its starting frontier, so a later usage read cannot resolve that
        // identity to membership and takes this instead.
        let projected_members =
            signalbox_domain::ContextFrontierProjection::from_complete_entries(&frontier_entries)
                .map_err(|_| ModelCallCorruption::Inconsistent("preview frontier projection"))?
                .ordered_entries()
                .collect::<Box<[SemanticTranscriptEntryRef]>>();
        // The entries this preview minted are equally uncommitted, so the
        // durable payload-kind accounting a usage read applies to committed
        // members cannot see them. Every other projected member resolved
        // through the scheduling projection and is scored durably there.
        let uncommitted_content_bytes = projected_members
            .iter()
            .filter(|reference| scheduling.semantic_entry(**reference).is_none())
            .filter_map(|reference| starting_entries.get(reference))
            .fold(0_u64, |total, entry| {
                total.saturating_add(preview_entry_content_bytes(entry, &origin_contents))
            });
        let execution = ModelCallExecutionReconstitutionInput::new(
            preview.turn(),
            self.targets.clone(),
            preview.starting_snapshot().clone(),
            frontier_entries,
            origin_contents,
            None,
            Vec::new(),
        )
        .with_attachment_blob_facts(attachment_blob_facts)
        .with_tool_result_correlations(tool_result_correlations)
        .with_tool_denial_correlations(tool_denial_correlations)
        .reconstitute()
        .map_err(|error| {
            let (_, failure) = error.into_parts();
            ModelCallCorruption::Execution(failure)
        })?;
        let prepared = execution
            .clone()
            .prepare_initial_call(call)
            .map_err(|_| ModelCallRepositoryError::InvalidTransition("preview initial call"))?;
        let request = execution
            .preview_initial_call(call)
            .map_err(|_| ModelCallRepositoryError::InvalidTransition("preview initial call"))?;
        let system_prompt = load_frozen_epoch_system_prompt(
            &mut transaction,
            session_id,
            preview.turn().configuration().session_defaults_version(),
        )
        .await?;
        let tool_entries = load_tool_conversation_entries(&mut transaction, &request).await?;
        let fast_mode = request.model_settings().effective().fast_mode();
        let credential_reference = resolve_session_credential(
            &mut transaction,
            session_id,
            request.call().target(),
            fast_mode,
            &self.credential_reference,
            self.credential_families.as_ref(),
        )
        .await?;
        // Preview the member preparation will actually select. The caller
        // spends an authenticated input-token count on this reference before
        // any call exists, so previewing the session default would count
        // against a member a pending displacement or quarantine has already
        // excluded — and an account-wide rate limit reaches the count endpoint
        // too, so activation would abort before selection could reach the
        // admissible member. Selection consumes no displacement row and this
        // transaction rolls back, so nothing durable moves.
        let selected = select_runtime_pool_credential(
            &mut transaction,
            session_id,
            execution.turn(),
            execution.current_attempt().id(),
            serving_pool_target(
                self.credential_families.as_ref(),
                request.call().target(),
                fast_mode,
            ),
            credential_reference.clone(),
            &self.credential_pools,
        )
        .await?;
        // An exhausted pool has no member to preview at all. Falling back to
        // the session default would spend an authenticated token count against
        // a quarantined or rejected account, and that count fails, aborting
        // activation before preparation could record the typed exhaustion. The
        // caller activates the turn call-free instead and lets ordinary
        // preparation own the closure, exactly as the counted-activation
        // checkpoint already does when selection admits no member.
        let Some(credential_reference) = selected.reference else {
            transaction.rollback().await?;
            return Ok(None);
        };
        transaction.rollback().await?;
        Ok(Some(ProspectiveModelCall {
            prepared,
            request,
            credential_reference,
            system_prompt,
            tool_entries,
            projected_members,
            uncommitted_content_bytes,
        }))
    }

    /// Checkpoints the exact no-steering initial call in the transaction that
    /// just committed its counted activation.
    pub(crate) async fn checkpoint_counted_activation_in_transaction(
        &self,
        connection: &mut PgConnection,
        activated: &signalbox_domain::ActivatedTurn,
        prospective: &ProspectiveModelCall,
        _outbox_order_guard: ModelCallOutboxOrderGuard,
    ) -> Result<(), ModelCallRepositoryError> {
        let prepared = prospective.prepared();
        let signalbox_domain::ActiveTurnPhase::Running { current_attempt } = activated.phase()
        else {
            return Err(ModelCallRepositoryError::InvalidTransition(
                "counted activation is not running",
            ));
        };
        if prepared.session() != activated.session()
            || prepared.turn() != activated.turn()
            || prepared.attempt() != current_attempt.id()
            || current_attempt.state() != &signalbox_domain::CurrentTurnAttemptState::Prepared
            || !prepared.consumed_steering().is_empty()
            || prepared.steering_snapshot().is_some()
        {
            return Err(ModelCallRepositoryError::InvalidTransition(
                "counted preparation does not match activated turn",
            ));
        }
        let fast_mode = activated
            .configuration()
            .effective()
            .model_settings()
            .effective()
            .fast_mode();
        let credential_reference = resolve_session_credential(
            connection,
            activated.session(),
            prepared.call().target(),
            fast_mode,
            &self.credential_reference,
            self.credential_families.as_ref(),
        )
        .await?;
        let selected = select_runtime_pool_credential(
            connection,
            prepared.session(),
            prepared.turn(),
            prepared.attempt(),
            serving_pool_target(
                self.credential_families.as_ref(),
                prepared.call().target(),
                fast_mode,
            ),
            credential_reference,
            &self.credential_pools,
        )
        .await?;
        outbox::lock_sequence_allocator(connection).await?;
        let Some(credential_reference) = selected.reference.as_ref() else {
            // The activated turn remains call-free; the ordinary preparation
            // pass owns the typed pool-exhaustion closure and its identities.
            return Ok(());
        };
        insert_prepared_call(
            connection,
            prepared,
            credential_reference,
            selected.policy.as_ref(),
            self.cache_inclusive_input_targets
                .contains(&prepared.call().target()),
        )
        .await?;
        consume_pool_member_actions(
            connection,
            prepared.turn(),
            &selected.pending_consumed_actions,
        )
        .await
    }

    /// Checkpoints and closes the exact prospective call after attachment
    /// verification found a definitive failure during provider-native counting.
    pub(crate) async fn fail_counted_attachment_in_transaction(
        &self,
        connection: &mut PgConnection,
        activated: &signalbox_domain::ActivatedTurn,
        prospective: &ProspectiveModelCall,
        failure: AttachmentPreparationFailure,
        identities: FailedModelCallTurnIdentities,
        outbox_order_guard: ModelCallOutboxOrderGuard,
    ) -> Result<FailedModelCallTurn, ModelCallRepositoryError> {
        self.checkpoint_counted_activation_in_transaction(
            connection,
            activated,
            prospective,
            outbox_order_guard,
        )
        .await?;
        let call = prospective.prepared().call().id();
        let execution = require_exact_call(
            require_live_execution(connection, activated.session(), &self.targets).await?,
            call,
        )?;
        let failed = execution.fail_prepared_call(identities).map_err(|_| {
            ModelCallRepositoryError::InvalidTransition(
                "counted attachment failure requires the exact Prepared call",
            )
        })?;
        persist_failed_with_delegated_child_result(
            connection,
            &failed,
            TurnTerminalCause::AttachmentPreparationFailed,
            ProviderReportedTokenUsage::unreported(),
            None,
            Some(failure),
        )
        .await?;
        Ok(failed)
    }

    /// Commits Prepared while consuming the complete locked steering inventory.
    pub async fn prepare_initial_call<NextSteeringIdentities>(
        &self,
        session: SessionId,
        call: ModelCallId,
        failure_identities: FailedModelCallTurnIdentities,
        steering_frontier: signalbox_domain::ContextFrontierId,
        mut next_steering_identities: NextSteeringIdentities,
    ) -> Result<PrepareInitialModelCallOutcome, ModelCallRepositoryError>
    where
        NextSteeringIdentities:
            FnMut(AcceptedInputId) -> (signalbox_domain::SemanticTranscriptEntryId, TurnId),
    {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_delegated_child_endpoint_sessions(&mut transaction, session).await?;
            lock_session(&mut transaction, session).await?;
            let dispatch_start_lease_expired: bool =
                sqlx::query_scalar(crate::lock_inventory::EXPIRED_DISPATCH_START_LEASE)
                    .bind(session_id_to_uuid(session))
                    .fetch_one(&mut *transaction)
                    .await?;
            if dispatch_start_lease_expired {
                return Ok((false, PrepareInitialModelCallOutcome::NoWork));
            }
            let execution =
                require_live_execution(&mut transaction, session, &self.targets).await?;
            if execution.current_call().is_none()
                && let Some(delay) = load_availability_successor_backoff(
                    &mut transaction,
                    execution.current_attempt().id(),
                )
                .await?
            {
                return Ok((false, PrepareInitialModelCallOutcome::RetryBackoff(delay)));
            }
            if let Some(current_call) = execution.current_call() {
                return match current_call.state() {
                    signalbox_domain::CurrentModelCallState::Prepared => {
                        let current_call_id = current_call.id();
                        let request = execution.resume_prepared_call().map_err(|_| {
                            ModelCallRepositoryError::InvalidTransition(
                                "Prepared call could not resume",
                            )
                        })?;
                        let credential_reference = load_call_credential_reference(
                            &mut transaction,
                            session,
                            current_call_id,
                        )
                        .await?;
                        let dangerous_tool_auto_approval = execution
                            .active_turn()
                            .configuration()
                            .effective()
                            .dangerous_tool_auto_approval();
                        let system_prompt = load_frozen_epoch_system_prompt(
                            &mut transaction,
                            session,
                            execution
                                .active_turn()
                                .configuration()
                                .session_defaults_version(),
                        )
                        .await?;
                        let tool_entries =
                            load_tool_conversation_entries(&mut transaction, &request).await?;
                        let recorded_user_overrides =
                            load_call_user_overrides(&mut transaction, session, current_call_id)
                                .await?;
                        Ok((
                            false,
                            PrepareInitialModelCallOutcome::Ready {
                                request: Box::new(request),
                                credential_reference,
                                dangerous_tool_auto_approval,
                                recorded_user_overrides,
                                system_prompt,
                                tool_entries,
                            },
                        ))
                    }
                    signalbox_domain::CurrentModelCallState::InFlight
                    | signalbox_domain::CurrentModelCallState::CancellationRequested => {
                        Ok((false, PrepareInitialModelCallOutcome::NoWork))
                    }
                };
            }

            let mut reserved_entries = execution
                .frontier_entries()
                .map(signalbox_domain::SemanticTranscriptEntry::identity)
                .collect::<std::collections::BTreeSet<_>>();
            let mut steering_identities =
                Vec::with_capacity(execution.active_turn().pending_steering().len());
            for pending in execution.active_turn().pending_steering() {
                let accepted_input = pending.accepted_input();
                let (entry, turn) = next_steering_identities(accepted_input);
                if !reserved_entries.insert(entry) {
                    return Err(ModelCallRepositoryError::IdentityCollision(
                        ModelCallIdentityCollision::SemanticEntry,
                    ));
                }
                steering_identities.push((
                    entry,
                    PendingSteeringReclassificationIdentity::new(accepted_input, turn),
                ));
            }
            let steering_entries = steering_identities
                .iter()
                .map(|(entry, _)| *entry)
                .collect::<Vec<_>>();
            if !steering_entries.is_empty()
                && steering_frontier == execution.start().frontier().snapshot()
            {
                return Err(ModelCallRepositoryError::IdentityCollision(
                    ModelCallIdentityCollision::TerminalFrontier,
                ));
            }
            let steering_snapshot = (!steering_entries.is_empty()).then_some(steering_frontier);
            let fast_mode = execution
                .configuration()
                .effective()
                .model_settings()
                .effective()
                .fast_mode();
            let selected = if let Ok(resolved) = self
                .targets
                .resolve(*execution.configuration().effective().model())
            {
                let credential_reference = resolve_session_credential(
                    &mut transaction,
                    session,
                    resolved.target(),
                    fast_mode,
                    &self.credential_reference,
                    self.credential_families.as_ref(),
                )
                .await?;
                acquire_model_call_outbox_order_guard(&mut transaction).await?;
                let selected = Some(
                    select_runtime_pool_credential(
                        &mut transaction,
                        session,
                        execution.turn(),
                        execution.current_attempt().id(),
                        serving_pool_target(
                            self.credential_families.as_ref(),
                            resolved.target(),
                            fast_mode,
                        ),
                        credential_reference,
                        &self.credential_pools,
                    )
                    .await?,
                );
                outbox::lock_sequence_allocator(&mut transaction).await?;
                selected
            } else {
                None
            };
            if let Some(SelectedRuntimePoolCredential {
                reference: None,
                policy: Some(policy),
                ..
            }) = selected.as_ref()
            {
                let source_turn = execution.turn();
                let reclassifications = steering_identities
                    .iter()
                    .map(|(_, reclassification)| *reclassification)
                    .collect::<Vec<_>>();
                let mut proposed_turns = BTreeSet::new();
                for reclassification in &reclassifications {
                    record_reclassified_turn_candidate(
                        source_turn,
                        reclassification.turn(),
                        &mut proposed_turns,
                    )?;
                }
                let exhausted = execution
                    .fail_credential_pool_exhausted(
                        policy.name().to_owned(),
                        failure_identities
                            .clone()
                            .with_pending_steering_reclassifications(reclassifications),
                    )
                    .map_err(|_| {
                        ModelCallRepositoryError::InvalidTransition(
                            "credential-pool exhaustion could not close fresh execution state",
                        )
                    })?;
                persist_credential_pool_exhaustion(&mut transaction, &exhausted).await?;
                return Ok((
                    true,
                    PrepareInitialModelCallOutcome::PoolExhausted(Box::new(exhausted)),
                ));
            }
            let prepared = match execution.prepare_initial_call_consuming_steering(
                call,
                steering_entries,
                steering_snapshot,
            ) {
                Ok(prepared) => prepared,
                Err(error) if error.failure() == ModelCallPreparationFailure::TargetUnavailable => {
                    let resolution = error.target_resolution_error().ok_or(
                        ModelCallRepositoryError::InvalidTransition(
                            "target-unavailable result omitted its resolution proof",
                        ),
                    )?;
                    let source_turn = error.execution().turn();
                    let reclassifications = steering_identities
                        .into_iter()
                        .map(|(_, reclassification)| reclassification)
                        .collect::<Vec<_>>();
                    let mut proposed_turns = BTreeSet::new();
                    for reclassification in &reclassifications {
                        record_reclassified_turn_candidate(
                            source_turn,
                            reclassification.turn(),
                            &mut proposed_turns,
                        )?;
                    }
                    let failed = error
                        .execution()
                        .clone()
                        .fail_target_resolution(
                            resolution,
                            failure_identities
                                .with_pending_steering_reclassifications(reclassifications),
                        )
                        .map_err(|_| {
                            ModelCallRepositoryError::InvalidTransition(
                                "target-resolution failure could not close fresh execution state",
                            )
                        })?;
                    persist_failed_with_delegated_child_result(
                        &mut transaction,
                        &failed,
                        TurnTerminalCause::ModelTargetUnavailable,
                        ProviderReportedTokenUsage::unreported(),
                        None,
                        None,
                    )
                    .await?;
                    return Ok((
                        true,
                        PrepareInitialModelCallOutcome::TargetUnavailable(Box::new(failed)),
                    ));
                }
                Err(_) => {
                    return Err(ModelCallRepositoryError::InvalidTransition(
                        "initial call cannot be prepared",
                    ));
                }
            };
            let selected = selected.ok_or(ModelCallRepositoryError::InvalidTransition(
                "resolved initial call omitted credential selection",
            ))?;
            let credential_reference =
                selected
                    .reference
                    .as_ref()
                    .ok_or(ModelCallRepositoryError::InvalidTransition(
                        "admitted credential pool omitted its selected member",
                    ))?;
            insert_prepared_call(
                &mut transaction,
                &prepared,
                credential_reference,
                selected.policy.as_ref(),
                self.cache_inclusive_input_targets
                    .contains(&prepared.call().target()),
            )
            .await?;
            consume_pool_member_actions(
                &mut transaction,
                prepared.turn(),
                &selected.pending_consumed_actions,
            )
            .await?;
            let reloaded = require_exact_call(
                require_live_execution(&mut transaction, session, &self.targets).await?,
                call,
            )?;
            reloaded.resume_prepared_call().map_err(|_| {
                ModelCallCorruption::Inconsistent("committed Prepared call cannot resume")
            })?;
            Ok((true, PrepareInitialModelCallOutcome::Checkpointed(call)))
        }
        .await;

        finish_optional_commit(transaction, result).await
    }

    /// Atomically authorizes the exact Prepared call and attempt for send.
    pub async fn authorize_send(
        &self,
        session: SessionId,
        call: ModelCallId,
    ) -> Result<AuthorizeModelCallOutcome, ModelCallRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            if let Err(error) = lock_session(&mut transaction, session).await {
                return match error {
                    ModelCallRepositoryError::NoLiveExecution => {
                        Ok((false, AuthorizeModelCallOutcome::NoSend))
                    }
                    error => Err(error),
                };
            }
            let execution =
                match require_live_execution(&mut transaction, session, &self.targets).await {
                    Ok(execution) => execution,
                    Err(ModelCallRepositoryError::NoLiveExecution) => {
                        return Ok((false, AuthorizeModelCallOutcome::NoSend));
                    }
                    Err(error) => return Err(error),
                };
            if !matches!(
                execution.current_call(),
                Some(current)
                    if current.id() == call
                        && current.state()
                            == signalbox_domain::CurrentModelCallState::Prepared
            ) {
                return Ok((false, AuthorizeModelCallOutcome::NoSend));
            }
            let authorized = execution.authorize_send().map_err(|_| {
                ModelCallCorruption::Inconsistent("checked Prepared call could not authorize send")
            })?;
            persist_authorization(&mut transaction, &authorized).await?;
            Ok((
                true,
                AuthorizeModelCallOutcome::Authorized(Box::new(authorized)),
            ))
        }
        .await;
        finish_optional_commit(transaction, result).await
    }

    /// Freshly reloads issued authority and commits one terminal observation.
    pub async fn apply_terminal_observation<NextTurn>(
        &self,
        session: SessionId,
        observation: CorrelatedModelCallTerminalObservation,
        identities: ModelCallTerminalIdentities,
        next_reclassified_turn: NextTurn,
    ) -> Result<ModelCallTerminalOutcome, ModelCallRepositoryError>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        let outcome = self
            .apply_terminal_observation_candidates(
                session,
                observation,
                ModelCallTerminalIdentityCandidates::Exact(identities),
                next_reclassified_turn,
            )
            .await?
            .ok_or(ModelCallRepositoryError::InvalidTransition(
                "provider observation was discarded by logical delegation terminalization",
            ))?;
        match outcome {
            ModelCallObservationCommitOutcome::Terminal(outcome) => Ok(*outcome),
            ModelCallObservationCommitOutcome::AvailabilitySuccessor(_) => {
                Err(ModelCallRepositoryError::InvalidTransition(
                    "exact terminal candidates produced an availability successor",
                ))
            }
            ModelCallObservationCommitOutcome::PoolExhausted(_) => {
                Err(ModelCallRepositoryError::InvalidTransition(
                    "exact terminal candidates produced pool exhaustion",
                ))
            }
        }
    }

    async fn apply_terminal_observation_candidates<NextTurn>(
        &self,
        session: SessionId,
        observation: CorrelatedModelCallTerminalObservation,
        identities: ModelCallTerminalIdentityCandidates,
        mut next_reclassified_turn: NextTurn,
    ) -> Result<Option<ModelCallObservationCommitOutcome>, ModelCallRepositoryError>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            if locked_delegation_logical_terminal(&mut transaction, session, observation.call())
                .await?
            {
                return Ok(None);
            }
            let execution = require_exact_call(
                require_live_execution(&mut transaction, session, &self.targets).await?,
                observation.call(),
            )?;
            let identities = select_terminal_identity_candidates(identities, &execution);
            let identities = attach_pending_reclassification_candidates(
                identities,
                &execution,
                &mut next_reclassified_turn,
            )?;
            let usage = observation.usage();
            let provider_failure_cause = observation.provider_failure_cause();
            let retry_after = observation.retry_after();
            if let ModelCallTerminalIdentityCandidates::Availability {
                failed,
                successor_attempt,
            } = identities
            {
                let cause =
                    provider_failure_cause.ok_or(ModelCallRepositoryError::InvalidTransition(
                        "availability candidates require a classified provider failure",
                    ))?;
                let policy =
                    load_call_pool_policy(&mut transaction, observation.call().into_uuid()).await?;
                let Some(policy) = policy else {
                    outbox::lock_sequence_allocator(&mut transaction).await?;
                    // The call carried no credential pool, so no configured
                    // action governs this availability cause. Close the turn on
                    // the ordinary terminal path rather than failing the commit.
                    let outcome = execution
                        .apply_terminal_observation(
                            observation,
                            ModelCallTerminalIdentities::Failed(failed),
                        )
                        .map_err(|_| {
                            ModelCallRepositoryError::InvalidTransition(
                                "terminal observation does not match fresh issued state",
                            )
                        })?;
                    persist_terminal_outcome_with_usage(
                        &mut transaction,
                        &outcome,
                        Some(TurnTerminalCause::ModelCallFailed),
                        usage,
                        provider_failure_cause,
                    )
                    .await?;
                    return Ok(Some(ModelCallObservationCommitOutcome::Terminal(Box::new(
                        outcome,
                    ))));
                };
                acquire_model_call_outbox_order_guard(&mut transaction).await?;
                lock_credential_pool_action_heads(&mut transaction, &policy).await?;
                outbox::lock_sequence_allocator(&mut transaction).await?;
                let action = policy.action(cause);
                let mut pool_exhausted_name = None;
                let current_reference = sqlx::query_scalar::<_, String>(
                    "SELECT credential_reference
                       FROM model_call
                      WHERE model_call_id = $1",
                )
                .bind(observation.call().into_uuid())
                .fetch_one(&mut *transaction)
                .await?;
                // A successor reissues the request, so it needs the
                // adapter's proof that the failed request was never accepted.
                // Without it the call closes terminally rather than
                // substituting a member behind an effect that may have landed.
                // A stop already requested on this attempt forbids the reissue
                // outright: the successor would reload an attempt the domain
                // admits only while running.
                let stop_requested = matches!(
                    execution.current_attempt().state(),
                    signalbox_domain::CurrentTurnAttemptState::StopRequested { .. }
                );
                let same_credential_attempts = count_turn_credential_attempts(
                    &mut transaction,
                    session,
                    observation.correlation().turn(),
                    &current_reference,
                )
                .await?;
                let retry_candidate = is_same_credential_retry_cause(cause)
                    && same_credential_attempts < self.same_credential_attempt_bound.get()
                    && observation.non_acceptance_proven()
                    && !stop_requested;
                let rotation_candidate = action == CredentialPoolRuntimeAction::SwitchNow
                    && observation.non_acceptance_proven()
                    && !stop_requested;
                let mut durable_exclusions = if retry_candidate || rotation_candidate {
                    Some(
                        load_durable_pool_exclusions(
                            &mut transaction,
                            session,
                            observation.correlation().turn(),
                            &policy,
                        )
                        .await?,
                    )
                } else {
                    None
                };
                let retrying_same_credential = retry_candidate
                    && durable_exclusions.as_ref().is_some_and(|exclusions| {
                        !exclusions.excluded.contains(&current_reference)
                    });
                let rotating = !retrying_same_credential && rotation_candidate;
                if retrying_same_credential || rotating {
                    let Some(DurablePoolExclusions { mut excluded, .. }) =
                        durable_exclusions.take()
                    else {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "availability successor omitted pool exclusions",
                        ));
                    };
                    if rotating {
                        sqlx::query(
                            "INSERT INTO credential_pool_chain_exclusion
                            (session_id, turn_id, credential_reference,
                             predecessor_model_call_id, cause_kind)
                         VALUES ($1, $2, $3, $4, $5)
                         ON CONFLICT (session_id, turn_id, credential_reference) DO NOTHING",
                        )
                        .bind(session_id_to_uuid(session))
                        .bind(turn_id_to_uuid(observation.correlation().turn()))
                        .bind(&current_reference)
                        .bind(observation.call().into_uuid())
                        .bind(encode_provider_failure_cause(cause))
                        .execute(&mut *transaction)
                        .await?;
                        excluded.insert(current_reference.clone());
                    }
                    pool_exhausted_name = Some(Arc::<str>::from(policy.name()));
                    if policy
                        .members()
                        .iter()
                        .any(|member| !excluded.contains(member.credential_reference()))
                    {
                        let backoff = availability_retry_backoff(
                            cause,
                            retry_after,
                            if retrying_same_credential {
                                same_credential_attempts
                            } else {
                                1
                            },
                            observation.call(),
                        );
                        let successor = execution
                            .apply_availability_successor(observation, successor_attempt)
                            .map_err(|_| {
                                ModelCallRepositoryError::InvalidTransition(
                                    "availability successor does not match fresh issued state",
                                )
                            })?;
                        persist_availability_successor(
                            &mut transaction,
                            &successor,
                            usage,
                            cause,
                            backoff,
                        )
                        .await?;
                        return Ok(Some(
                            ModelCallObservationCommitOutcome::AvailabilitySuccessor(Box::new(
                                AvailabilitySuccessorOutcome::new(successor, backoff),
                            )),
                        ));
                    }
                    insert_credential_pool_terminal_exhaustion(
                        &mut transaction,
                        observation.correlation().attempt(),
                        session,
                        observation.correlation().turn(),
                        policy.name(),
                        Some(observation.call()),
                        Some(cause),
                    )
                    .await?;
                } else if action != CredentialPoolRuntimeAction::Stay
                    && action != CredentialPoolRuntimeAction::SwitchNow
                {
                    persist_credential_pool_member_action(
                        &mut transaction,
                        &policy,
                        action,
                        current_reference,
                        &observation,
                        cause,
                    )
                    .await?;
                }
                let outcome = execution
                    .apply_terminal_observation(
                        observation,
                        ModelCallTerminalIdentities::Failed(failed),
                    )
                    .map_err(|_| {
                        ModelCallRepositoryError::InvalidTransition(
                            "terminal observation does not match fresh issued state",
                        )
                    })?;
                // Exhausting the pool's last member is why this turn ended,
                // so the durable exhaustion record and the cause agree.
                let terminal_cause = match pool_exhausted_name {
                    Some(_) => TurnTerminalCause::CredentialPoolExhausted,
                    None => TurnTerminalCause::ModelCallFailed,
                };
                persist_terminal_outcome_with_usage(
                    &mut transaction,
                    &outcome,
                    Some(terminal_cause),
                    usage,
                    provider_failure_cause,
                )
                .await?;
                if let Some(pool_name) = pool_exhausted_name {
                    return Ok(Some(ModelCallObservationCommitOutcome::PoolExhausted(
                        CredentialPoolExhaustedOutcome::AfterCall {
                            pool_name,
                            terminal: Box::new(outcome),
                        },
                    )));
                }
                return Ok(Some(ModelCallObservationCommitOutcome::Terminal(Box::new(
                    outcome,
                ))));
            }
            let ModelCallTerminalIdentityCandidates::Exact(identities) = identities else {
                return Err(ModelCallRepositoryError::InvalidTransition(
                    "terminal candidate selection retained a nonterminal alternative",
                ));
            };
            let outcome = execution
                .apply_terminal_observation(observation, identities)
                .map_err(|_| {
                    ModelCallRepositoryError::InvalidTransition(
                        "terminal observation does not match fresh issued state",
                    )
                })?;
            persist_terminal_outcome_with_usage(
                &mut transaction,
                &outcome,
                Some(TurnTerminalCause::ModelCallFailed),
                usage,
                provider_failure_cause,
            )
            .await?;
            Ok(Some(ModelCallObservationCommitOutcome::Terminal(Box::new(
                outcome,
            ))))
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Atomically closes a trustworthy prepared failure before send.
    pub async fn fail_prepared_call<NextTurn>(
        &self,
        session: SessionId,
        call: ModelCallId,
        cause: PreparedModelCallFailureCause,
        attachment_failure: Option<AttachmentPreparationFailure>,
        identities: FailedModelCallTurnIdentities,
        mut next_reclassified_turn: NextTurn,
    ) -> Result<FailedModelCallTurn, ModelCallRepositoryError>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_model_call_terminal_frontier(&mut transaction, session, call).await?;
            let execution = require_exact_call(
                require_live_execution(&mut transaction, session, &self.targets).await?,
                call,
            )?;
            let reclassifications =
                pending_reclassification_candidates(&execution, &mut next_reclassified_turn)?;
            let failed = execution
                .fail_prepared_call(
                    identities.with_pending_steering_reclassifications(reclassifications),
                )
                .map_err(|_| {
                    ModelCallRepositoryError::InvalidTransition(
                        "prepared failure requires a Prepared call",
                    )
                })?;
            persist_failed_with_delegated_child_result(
                &mut transaction,
                &failed,
                prepared_failure_cause(cause, attachment_failure),
                ProviderReportedTokenUsage::unreported(),
                None,
                attachment_failure,
            )
            .await?;
            Ok(failed)
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Closes a freshly activated call-free turn after required automatic
    /// context compaction failed in the same transaction.
    pub(crate) async fn fail_automatic_compaction_in_transaction(
        &self,
        connection: &mut PgConnection,
        session: SessionId,
        turn: TurnId,
        identities: FailedModelCallTurnIdentities,
        terminal_cause: TurnTerminalCause,
        recovery_cause: Option<crate::goal::GoalExecutionFailureRecoveryCause>,
    ) -> Result<FailedModelCallTurn, ModelCallRepositoryError> {
        let execution = require_live_execution(connection, session, &self.targets).await?;
        if execution.turn() != turn || execution.current_call().is_some() {
            return Err(ModelCallRepositoryError::InvalidTransition(
                "automatic compaction failure does not match fresh call-free execution",
            ));
        }
        let failed = execution
            .fail_automatic_context_compaction(identities)
            .map_err(|_| {
                ModelCallRepositoryError::InvalidTransition(
                    "automatic compaction failure could not close fresh execution",
                )
            })?;
        persist_failed_with_delegated_child_result(
            connection,
            &failed,
            terminal_cause,
            ProviderReportedTokenUsage::unreported(),
            None,
            None,
        )
        .await?;
        if let Some(cause) = recovery_cause {
            crate::goal::record_execution_failure_recovery_cause(connection, session, turn, cause)
                .await?;
        }
        Ok(failed)
    }

    /// Rereads whether an unchanged pre-send prepared failure committed.
    pub async fn reread_prepared_failure(
        &self,
        session: SessionId,
        call: ModelCallId,
        attachment_failure: Option<AttachmentPreparationFailure>,
    ) -> Result<RetainedPreparedFailureStatus, ModelCallRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_session(&mut transaction, session).await?;
            let stored = sqlx::query_as::<
                _,
                (
                    Uuid,
                    Uuid,
                    Uuid,
                    String,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                    Option<Decimal>,
                ),
            >(
                "SELECT turn_id, turn_attempt_id, context_frontier_id, state_kind,
                        terminal_disposition_kind, terminal_provider_failure_cause,
                        terminal_attachment_preparation_failure_cause,
                        terminal_attachment_preparation_failure_maximum_bytes
                   FROM model_call
                  WHERE session_id = $1
                    AND model_call_id = $2",
            )
            .bind(session_id_to_uuid(session))
            .bind(call.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(ModelCallCorruption::Missing(
                "retained prepared-failure model call",
            ))?;
            let (
                turn,
                attempt,
                source_frontier,
                state,
                disposition,
                provider_failure_cause,
                stored_attachment_failure,
                stored_attachment_maximum,
            ) = stored;
            match (state.as_str(), disposition.as_deref()) {
                ("prepared", None) => {
                    let execution = require_exact_call(
                        require_live_execution(&mut transaction, session, &self.targets).await?,
                        call,
                    )?;
                    execution.resume_prepared_call().map_err(|_| {
                        ModelCallRepositoryError::InvalidTransition(
                            "retained prepared failure could not resume Prepared",
                        )
                    })?;
                    Ok(RetainedPreparedFailureStatus::Pending)
                }
                ("terminal", Some("known_failed")) => {
                    let (expected_attachment_failure, expected_attachment_maximum) =
                        match attachment_failure {
                            Some(AttachmentPreparationFailure::TooLarge { maximum_bytes }) => (
                                Some("too_large"),
                                Some(Decimal::from(maximum_bytes)),
                            ),
                            Some(AttachmentPreparationFailure::Missing) => (Some("missing"), None),
                            Some(AttachmentPreparationFailure::Corrupt) => (Some("corrupt"), None),
                            Some(AttachmentPreparationFailure::Unavailable) => {
                                return Err(ModelCallRepositoryError::InvalidTransition(
                                    "retryable attachment unavailability cannot have a terminal closure",
                                ));
                            }
                            None => (None, None),
                        };
                    if provider_failure_cause.is_some()
                        || stored_attachment_failure.as_deref() != expected_attachment_failure
                        || stored_attachment_maximum != expected_attachment_maximum
                    {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "retained capability failure durable cause changed",
                        ));
                    }
                    let transition_history_matches = sqlx::query_scalar::<_, bool>(
                        "SELECT
                            EXISTS (
                                SELECT 1
                                  FROM model_call_transition_outbox_event
                                 WHERE session_id = $1
                                   AND model_call_id = $3
                                   AND turn_id = $2
                                   AND call_state_kind = 'prepared'
                                   AND terminal_disposition_kind IS NULL
                            )
                            AND NOT EXISTS (
                                SELECT 1
                                  FROM model_call_transition_outbox_event
                                 WHERE session_id = $1
                                   AND model_call_id = $3
                                   AND turn_id = $2
                                   AND call_state_kind = 'in_flight'
                            )
                            AND EXISTS (
                                SELECT 1
                                  FROM model_call_transition_outbox_event
                                 WHERE session_id = $1
                                   AND model_call_id = $3
                                   AND turn_id = $2
                                   AND call_state_kind = 'terminal'
                                   AND terminal_disposition_kind = 'known_failed'
                            )",
                    )
                    .bind(session_id_to_uuid(session))
                    .bind(turn)
                    .bind(call.into_uuid())
                    .fetch_one(&mut *transaction)
                    .await?;
                    let closure_matches = failed_turn_closure_matches(
                        &mut transaction,
                        session,
                        turn,
                        attempt,
                        call.into_uuid(),
                        source_frontier,
                    )
                    .await?;
                    let delegated_result_matches = delegated_terminal_result_matches(
                        &mut transaction,
                        session,
                        turn_id_from_uuid(turn),
                        &ExpectedDelegatedChildResult::Failed,
                    )
                    .await?;
                    if transition_history_matches && closure_matches && delegated_result_matches {
                        Ok(RetainedPreparedFailureStatus::AlreadyCommitted)
                    } else {
                        Err(ModelCallRepositoryError::InvalidTransition(
                            "retained prepared failure durable closure is incomplete",
                        ))
                    }
                }
                ("terminal", Some("cancelled")) => {
                    if prepared_cancellation_closure_matches(
                        &mut transaction,
                        session,
                        turn,
                        attempt,
                        call.into_uuid(),
                        source_frontier,
                    )
                    .await?
                    {
                        Ok(RetainedPreparedFailureStatus::Cancelled)
                    } else {
                        Err(ModelCallRepositoryError::InvalidTransition(
                            "retained prepared failure cancellation closure is incomplete",
                        ))
                    }
                }
                _ => Err(ModelCallRepositoryError::InvalidTransition(
                    "retained prepared failure durable state changed",
                )),
            }
        }
        .await;
        transaction.rollback().await?;
        result
    }

    /// Rereads exact durable authority after an ambiguous authorization commit.
    pub async fn reread_ambiguous_authorization(
        &self,
        session: SessionId,
        prepared: &signalbox_domain::PreparedModelCallRequest,
    ) -> Result<ModelCallAuthorizationReread, ModelCallRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_session(&mut transaction, session).await?;
            let stored = sqlx::query(
                "SELECT call.model_call_id, call.turn_id, call.turn_attempt_id,
                        call.selection_kind, call.direct_model_selection_id,
                        call.frozen_model_alias_id, call.frozen_alias_selected_direct_id,
                        call.resolved_provider_model_identity_id, call.context_frontier_id,
                        call.state_kind, call.terminal_disposition_kind,
                        manifest.turn_instruction_manifest_id,
                        manifest.boundary_kind AS instruction_manifest_boundary_kind,
                        manifest.eligibility_hash_algorithm
                            AS instruction_eligibility_hash_algorithm,
                        manifest.eligibility_hash AS instruction_eligibility_hash,
                        manifest.admitted_set_hash_algorithm
                            AS instruction_admitted_set_hash_algorithm,
                        manifest.admitted_set_hash AS instruction_admitted_set_hash,
                        manifest.manifest_hash_algorithm
                            AS instruction_manifest_hash_algorithm,
                        manifest.manifest_hash AS instruction_manifest_hash,
                        discovery.scan_complete AS instruction_discovery_complete
                   FROM model_call AS call
              LEFT JOIN turn_instruction_manifest AS manifest
                     ON manifest.turn_instruction_manifest_id = call.turn_instruction_manifest_id
                    AND manifest.session_id = call.session_id
                    AND manifest.turn_id = call.turn_id
              LEFT JOIN instruction_discovery AS discovery
                     ON discovery.instruction_discovery_id = manifest.instruction_discovery_id
                  WHERE call.session_id = $1
                    AND call.model_call_id = $2",
            )
            .bind(session_id_to_uuid(session))
            .bind(prepared.call().id().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(ModelCallCorruption::Missing(
                "ambiguous authorization model call",
            ))?;
            let stored = decode_model_call(stored, session)?;
            if stored.state()
                == ModelCallReconstitutionState::Terminal(ModelCallDisposition::Cancelled)
            {
                let stored_members =
                    load_frontier_members(&mut transaction, session, stored.frontier().into_uuid())
                        .await?;
                let exact_request = prepared.session() == session
                    && prepared.turn() == stored.turn()
                    && prepared.attempt() == stored.attempt()
                    && prepared.call().id() == stored.id()
                    && prepared.call().selection() == stored.selection()
                    && prepared.call().target() == stored.target()
                    && prepared.call().frontier().snapshot() == stored.frontier()
                    && prepared
                        .frontier_entries()
                        .map(|entry| {
                            (
                                session_id_to_uuid(entry.source_session()),
                                entry.identity().into_uuid(),
                            )
                        })
                        .eq(stored_members);
                if !exact_request {
                    return Err(ModelCallRepositoryError::InvalidTransition(
                        "ambiguous authorization reread changed terminal request",
                    ));
                }
                if prepared_cancellation_closure_matches(
                    &mut transaction,
                    session,
                    stored.turn().into_uuid(),
                    stored.attempt().into_uuid(),
                    stored.id().into_uuid(),
                    stored.frontier().into_uuid(),
                )
                .await?
                {
                    return Ok(ModelCallAuthorizationReread::Cancelled);
                }
                return Err(ModelCallRepositoryError::InvalidTransition(
                    "ambiguous authorization terminal cancellation closure is incomplete",
                ));
            }
            let execution = require_exact_call(
                require_live_execution(&mut transaction, session, &self.targets).await?,
                prepared.call().id(),
            )?;
            match execution
                .current_call()
                .map(signalbox_domain::CurrentModelCall::state)
            {
                Some(signalbox_domain::CurrentModelCallState::Prepared) => {
                    let reloaded = execution.resume_prepared_call().map_err(|_| {
                        ModelCallRepositoryError::InvalidTransition(
                            "ambiguous authorization reread could not resume Prepared",
                        )
                    })?;
                    if &reloaded != prepared {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "ambiguous authorization reread changed Prepared request",
                        ));
                    }
                    Ok(ModelCallAuthorizationReread::Prepared)
                }
                Some(signalbox_domain::CurrentModelCallState::InFlight) => {
                    let authorized = execution.resume_in_flight_call().ok_or(
                        ModelCallRepositoryError::InvalidTransition(
                            "ambiguous authorization reread could not resume InFlight",
                        ),
                    )?;
                    if !prepared_matches_authorized(prepared, &authorized) {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "ambiguous authorization reread changed issued request",
                        ));
                    }
                    Ok(ModelCallAuthorizationReread::InFlight(Box::new(authorized)))
                }
                Some(signalbox_domain::CurrentModelCallState::CancellationRequested) => {
                    let stopped = execution.resume_cancellation_requested_call().ok_or(
                        ModelCallRepositoryError::InvalidTransition(
                            "ambiguous authorization reread could not resume CancellationRequested",
                        ),
                    )?;
                    if !prepared_matches_stopped(prepared, &execution, &stopped) {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "ambiguous authorization reread changed stopped request",
                        ));
                    }
                    Ok(ModelCallAuthorizationReread::CancellationRequested(
                        Box::new(stopped),
                    ))
                }
                None => Err(ModelCallRepositoryError::InvalidTransition(
                    "ambiguous authorization reread found no resumable call",
                )),
            }
        }
        .await;
        transaction.rollback().await?;
        result
    }

    /// Rereads whether an unchanged terminal observation already committed.
    pub async fn reread_terminal_observation(
        &self,
        session: SessionId,
        observation: &CorrelatedModelCallTerminalObservation,
    ) -> Result<RetainedModelCallObservationStatus, ModelCallRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            let correlation = observation.correlation();
            if correlation.session() != session {
                return Err(ModelCallRepositoryError::InvalidTransition(
                    "retained observation session changed",
                ));
            }
            let delegation_logically_terminal =
                locked_delegation_logical_terminal(&mut transaction, session, observation.call())
                    .await?;
            let stored_row = sqlx::query(
                "SELECT session_id, turn_id, turn_attempt_id,
                        resolved_provider_model_identity_id, context_frontier_id,
                        state_kind, terminal_disposition_kind,
                        terminal_provider_failure_cause,
                        usage_input_tokens, usage_output_tokens,
                        usage_cache_creation_input_tokens,
                        usage_cache_read_input_tokens
                   FROM model_call
                  WHERE model_call_id = $1",
            )
            .bind(observation.call().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(ModelCallCorruption::Missing(
                "retained observation model call",
            ))?;
            let stored = decode_stored_model_call_observation(&stored_row)?;
            if stored.session != session_id_to_uuid(correlation.session())
                || stored.turn != turn_id_to_uuid(correlation.turn())
                || stored.attempt != correlation.attempt().into_uuid()
                || stored.target != correlation.target().identity().into_uuid()
                || stored.frontier != correlation.frontier().into_uuid()
            {
                return Err(ModelCallRepositoryError::InvalidTransition(
                    "retained observation correlation changed",
                ));
            }
            if delegation_logically_terminal {
                return Ok(RetainedModelCallObservationStatus::DiscardedByLogicalTerminal);
            }
            match (stored.state.as_str(), stored.disposition.as_deref()) {
                ("in_flight", None) => {
                    let execution = require_exact_call(
                        require_live_execution(&mut transaction, session, &self.targets).await?,
                        observation.call(),
                    )?;
                    let authorized = execution.resume_in_flight_call().ok_or(
                        ModelCallRepositoryError::InvalidTransition(
                            "retained observation could not resume issued call",
                        ),
                    )?;
                    if authorized.observation_correlation() != *correlation {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "retained observation issued authority changed",
                        ));
                    }
                    Ok(RetainedModelCallObservationStatus::Pending)
                }
                ("cancellation_requested", None) => {
                    let retained_stop = sqlx::query_scalar::<_, bool>(
                        "SELECT EXISTS (
                            SELECT 1
                              FROM turn_lifecycle AS lifecycle
                              JOIN turn_attempt AS attempt
                                ON attempt.turn_attempt_id =
                                    lifecycle.current_attempt_id
                               AND attempt.turn_id = lifecycle.turn_id
                               AND attempt.session_id = lifecycle.session_id
                               AND attempt.state_kind = 'stop_requested'
                               AND attempt.interrupt_command_id IS NOT NULL
                              JOIN model_call_transition_outbox_event AS event
                                ON event.session_id = lifecycle.session_id
                               AND event.turn_id = lifecycle.turn_id
                               AND event.model_call_id = $3
                               AND event.call_state_kind =
                                   'cancellation_requested'
                             WHERE lifecycle.session_id = $1
                               AND lifecycle.turn_id = $2
                               AND lifecycle.state_kind = 'active'
                               AND lifecycle.active_phase_kind = 'running'
                        )",
                    )
                    .bind(session_id_to_uuid(session))
                    .bind(stored.turn)
                    .bind(observation.call().into_uuid())
                    .fetch_one(&mut *transaction)
                    .await?;
                    if !retained_stop {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "retained observation stop authority changed",
                        ));
                    }
                    Ok(RetainedModelCallObservationStatus::Pending)
                }
                ("terminal", Some(stored_disposition))
                    if stored_disposition
                        == encode_disposition(observation.observation().disposition())
                        && stored.provider_failure_cause.as_deref()
                            == observation
                                .provider_failure_cause()
                                .map(encode_provider_failure_cause)
                        && stored.usage == encode_token_usage(observation.usage()) =>
                {
                    // A commit-ambiguous driver error can hide a commit that
                    // durably created an availability successor. The
                    // predecessor is then terminal while its turn stays active
                    // on the successor attempt, which is not the terminal
                    // failed turn the ordinary closure predicate requires.
                    if let Some(retry_backoff) = committed_availability_successor_backoff(
                        &mut transaction,
                        observation.call(),
                    )
                    .await?
                    {
                        return Ok(
                            RetainedModelCallObservationStatus::AvailabilitySuccessorCommitted {
                                retry_backoff,
                            },
                        );
                    }
                    if !terminal_observation_closure_matches(&mut transaction, session, observation)
                        .await?
                    {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "retained observation terminal closure changed",
                        ));
                    }
                    if !delegated_observation_result_matches(&mut transaction, session, observation)
                        .await?
                    {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "retained observation delegated result closure changed",
                        ));
                    }
                    Ok(RetainedModelCallObservationStatus::AlreadyCommitted)
                }
                _ => Err(ModelCallRepositoryError::InvalidTransition(
                    "retained observation durable state changed",
                )),
            }
        }
        .await;
        transaction.rollback().await?;
        result
    }

    /// Applies the accepted prior-process recovery rule to one live call.
    pub async fn recover_after_restart(
        &self,
        session: SessionId,
        call: ModelCallId,
        identities: FailedModelCallTurnIdentities,
    ) -> Result<ModelCallTerminalOutcome, ModelCallRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_model_call_terminal_frontier(&mut transaction, session, call).await?;
            let execution = require_exact_call(
                require_live_execution_for_restart(&mut transaction, session).await?,
                call,
            )?;
            let outcome = execution.recover_after_restart(identities).map_err(|_| {
                ModelCallRepositoryError::InvalidTransition(
                    "startup recovery requires a live Prepared or issued call",
                )
            })?;
            persist_terminal_outcome(
                &mut transaction,
                &outcome,
                Some(TurnTerminalCause::AbandonedAtRestart),
            )
            .await?;
            Ok(outcome)
        }
        .await;
        finish_commit(transaction, result).await
    }
}

enum ExpectedDelegatedChildResult {
    Returned(String),
    Failed,
    Cancelled,
    ResultUnavailable,
}

struct StoredDelegatedChildResultFacts<'a> {
    outcome: DelegationOutcomeKind,
    reason: DelegationOutcomeReason,
    content: Option<&'a str>,
}

impl ExpectedDelegatedChildResult {
    fn stored_facts(&self) -> StoredDelegatedChildResultFacts<'_> {
        match self {
            Self::Returned(content) => StoredDelegatedChildResultFacts {
                outcome: DelegationOutcomeKind::ResultReturned,
                reason: DelegationOutcomeReason::ChildCompleted,
                content: Some(content.as_str()),
            },
            Self::Failed => StoredDelegatedChildResultFacts {
                outcome: DelegationOutcomeKind::ChildFailed,
                reason: DelegationOutcomeReason::ChildExecutionFailed,
                content: None,
            },
            Self::Cancelled => StoredDelegatedChildResultFacts {
                outcome: DelegationOutcomeKind::ChildCancelled,
                reason: DelegationOutcomeReason::ChildCancelled,
                content: None,
            },
            Self::ResultUnavailable => StoredDelegatedChildResultFacts {
                outcome: DelegationOutcomeKind::ChildFailed,
                reason: DelegationOutcomeReason::ChildResultUnavailable,
                content: None,
            },
        }
    }
}

async fn delegated_observation_result_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    let expected = match observation.observation() {
        ModelCallTerminalObservation::Completed { assistant_text } => {
            match signalbox_domain::DelegationContent::from_assistant_text(assistant_text) {
                Ok(content) => ExpectedDelegatedChildResult::Returned(content.as_str().to_owned()),
                Err(_) => ExpectedDelegatedChildResult::ResultUnavailable,
            }
        }
        ModelCallTerminalObservation::CompletedWithProviderCompaction { response } => {
            let assistant_text = response
                .iter()
                .filter_map(|part| match part {
                    AssistantResponsePart::Text(text) => Some(text.clone()),
                    AssistantResponsePart::ProviderCompaction(_) => None,
                    AssistantResponsePart::ToolCall(_) => None,
                })
                .collect::<Vec<_>>();
            match signalbox_domain::DelegationContent::from_assistant_text(&assistant_text) {
                Ok(content) => ExpectedDelegatedChildResult::Returned(content.as_str().to_owned()),
                Err(_) => ExpectedDelegatedChildResult::ResultUnavailable,
            }
        }
        ModelCallTerminalObservation::KnownFailed | ModelCallTerminalObservation::Refused => {
            ExpectedDelegatedChildResult::Failed
        }
        ModelCallTerminalObservation::Cancelled => {
            if cancelled_terminal_closure_matches(connection, session, observation).await? {
                ExpectedDelegatedChildResult::Cancelled
            } else {
                ExpectedDelegatedChildResult::Failed
            }
        }
        ModelCallTerminalObservation::CompletedWithTools { .. }
        | ModelCallTerminalObservation::Ambiguous => {
            return delegated_nonterminal_result_absent(
                connection,
                session,
                observation.correlation().turn(),
            )
            .await;
        }
    };
    delegated_terminal_result_matches(
        connection,
        session,
        observation.correlation().turn(),
        &expected,
    )
    .await
}

async fn delegated_terminal_result_matches(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    expected: &ExpectedDelegatedChildResult,
) -> Result<bool, ModelCallRepositoryError> {
    let stored = expected.stored_facts();
    let outcome_kind = delegation_outcome_kind_to_str(stored.outcome);
    let reason_kind = delegation_outcome_reason_to_str(stored.reason).ok_or(
        ModelCallCorruption::Inconsistent("delegated child result reason"),
    )?;
    Ok(sqlx::query_scalar::<_, bool>(
        "WITH delegated AS (
            SELECT task.spawning_tool_request_id,
                   task.child_session_id,
                   task.turn_id,
                   relation.parent_session_id
              FROM session_delegation_initial_task AS task
              JOIN session_delegation AS relation
                ON relation.spawning_tool_request_id = task.spawning_tool_request_id
               AND relation.child_session_id = task.child_session_id
             WHERE task.child_session_id = $1
               AND task.turn_id = $2
        ),
        origin AS (
            SELECT EXISTS (
                       SELECT 1
                         FROM turn_lifecycle
                        WHERE session_id = $1
                          AND turn_id = $2
                          AND origin_kind = 'delegation'
                   ) AS delegated,
                   EXISTS (
                       SELECT 1
                         FROM session_delegation_initial_task
                        WHERE child_session_id = $1
                          AND turn_id = $2
                   ) AS initial_task,
                   EXISTS (
                       SELECT 1
                         FROM session_delegation_wake_turn_origin
                        WHERE recipient_session_id = $1
                          AND turn_id = $2
                   ) AS wake
        )
        SELECT NOT origin.delegated
            OR (origin.wake AND NOT origin.initial_task)
            OR (
                origin.initial_task
                AND NOT origin.wake
                AND (
                    SELECT count(*) = 1
                      FROM delegated
                      JOIN session_child_result AS result
                    ON result.spawning_tool_request_id =
                       delegated.spawning_tool_request_id
                   AND result.event_kind = 'outcome_recorded'
                   AND result.outcome_kind = $3
                   AND result.content_text IS NOT DISTINCT FROM $5
                  JOIN session_delegation_event AS outcome
                    ON outcome.spawning_tool_request_id =
                       result.spawning_tool_request_id
                   AND outcome.event_ordinal = result.event_ordinal
                   AND outcome.event_kind = result.event_kind
                   AND outcome.outcome_kind = result.outcome_kind
                   AND outcome.reason_kind = $4
                   AND outcome.provenance_kind = 'child_turn'
                   AND outcome.provenance_session_id = delegated.child_session_id
                   AND outcome.provenance_turn_id = delegated.turn_id
                  JOIN delegation_update_outbox_event AS parent_update
                    ON parent_update.result_spawning_request_id =
                       delegated.spawning_tool_request_id
                   AND parent_update.session_id = delegated.parent_session_id
                   AND parent_update.update_kind = 'child_result'
                   AND parent_update.spawning_tool_request_id =
                       delegated.spawning_tool_request_id
                   AND parent_update.child_session_id = delegated.child_session_id
                   AND parent_update.outcome_kind = $3
                   AND parent_update.reason_kind = $4
                   AND parent_update.provenance_kind = 'child_turn'
                   AND parent_update.provenance_session_id =
                       delegated.child_session_id
                   AND parent_update.provenance_turn_id = delegated.turn_id
                   AND parent_update.content_text IS NOT DISTINCT FROM $5
                   AND parent_update.event_kind = 'delegation_update'
                   AND parent_update.storage_version = 1
                  JOIN delegation_outbox_event AS update_header
                    ON update_header.event_sequence = parent_update.event_sequence
                   AND update_header.event_kind = parent_update.event_kind
                   AND update_header.storage_version = parent_update.storage_version
                   AND update_header.session_id = parent_update.session_id
                   AND update_header.event_kind = 'delegation_update'
                   AND update_header.storage_version = 1
                  JOIN delegation_wake_outbox_event AS parent_wake
                    ON parent_wake.result_spawning_request_id =
                       delegated.spawning_tool_request_id
                   AND parent_wake.session_id = delegated.parent_session_id
                   AND parent_wake.spawning_tool_request_id =
                       delegated.spawning_tool_request_id
                   AND parent_wake.subject_kind = 'result'
                   AND parent_wake.awaiting_tool_request_id IS NULL
                   AND parent_wake.message_id IS NULL
                   AND parent_wake.event_kind = 'delegation_wake'
                   AND parent_wake.storage_version = 1
                  JOIN delegation_outbox_event AS wake_header
                    ON wake_header.event_sequence = parent_wake.event_sequence
                   AND wake_header.event_kind = parent_wake.event_kind
                   AND wake_header.storage_version = parent_wake.storage_version
                   AND wake_header.session_id = parent_wake.session_id
                   AND wake_header.event_kind = 'delegation_wake'
                   AND wake_header.storage_version = 1
                 WHERE NOT EXISTS (
                    SELECT 1
                      FROM session_delegation_wait AS wait
                      LEFT JOIN session_child_result_delivery AS delivery
                        ON delivery.awaiting_tool_request_id =
                           wait.awaiting_tool_request_id
                       AND delivery.spawning_tool_request_id =
                           wait.spawning_tool_request_id
                       AND delivery.parent_session_id = wait.parent_session_id
                      LEFT JOIN session_pending_delivery AS pending
                        ON pending.recipient_session_id =
                           delivery.parent_session_id
                       AND pending.delivery_sequence = delivery.delivery_sequence
                       AND pending.delivery_kind = delivery.delivery_kind
                     WHERE wait.spawning_tool_request_id =
                           delegated.spawning_tool_request_id
                       AND (
                            delivery.awaiting_tool_request_id IS NULL
                            OR wait.wait_mode IS NULL
                            OR wait.wait_mode NOT IN ('foreground', 'background')
                            OR (
                                wait.wait_mode = 'foreground'
                                AND (
                                    delivery.delivery_sequence IS NOT NULL
                                    OR delivery.delivery_kind IS NOT NULL
                                )
                            )
                            OR (
                                wait.wait_mode = 'background'
                                AND (
                                    delivery.delivery_sequence IS NULL
                                    OR delivery.delivery_kind <>
                                       'background_result'
                                    OR pending.delivery_sequence IS NULL
                                )
                            )
                       )
                 )
                   AND NOT EXISTS (
                    SELECT 1
                      FROM session_child_result_delivery AS delivery
                      LEFT JOIN session_delegation_wait AS wait
                        ON wait.awaiting_tool_request_id =
                           delivery.awaiting_tool_request_id
                       AND wait.spawning_tool_request_id =
                           delivery.spawning_tool_request_id
                       AND wait.parent_session_id = delivery.parent_session_id
                     WHERE delivery.spawning_tool_request_id =
                           delegated.spawning_tool_request_id
                       AND wait.awaiting_tool_request_id IS NULL
                 )
                )
            )
          FROM origin",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(outcome_kind)
    .bind(reason_kind)
    .bind(stored.content)
    .fetch_one(connection)
    .await?)
}

async fn delegated_nonterminal_result_absent(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<bool, ModelCallRepositoryError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "WITH delegated AS (
            SELECT relation.spawning_tool_request_id
              FROM session_delegation AS relation
              JOIN session_delegation_initial_task AS task
                ON task.spawning_tool_request_id = relation.spawning_tool_request_id
               AND task.child_session_id = relation.child_session_id
               AND task.turn_id = $2
             WHERE relation.child_session_id = $1
        ),
        origin AS (
            SELECT EXISTS (
                       SELECT 1
                         FROM turn_lifecycle
                        WHERE session_id = $1
                          AND turn_id = $2
                          AND origin_kind = 'delegation'
                   ) AS delegated,
                   EXISTS (
                       SELECT 1
                         FROM session_delegation_initial_task
                        WHERE child_session_id = $1
                          AND turn_id = $2
                   ) AS initial_task,
                   EXISTS (
                       SELECT 1
                         FROM session_delegation_wake_turn_origin
                        WHERE recipient_session_id = $1
                          AND turn_id = $2
                   ) AS wake
        )
        SELECT NOT origin.delegated
            OR (origin.wake AND NOT origin.initial_task)
            OR (
                origin.initial_task
                AND NOT origin.wake
                AND (SELECT count(*) = 1 FROM delegated)
                AND NOT EXISTS (
                    SELECT 1
                      FROM delegated
                      JOIN session_child_result AS result
                        ON result.spawning_tool_request_id =
                           delegated.spawning_tool_request_id
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM delegated
                      JOIN delegation_update_outbox_event AS parent_update
                        ON parent_update.result_spawning_request_id =
                           delegated.spawning_tool_request_id
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM delegated
                      JOIN delegation_wake_outbox_event AS parent_wake
                        ON parent_wake.result_spawning_request_id =
                           delegated.spawning_tool_request_id
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM delegated
                      JOIN session_child_result_delivery AS delivery
                        ON delivery.spawning_tool_request_id =
                           delegated.spawning_tool_request_id
                )
            )
          FROM origin",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_one(connection)
    .await?)
}

/// Reconstitutes one continuation against caller-owned, transaction-local tool
/// results before the shared model-call/outbox ordering guard is acquired.
pub(crate) async fn load_tool_continuation_execution(
    connection: &mut PgConnection,
    session: SessionId,
    targets: &ModelTargetCatalog,
    projection: &PreparedToolResultProjection,
) -> Result<ModelCallExecution, ModelCallRepositoryError> {
    let continuation_snapshot = projection.snapshot();
    let continuation = ResolvedContextFrontierReconstitutionInput::new(
        session,
        continuation_snapshot.frontier().snapshot(),
        continuation_snapshot.ordered_entries().collect(),
    );
    require_live_execution_with_targets(
        connection,
        session,
        Some(targets),
        Some(continuation),
        Some(projection.clone()),
    )
    .await
}

/// Prepares one continuation call inside a caller-owned tool-result
/// transaction. The caller must hold the session scheduler lock and the shared
/// model-call/outbox ordering guard before projecting any result outbox event,
/// and commits or rolls back this function's writes together with that result.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_tool_continuation_call<NextSteeringIdentities>(
    connection: &mut PgConnection,
    _outbox_order_guard: ModelCallOutboxOrderGuard,
    execution: ModelCallExecution,
    session: SessionId,
    turn: TurnId,
    targets: &ModelTargetCatalog,
    credential_reference: &ModelCallCredentialReference,
    credential_families: Option<&crate::ModelCredentialFamilyCatalog>,
    credential_pools: &CredentialPoolRuntimeCatalog,
    cache_inclusive_input_targets: &HashSet<ResolvedProviderTarget>,
    continuation_usage_limits: &ToolContinuationUsageLimitCatalog,
    projection: &PreparedToolResultProjection,
    producing_call: ModelCallId,
    call: ModelCallId,
    failure_identities: FailedModelCallTurnIdentities,
    steering_frontier: signalbox_domain::ContextFrontierId,
    mut next_steering_identities: NextSteeringIdentities,
) -> Result<PrepareToolContinuationOutcome, ModelCallRepositoryError>
where
    NextSteeringIdentities:
        FnMut(AcceptedInputId) -> (signalbox_domain::SemanticTranscriptEntryId, TurnId),
{
    let continuation_snapshot = projection.snapshot();
    if execution.turn() != turn || execution.current_call().is_some() {
        return Ok(PrepareToolContinuationOutcome::NoWork);
    }
    let mut reserved_entries = execution
        .frontier_entries()
        .map(signalbox_domain::SemanticTranscriptEntry::identity)
        .collect::<BTreeSet<_>>();
    let mut steering_identities =
        Vec::with_capacity(execution.active_turn().pending_steering().len());
    for pending in execution.active_turn().pending_steering() {
        let accepted_input = pending.accepted_input();
        let (entry, successor_turn) = next_steering_identities(accepted_input);
        if !reserved_entries.insert(entry) {
            return Err(ModelCallRepositoryError::IdentityCollision(
                ModelCallIdentityCollision::SemanticEntry,
            ));
        }
        steering_identities.push((
            entry,
            PendingSteeringReclassificationIdentity::new(accepted_input, successor_turn),
        ));
    }
    let steering_entries = steering_identities
        .iter()
        .map(|(entry, _)| *entry)
        .collect::<Vec<_>>();
    if !steering_entries.is_empty()
        && steering_frontier == continuation_snapshot.frontier().snapshot()
    {
        return Err(ModelCallRepositoryError::IdentityCollision(
            ModelCallIdentityCollision::TerminalFrontier,
        ));
    }
    let steering_snapshot = (!steering_entries.is_empty()).then_some(steering_frontier);
    let fast_mode = execution
        .configuration()
        .effective()
        .model_settings()
        .effective()
        .fast_mode();
    let resolved_target = targets.resolve(*execution.configuration().effective().model());
    if let Ok(resolved) = resolved_target
        && let Some(limit) = continuation_usage_limits.get(&(resolved.target(), fast_mode))
        && let Some(evidence) = load_tool_continuation_headroom_evidence(
            connection,
            session,
            turn,
            producing_call,
            *limit,
        )
        .await?
    {
        let source_turn = execution.turn();
        let reclassifications = steering_identities
            .iter()
            .map(|(_, reclassification)| *reclassification)
            .collect::<Vec<_>>();
        let mut proposed_turns = BTreeSet::new();
        for reclassification in &reclassifications {
            record_reclassified_turn_candidate(
                source_turn,
                reclassification.turn(),
                &mut proposed_turns,
            )?;
        }
        let required = execution
            .require_context_compaction_after_tool_results(
                producing_call,
                failure_identities
                    .clone()
                    .with_pending_steering_reclassifications(reclassifications),
            )
            .map_err(|_| {
                ModelCallRepositoryError::InvalidTransition(
                    "context headroom exhaustion could not close tool continuation",
                )
            })?;
        persist_failed_with_delegated_child_result(
            connection,
            required.failed(),
            TurnTerminalCause::ContextHeadroomExhausted,
            ProviderReportedTokenUsage::unreported(),
            None,
            None,
        )
        .await?;
        persist_tool_continuation_headroom_exhaustion(connection, &required, evidence).await?;
        return Ok(PrepareToolContinuationOutcome::ContextCompactionRequired(
            Box::new(required),
        ));
    }
    let selected = if let Ok(resolved) = resolved_target {
        let default_reference = resolve_session_credential(
            connection,
            session,
            resolved.target(),
            fast_mode,
            credential_reference,
            credential_families,
        )
        .await?;
        let selected = Some(
            select_runtime_pool_credential(
                connection,
                session,
                turn,
                execution.current_attempt().id(),
                serving_pool_target(credential_families, resolved.target(), fast_mode),
                default_reference,
                credential_pools,
            )
            .await?,
        );
        outbox::lock_sequence_allocator(connection).await?;
        selected
    } else {
        None
    };
    if let Some(SelectedRuntimePoolCredential {
        reference: None,
        policy: Some(policy),
        ..
    }) = selected.as_ref()
    {
        let source_turn = execution.turn();
        let reclassifications = steering_identities
            .iter()
            .map(|(_, reclassification)| *reclassification)
            .collect::<Vec<_>>();
        let mut proposed_turns = BTreeSet::new();
        for reclassification in &reclassifications {
            record_reclassified_turn_candidate(
                source_turn,
                reclassification.turn(),
                &mut proposed_turns,
            )?;
        }
        let exhausted = execution
            .fail_credential_pool_exhausted(
                policy.name().to_owned(),
                failure_identities
                    .clone()
                    .with_pending_steering_reclassifications(reclassifications),
            )
            .map_err(|_| {
                ModelCallRepositoryError::InvalidTransition(
                    "credential-pool exhaustion could not close tool continuation",
                )
            })?;
        persist_credential_pool_exhaustion(connection, &exhausted).await?;
        return Ok(PrepareToolContinuationOutcome::PoolExhausted(Box::new(
            exhausted,
        )));
    }
    let prepared = match execution.prepare_initial_call_consuming_steering(
        call,
        steering_entries,
        steering_snapshot,
    ) {
        Ok(prepared) => prepared,
        Err(error) if error.failure() == ModelCallPreparationFailure::TargetUnavailable => {
            let resolution = error.target_resolution_error().ok_or(
                ModelCallRepositoryError::InvalidTransition(
                    "continuation target failure omitted its resolution proof",
                ),
            )?;
            let source_turn = error.execution().turn();
            let reclassifications = steering_identities
                .into_iter()
                .map(|(_, reclassification)| reclassification)
                .collect::<Vec<_>>();
            let mut proposed_turns = BTreeSet::new();
            for reclassification in &reclassifications {
                record_reclassified_turn_candidate(
                    source_turn,
                    reclassification.turn(),
                    &mut proposed_turns,
                )?;
            }
            let failed = error
                .execution()
                .clone()
                .fail_target_resolution(
                    resolution,
                    failure_identities.with_pending_steering_reclassifications(reclassifications),
                )
                .map_err(|_| {
                    ModelCallRepositoryError::InvalidTransition(
                        "continuation target failure could not close execution",
                    )
                })?;
            persist_failed_with_delegated_child_result(
                connection,
                &failed,
                TurnTerminalCause::ModelTargetUnavailable,
                ProviderReportedTokenUsage::unreported(),
                None,
                None,
            )
            .await?;
            return Ok(PrepareToolContinuationOutcome::TargetUnavailable(Box::new(
                failed,
            )));
        }
        Err(_) => {
            return Err(ModelCallRepositoryError::InvalidTransition(
                "continuation call cannot be prepared",
            ));
        }
    };
    let selected = selected.ok_or(ModelCallRepositoryError::InvalidTransition(
        "resolved continuation omitted credential selection",
    ))?;
    let credential_reference =
        selected
            .reference
            .ok_or(ModelCallRepositoryError::InvalidTransition(
                "available continuation selection omitted a credential reference",
            ))?;
    insert_prepared_call(
        connection,
        &prepared,
        &credential_reference,
        selected.policy.as_ref(),
        cache_inclusive_input_targets.contains(&prepared.call().target()),
    )
    .await?;
    consume_pool_member_actions(
        connection,
        prepared.turn(),
        &selected.pending_consumed_actions,
    )
    .await?;
    Ok(PrepareToolContinuationOutcome::Checkpointed(call))
}

#[derive(Clone, Copy)]
struct ToolContinuationHeadroomEvidence {
    usage: ProviderReportedTokenUsage,
    input_includes_cache_tokens: bool,
    projected_result_content_bytes: u64,
    limit: ToolContinuationUsageLimit,
}

async fn load_tool_continuation_headroom_evidence(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    producing_call: ModelCallId,
    limit: ToolContinuationUsageLimit,
) -> Result<Option<ToolContinuationHeadroomEvidence>, ModelCallRepositoryError> {
    let row = sqlx::query(
        "SELECT usage_input_includes_cache_tokens,
                usage_input_tokens, usage_output_tokens,
                usage_cache_creation_input_tokens,
                usage_cache_read_input_tokens,
                NOT EXISTS (
                    SELECT 1
                      FROM semantic_transcript_entry AS compacted
                     WHERE compacted.source_session_id = model_call.session_id
                       AND compacted.producing_model_call_id = model_call.model_call_id
                       AND compacted.payload_kind = 'provider_compaction'
                       AND compacted.assistant_text_value::jsonb ->> 'content' IS NOT NULL
                ) AS input_is_retained,
                (
                    SELECT COALESCE(SUM(projected.content_bytes), 0)::numeric
                      FROM (
                            SELECT COALESCE(octet_length(attempt.result_text), 0)
                                   + COALESCE(octet_length(attempt.error_detail), 0)
                                       AS content_bytes
                              FROM semantic_transcript_entry AS entry
                              JOIN tool_attempt AS attempt
                                ON attempt.attempt_id = entry.tool_result_attempt_id
                               AND attempt.session_id = entry.source_session_id
                              JOIN tool_request AS request
                                ON request.request_id = attempt.request_id
                               AND request.session_id = attempt.session_id
                               AND request.turn_id = attempt.turn_id
                             WHERE request.producing_model_call_id = model_call.model_call_id
                               AND request.session_id = model_call.session_id
                               AND request.turn_id = model_call.turn_id

                            UNION ALL

                            SELECT COALESCE(octet_length(decision.denial_reason), 0)
                                       AS content_bytes
                              FROM semantic_transcript_entry AS entry
                              JOIN tool_request AS request
                                ON request.request_id = entry.tool_result_request_id
                               AND request.session_id = entry.source_session_id
                              JOIN tool_approval_decision AS decision
                                ON decision.request_id = request.request_id
                             WHERE entry.payload_kind = 'tool_denied'
                               AND request.producing_model_call_id = model_call.model_call_id
                               AND request.session_id = model_call.session_id
                               AND request.turn_id = model_call.turn_id

                            UNION ALL

                            -- A returning foreground await renders the child's
                            -- delivered result as this round's tool result, so
                            -- its content joins the round through the awaiting
                            -- request this call issued.
                            SELECT COALESCE(octet_length(child_result.content_text), 0)
                                       AS content_bytes
                              FROM semantic_transcript_entry AS entry
                              JOIN tool_request AS request
                                ON request.request_id =
                                   entry.delegation_result_awaiting_tool_request_id
                               AND request.session_id = entry.source_session_id
                              JOIN session_child_result AS child_result
                                ON child_result.spawning_tool_request_id =
                                   entry.delegation_result_spawning_tool_request_id
                             WHERE entry.payload_kind = 'delegation_result'
                               AND request.producing_model_call_id = model_call.model_call_id
                               AND request.session_id = model_call.session_id
                               AND request.turn_id = model_call.turn_id
                      ) AS projected
                ) AS projected_result_content_bytes
           FROM model_call
          WHERE model_call_id = $1
            AND session_id = $2
            AND turn_id = $3
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'completed'",
    )
    .bind(producing_call.into_uuid())
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Err(ModelCallCorruption::Missing("completed tool-producing call").into());
    };
    let decode = |field: &'static str| -> Result<Option<u64>, ModelCallRepositoryError> {
        row.try_get::<Option<Decimal>, _>(field)?
            .map(|value| {
                if !value.fract().is_zero() || value.is_sign_negative() {
                    return Err(ModelCallCorruption::Inconsistent(
                        "tool-producing model-call token usage",
                    )
                    .into());
                }
                u64::try_from(value).map_err(|_| {
                    ModelCallCorruption::Inconsistent("tool-producing model-call token usage")
                        .into()
                })
            })
            .transpose()
    };
    let usage = ProviderReportedTokenUsage::unreported()
        .with_input_tokens(decode("usage_input_tokens")?)
        .with_output_tokens(decode("usage_output_tokens")?)
        .with_cache_creation_input_tokens(decode("usage_cache_creation_input_tokens")?)
        .with_cache_read_input_tokens(decode("usage_cache_read_input_tokens")?);
    let input_includes_cache_tokens = row.try_get("usage_input_includes_cache_tokens")?;
    let projected_result_content_bytes = decode("projected_result_content_bytes")?.ok_or(
        ModelCallCorruption::Missing("projected tool-result content byte count"),
    )?;
    let Some(input_tokens) = usage.input_tokens() else {
        return Ok(None);
    };
    let input_is_retained: bool = row.try_get("input_is_retained")?;
    let input_tokens = if !input_is_retained {
        0
    } else if input_includes_cache_tokens {
        input_tokens
    } else {
        input_tokens
            .saturating_add(usage.cache_creation_input_tokens().unwrap_or(0))
            .saturating_add(usage.cache_read_input_tokens().unwrap_or(0))
    };
    let exhausted = input_tokens
        .saturating_add(usage.output_tokens().unwrap_or(0))
        // Provider-neutral CLI adapters expose no tokenizer-only operation.
        // UTF-8 payload bytes therefore reserve a deliberately conservative
        // allowance for result material appended after the reported input.
        .saturating_add(projected_result_content_bytes)
        .saturating_add(limit.max_output_tokens())
        > limit.context_window_tokens();
    Ok(exhausted.then_some(ToolContinuationHeadroomEvidence {
        usage,
        input_includes_cache_tokens,
        projected_result_content_bytes,
        limit,
    }))
}

pub(crate) async fn resolve_session_credential(
    connection: &mut PgConnection,
    session: SessionId,
    target: ResolvedProviderTarget,
    fast_mode: FastMode,
    fallback: &ModelCallCredentialReference,
    families: Option<&crate::ModelCredentialFamilyCatalog>,
) -> Result<ModelCallCredentialReference, ModelCallRepositoryError> {
    let Some(families) = families else {
        return Ok(fallback.clone());
    };
    let family = families
        .family_for_call(target, fast_mode)
        .ok_or(ModelCallCorruption::Missing("model credential family"))?;
    match crate::session_credentials::load_current_session_credential(
        connection,
        session_id_to_uuid(session),
        family,
    )
    .await
    {
        Ok(reference) => Ok(reference),
        Err(sqlx::Error::RowNotFound) => match families
            .migration_fallback_family_for_call(target, fast_mode)
        {
            Some(fallback_family) => crate::session_credentials::load_migrated_session_credential(
                connection,
                session_id_to_uuid(session),
                fallback_family,
            )
            .await
            .map_err(|error| match error {
                sqlx::Error::RowNotFound => {
                    ModelCallCorruption::Missing("current session model credential").into()
                }
                error => error.into(),
            }),
            None => Err(ModelCallCorruption::Missing("current session model credential").into()),
        },
        Err(error) => Err(error.into()),
    }
}

/// Closes a turn after a prepared or effect-free tool attempt was lost across
/// process restart. The caller owns the delegated endpoint and scheduler locks
/// and commits this closure with the attempt's `CrashLost` evidence.
pub(crate) async fn fail_tool_crash_in_transaction<NextTurn>(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    projection: &PreparedToolResultProjection,
    identities: FailedModelCallTurnIdentities,
    mut next_turn: NextTurn,
) -> Result<FailedModelCallTurn, ModelCallRepositoryError>
where
    NextTurn: FnMut(AcceptedInputId) -> TurnId,
{
    let current_snapshot = projection.snapshot();
    let continuation = ResolvedContextFrontierReconstitutionInput::new(
        session,
        current_snapshot.frontier().snapshot(),
        current_snapshot.ordered_entries().collect(),
    );
    let execution = require_live_execution_with_targets(
        connection,
        session,
        None,
        Some(continuation),
        Some(projection.clone()),
    )
    .await?;
    if execution.turn() != turn || execution.current_call().is_some() {
        return Err(ModelCallRepositoryError::InvalidTransition(
            "tool crash closure does not match live execution",
        ));
    }
    let reclassifications = pending_reclassification_candidates(&execution, &mut next_turn)?;
    let failed = execution
        .recover_tool_crash_after_restart(
            identities.with_pending_steering_reclassifications(reclassifications),
        )
        .map_err(|_| {
            ModelCallRepositoryError::InvalidTransition(
                "tool crash could not close evidence-free execution",
            )
        })?;
    persist_failed_with_delegated_child_result(
        connection,
        &failed,
        TurnTerminalCause::ToolAttemptLost,
        ProviderReportedTokenUsage::unreported(),
        None,
        None,
    )
    .await?;
    Ok(failed)
}

impl PrepareModelCallTransaction for PostgresModelCallRepository {
    type Error = ModelCallRepositoryError;

    async fn prepare<NextSteeringIdentities>(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        failure_identities: FailedModelCallTurnIdentities,
        steering_frontier: signalbox_domain::ContextFrontierId,
        next_steering_identities: NextSteeringIdentities,
    ) -> Result<PrepareModelCallOutcome, Self::Error>
    where
        NextSteeringIdentities:
            FnMut(AcceptedInputId) -> (signalbox_domain::SemanticTranscriptEntryId, TurnId) + Send,
    {
        match self
            .prepare_initial_call(
                session,
                call,
                failure_identities,
                steering_frontier,
                next_steering_identities,
            )
            .await
        {
            Err(ModelCallRepositoryError::NoLiveExecution) => Ok(PrepareModelCallOutcome::NoWork),
            result => result,
        }
    }
}

impl FailPreparedModelCallTransaction for PostgresModelCallRepository {
    type Error = ModelCallRepositoryError;

    async fn fail_prepared<NextTurn>(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        cause: PreparedModelCallFailureCause,
        attachment_failure: Option<AttachmentPreparationFailure>,
        identities: FailedModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
    ) -> Result<FailedModelCallTurn, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        PostgresModelCallRepository::fail_prepared_call(
            self,
            session,
            call,
            cause,
            attachment_failure,
            identities,
            next_reclassified_turn,
        )
        .await
    }

    async fn reread_failure(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        attachment_failure: Option<AttachmentPreparationFailure>,
    ) -> Result<RetainedPreparedFailureStatus, Self::Error> {
        self.reread_prepared_failure(session, call, attachment_failure)
            .await
    }
}

/// Locks the terminal-observation frontier, then reports whether a cascade
/// already delivered this delegated turn's logical terminal.
///
/// An ordinary call retains the model-execution scheduler lock. A delegated
/// call instead shares peer-message ordering: canonical endpoint sessions,
/// canonical endpoint schedulers, then the relationship. Besides making the
/// logical-terminal read authoritative, this prevents a message transaction
/// holding the parent session from waiting on a child scheduler held by an
/// observation that is itself waiting for that parent session.
async fn locked_delegation_logical_terminal(
    connection: &mut PgConnection,
    session: SessionId,
    call: ModelCallId,
) -> Result<bool, ModelCallRepositoryError> {
    if lock_model_call_terminal_frontier(connection, session, call)
        .await?
        .is_none()
    {
        return Ok(false);
    }
    model_call_is_delegation_logically_terminal(connection, session, call).await
}

async fn lock_model_call_terminal_frontier(
    connection: &mut PgConnection,
    session: SessionId,
    call: ModelCallId,
) -> Result<Option<TurnId>, ModelCallRepositoryError> {
    let turn: Option<Uuid> = sqlx::query_scalar(
        "SELECT turn_id
           FROM model_call
          WHERE session_id = $1
            AND model_call_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(call.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(turn) = turn else {
        lock_session(connection, session).await?;
        return Ok(None);
    };
    let turn = turn_id_from_uuid(turn);
    lock_delegated_turn_terminal_frontier(connection, session, turn).await?;
    Ok(Some(turn))
}

pub(crate) async fn lock_delegated_turn_terminal_frontier(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<(), ModelCallRepositoryError> {
    let relation = load_delegation_terminal_relation(
        connection,
        crate::lock_inventory::DELEGATION_TERMINAL_RELATION_IDENTITY,
        session_id_to_uuid(session),
        turn_id_to_uuid(turn),
    )
    .await?;
    let Some(relation) = relation else {
        lock_session(connection, session).await?;
        return Ok(());
    };
    let parent = session_id_from_uuid(relation.parent_session_id);
    let (first, second) = crate::lock_inventory::ordered_session_pair(session, parent);
    lock_delegation_terminal_session(connection, first).await?;
    if second != first {
        lock_delegation_terminal_session(connection, second).await?;
    }
    lock_session(connection, first).await?;
    if second != first {
        lock_session(connection, second).await?;
    }
    let locked = load_delegation_terminal_relation(
        connection,
        crate::lock_inventory::DELEGATION_TERMINAL_RELATION,
        session_id_to_uuid(session),
        turn_id_to_uuid(turn),
    )
    .await?;
    if locked != Some(relation) {
        return Err(ModelCallCorruption::Inconsistent(
            "delegated terminal relationship changed while locking",
        )
        .into());
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct DelegationTerminalRelationRow {
    spawning_tool_request_id: Uuid,
    parent_session_id: Uuid,
}

async fn load_delegation_terminal_relation(
    connection: &mut PgConnection,
    statement: &'static str,
    child: Uuid,
    turn: Uuid,
) -> Result<Option<DelegationTerminalRelationRow>, ModelCallRepositoryError> {
    sqlx::query(statement)
        .bind(child)
        .bind(turn)
        .fetch_optional(connection)
        .await?
        .map(|row| {
            Ok(DelegationTerminalRelationRow {
                spawning_tool_request_id: delegation_terminal_relation_uuid(
                    &row,
                    "spawning_tool_request_id",
                )?,
                parent_session_id: delegation_terminal_relation_uuid(&row, "parent_session_id")?,
            })
        })
        .transpose()
}

fn delegation_terminal_relation_uuid(
    row: &PgRow,
    column: &'static str,
) -> Result<Uuid, ModelCallRepositoryError> {
    match row.try_get(column) {
        Ok(value) => Ok(value),
        Err(error @ (sqlx::Error::ColumnDecode { .. } | sqlx::Error::Decode(_))) => {
            Err(delegation_terminal_relation_decode_error(error))
        }
        Err(error) => Err(error.into()),
    }
}

fn delegation_terminal_relation_decode_error(error: sqlx::Error) -> ModelCallRepositoryError {
    debug_assert!(matches!(
        error,
        sqlx::Error::ColumnDecode { .. } | sqlx::Error::Decode(_)
    ));
    ModelCallCorruption::Inconsistent("delegated terminal relationship identity").into()
}

async fn lock_delegation_terminal_session(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<(), ModelCallRepositoryError> {
    let locked =
        sqlx::query_scalar::<_, Uuid>(crate::lock_inventory::DELEGATION_TERMINAL_ENDPOINT_SESSION)
            .bind(session_id_to_uuid(session))
            .fetch_optional(connection)
            .await?;
    if locked.is_some_and(|locked| session_id_from_uuid(locked) == session) {
        Ok(())
    } else {
        Err(ModelCallCorruption::Missing("delegated terminal endpoint session").into())
    }
}

async fn model_call_is_delegation_logically_terminal(
    connection: &mut PgConnection,
    session: SessionId,
    call: ModelCallId,
) -> Result<bool, ModelCallRepositoryError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM model_call AS call
              JOIN session_delegation_initial_task AS task
                ON task.child_session_id = call.session_id
               AND task.turn_id = call.turn_id
              JOIN session_delegation_logical_terminal AS terminal
                ON terminal.spawning_tool_request_id = task.spawning_tool_request_id
               AND terminal.child_session_id = task.child_session_id
               AND terminal.child_turn_id = task.turn_id
             WHERE call.session_id = $1
               AND call.model_call_id = $2
        )",
    )
    .bind(session_id_to_uuid(session))
    .bind(call.into_uuid())
    .fetch_one(&mut *connection)
    .await?)
}

impl AuthorizeModelCallTransaction for PostgresModelCallRepository {
    type Error = ModelCallRepositoryError;

    async fn authorize(
        &mut self,
        session: SessionId,
        call: ModelCallId,
    ) -> Result<AuthorizeModelCallOutcome, Self::Error> {
        self.authorize_send(session, call).await
    }

    async fn reread_after_ambiguous_commit(
        &mut self,
        session: SessionId,
        prepared: &signalbox_domain::PreparedModelCallRequest,
    ) -> Result<ModelCallAuthorizationReread, Self::Error> {
        self.reread_ambiguous_authorization(session, prepared).await
    }

    fn cancellation_signal(
        &self,
        session: SessionId,
        call: ModelCallId,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let pool = self.pool.clone();
        async move {
            let mut interval = cancellation_poll_interval();
            loop {
                interval.tick().await;
                let cancelled = sqlx::query_scalar::<_, bool>(
                    "SELECT call.state_kind IN ('cancellation_requested', 'terminal')
                            OR EXISTS (
                                SELECT 1
                                  FROM session_delegation_initial_task AS task
                                  JOIN session_delegation_logical_terminal AS terminal
                                    ON terminal.spawning_tool_request_id =
                                       task.spawning_tool_request_id
                                   AND terminal.child_session_id = task.child_session_id
                                   AND terminal.child_turn_id = task.turn_id
                                 WHERE task.child_session_id = call.session_id
                                   AND task.turn_id = call.turn_id
                            )
                       FROM model_call AS call
                      WHERE call.session_id = $1
                        AND call.model_call_id = $2",
                )
                .bind(session_id_to_uuid(session))
                .bind(call.into_uuid())
                .fetch_optional(&pool)
                .await;
                if matches!(cancelled, Ok(Some(true))) {
                    return;
                }
            }
        }
    }
}

fn cancellation_poll_interval() -> tokio::time::Interval {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(25));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

impl CommitModelCallObservationTransaction for PostgresModelCallRepository {
    type Error = ModelCallRepositoryError;

    async fn commit_observation<NextTurn>(
        &mut self,
        session: SessionId,
        observation: CorrelatedModelCallTerminalObservation,
        identities: ModelCallTerminalIdentityCandidates,
        next_reclassified_turn: NextTurn,
    ) -> Result<Option<ModelCallObservationCommitOutcome>, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        self.apply_terminal_observation_candidates(
            session,
            observation,
            identities,
            next_reclassified_turn,
        )
        .await
    }

    async fn reread_observation(
        &mut self,
        session: SessionId,
        observation: &CorrelatedModelCallTerminalObservation,
    ) -> Result<RetainedModelCallObservationStatus, Self::Error> {
        self.reread_terminal_observation(session, observation).await
    }
}

async fn terminal_observation_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    if !terminal_observation_transition_events_match(connection, session, observation).await? {
        return Ok(false);
    }
    match observation.observation() {
        ModelCallTerminalObservation::Completed { assistant_text } => {
            let response = assistant_text
                .iter()
                .cloned()
                .map(AssistantResponsePart::Text)
                .collect::<Vec<_>>();
            completed_terminal_closure_matches(connection, session, observation, &response).await
        }
        ModelCallTerminalObservation::CompletedWithProviderCompaction { response } => {
            completed_terminal_closure_matches(connection, session, observation, response).await
        }
        ModelCallTerminalObservation::CompletedWithTools { response } => {
            tool_round_terminal_closure_matches(connection, session, observation, response).await
        }
        ModelCallTerminalObservation::KnownFailed => {
            failed_terminal_closure_matches(connection, session, observation).await
        }
        ModelCallTerminalObservation::Cancelled => {
            if cancelled_terminal_closure_matches(connection, session, observation).await? {
                Ok(true)
            } else {
                failed_terminal_closure_matches(connection, session, observation).await
            }
        }
        ModelCallTerminalObservation::Refused => {
            refused_terminal_closure_matches(connection, session, observation).await
        }
        ModelCallTerminalObservation::Ambiguous => {
            ambiguous_terminal_closure_matches(connection, session, observation).await
        }
    }
}

async fn tool_round_terminal_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
    response: &signalbox_domain::ToolUsingAssistantResponse,
) -> Result<bool, ModelCallRepositoryError> {
    let row = sqlx::query(
        "SELECT boundary_frontier_id, response_part_count
           FROM tool_round
          WHERE producing_model_call_id = $1
            AND session_id = $2
            AND turn_id = $3",
    )
    .bind(observation.call().into_uuid())
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(observation.correlation().turn()))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let boundary_frontier: Uuid = required(&row, "boundary_frontier_id")?;
    let response_part_count: Decimal = required(&row, "response_part_count")?;
    if response_part_count != Decimal::from(response.parts().len()) {
        return Ok(false);
    }

    let source_members = load_frontier_members(
        connection,
        session,
        observation.correlation().frontier().into_uuid(),
    )
    .await?;
    let boundary_members = load_frontier_members(connection, session, boundary_frontier).await?;
    if boundary_members.len() < source_members.len() + response.parts().len()
        || boundary_members
            .iter()
            .zip(&source_members)
            .any(|(stored, expected)| stored != expected)
    {
        return Ok(false);
    }

    let response_start = Decimal::from(source_members.len() + 1);
    let response_end = Decimal::from(source_members.len() + response.parts().len() + 1);
    let stored_parts = sqlx::query(
        "SELECT entry.payload_kind, entry.assistant_text_value,
                entry.producing_model_call_id,
                entry.assistant_tool_request_id,
                request.tool_name, request.arguments_kind,
                request.arguments_text
           FROM resolve_context_frontier_members($1, $2) AS member
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
           LEFT JOIN tool_request AS request
             ON request.request_id = entry.assistant_tool_request_id
          WHERE member.member_position >= $3
            AND member.member_position < $4
          ORDER BY member.member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(boundary_frontier)
    .bind(response_start)
    .bind(response_end)
    .fetch_all(&mut *connection)
    .await?;
    if stored_parts.len() != response.parts().len() {
        return Ok(false);
    }
    let call = observation.call().into_uuid();
    let matches = stored_parts
        .into_iter()
        .zip(response.parts())
        .all(|(stored, expected)| {
            let payload_kind = stored.try_get::<String, _>("payload_kind").ok();
            let assistant_text = stored
                .try_get::<Option<String>, _>("assistant_text_value")
                .ok()
                .flatten();
            let producing_call = stored
                .try_get::<Option<Uuid>, _>("producing_model_call_id")
                .ok()
                .flatten();
            let request = stored
                .try_get::<Option<Uuid>, _>("assistant_tool_request_id")
                .ok()
                .flatten();
            let tool_name = stored
                .try_get::<Option<String>, _>("tool_name")
                .ok()
                .flatten();
            let arguments_kind = stored
                .try_get::<Option<String>, _>("arguments_kind")
                .ok()
                .flatten();
            let arguments_text = stored
                .try_get::<Option<String>, _>("arguments_text")
                .ok()
                .flatten();
            match expected {
                AssistantResponsePart::Text(expected) => {
                    payload_kind.as_deref() == Some("assistant_text")
                        && assistant_text.as_deref() == Some(expected.as_str())
                        && producing_call == Some(call)
                        && request.is_none()
                        && tool_name.is_none()
                        && arguments_kind.is_none()
                        && arguments_text.is_none()
                }
                AssistantResponsePart::ProviderCompaction(expected) => {
                    payload_kind.as_deref() == Some("provider_compaction")
                        && assistant_text.as_deref() == Some(expected.as_json())
                        && producing_call == Some(call)
                        && request.is_none()
                        && tool_name.is_none()
                        && arguments_kind.is_none()
                        && arguments_text.is_none()
                }
                AssistantResponsePart::ToolCall(expected) => {
                    let expected_kind = match expected.arguments().kind() {
                        signalbox_domain::ToolArgumentsKind::Json => "json",
                        signalbox_domain::ToolArgumentsKind::Undecodable => "undecodable",
                    };
                    payload_kind.as_deref() == Some("assistant_tool_use")
                        && assistant_text.is_none()
                        && producing_call == Some(call)
                        && request.is_some()
                        && tool_name.as_deref() == Some(expected.name().as_str())
                        && arguments_kind.as_deref() == Some(expected_kind)
                        && arguments_text.as_deref() == Some(expected.arguments().as_str())
                }
            }
        });
    Ok(matches)
}

async fn terminal_observation_transition_events_match(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT
            EXISTS (
                SELECT 1
                  FROM model_call_transition_outbox_event
                 WHERE session_id = $1
                   AND model_call_id = $2
                   AND turn_id = $3
                   AND call_state_kind = 'in_flight'
                   AND terminal_disposition_kind IS NULL
            )
            AND EXISTS (
                SELECT 1
                  FROM model_call_transition_outbox_event
                 WHERE session_id = $1
                   AND model_call_id = $2
                   AND turn_id = $3
                   AND call_state_kind = 'terminal'
                   AND terminal_disposition_kind = $4
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(observation.call().into_uuid())
    .bind(turn_id_to_uuid(observation.correlation().turn()))
    .bind(encode_disposition(observation.observation().disposition()))
    .fetch_one(&mut *connection)
    .await?)
}

async fn completed_terminal_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
    response: &[AssistantResponsePart],
) -> Result<bool, ModelCallRepositoryError> {
    let terminal_frontier = sqlx::query_scalar::<_, Uuid>(
        "SELECT terminal_frontier_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'completed'
            AND terminal_model_call_id = $3",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(observation.correlation().turn()))
    .bind(observation.call().into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(terminal_frontier) = terminal_frontier else {
        return Ok(false);
    };
    let source_frontier = load_frontier_members(
        connection,
        session,
        observation.correlation().frontier().into_uuid(),
    )
    .await?;
    let terminal_members = load_terminal_frontier(connection, session, terminal_frontier).await?;
    if !completed_terminal_frontier_matches(
        &source_frontier,
        &terminal_members,
        session_id_to_uuid(session),
        turn_id_to_uuid(observation.correlation().turn()),
        observation.call().into_uuid(),
        response,
    ) {
        return Ok(false);
    }
    let Some(completion_entry) = terminal_members.last().map(|member| member.entry) else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM turn_terminal_outbox_event
             WHERE disposition_kind = 'completed'
             AND session_id = $1
               AND turn_id = $2
               AND model_call_id = $3
               AND completion_entry_id = $4
               AND terminal_frontier_id = $5
        )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(observation.correlation().turn()))
    .bind(observation.call().into_uuid())
    .bind(completion_entry)
    .bind(terminal_frontier)
    .fetch_one(&mut *connection)
    .await?)
}

async fn failed_terminal_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    let correlation = observation.correlation();
    failed_turn_closure_matches(
        connection,
        session,
        turn_id_to_uuid(correlation.turn()),
        correlation.attempt().into_uuid(),
        observation.call().into_uuid(),
        correlation.frontier().into_uuid(),
    )
    .await
}

async fn cancelled_terminal_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    let correlation = observation.correlation();
    let terminal_frontier = sqlx::query_scalar::<_, Uuid>(
        "SELECT terminal_frontier_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'cancelled'
            AND terminal_attempt_id = $3
            AND terminal_model_call_id = $4
            AND EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND turn_attempt_id = $3
                   AND state_kind = 'ended'
                   AND end_variant = 'after_cancellation'
                   AND end_disposition = 'cancelled'
                   AND interrupt_command_id IS NOT NULL
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(correlation.turn()))
    .bind(correlation.attempt().into_uuid())
    .bind(observation.call().into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(terminal_frontier) = terminal_frontier else {
        return Ok(false);
    };
    let source_frontier =
        load_frontier_members(connection, session, correlation.frontier().into_uuid()).await?;
    let terminal_members = load_terminal_frontier(connection, session, terminal_frontier).await?;
    if terminal_members.len() != source_frontier.len() + 1
        || terminal_members
            .iter()
            .zip(&source_frontier)
            .any(|(stored, expected)| (stored.source_session, stored.entry) != *expected)
    {
        return Ok(false);
    }
    let cancellation = &terminal_members[source_frontier.len()];
    if cancellation.source_session != session_id_to_uuid(session)
        || cancellation.payload_kind != "turn_cancelled"
        || cancellation.assistant_text.is_some()
        || cancellation.producing_call.is_some()
        || cancellation.completed_turn.is_some()
        || cancellation.failed_turn.is_some()
        || cancellation.cancelled_turn != Some(turn_id_to_uuid(correlation.turn()))
    {
        return Ok(false);
    }
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT
            EXISTS (
                SELECT 1
                  FROM model_call_transition_outbox_event
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND model_call_id = $3
                   AND call_state_kind = 'cancellation_requested'
                   AND terminal_disposition_kind IS NULL
            )
            AND EXISTS (
                SELECT 1
                  FROM turn_terminal_outbox_event
                 WHERE disposition_kind = 'cancelled'
                 AND session_id = $1
                   AND turn_id = $2
                   AND cancellation_entry_id = $4
                   AND terminal_frontier_id = $5
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(correlation.turn()))
    .bind(observation.call().into_uuid())
    .bind(cancellation.entry)
    .bind(terminal_frontier)
    .fetch_one(&mut *connection)
    .await?)
}

async fn prepared_cancellation_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    turn: Uuid,
    attempt: Uuid,
    call: Uuid,
    source_frontier: Uuid,
) -> Result<bool, ModelCallRepositoryError> {
    let terminal_frontier = sqlx::query_scalar::<_, Uuid>(
        "SELECT terminal_frontier_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'cancelled'
            AND terminal_attempt_id = $3
            AND terminal_model_call_id = $4
            AND EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND turn_attempt_id = $3
                   AND state_kind = 'ended'
                   AND end_variant = 'after_cancellation'
                   AND end_disposition = 'cancelled'
                   AND interrupt_command_id IS NOT NULL
            )
            AND EXISTS (
                SELECT 1
                  FROM model_call_transition_outbox_event
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND model_call_id = $4
                   AND call_state_kind = 'prepared'
                   AND terminal_disposition_kind IS NULL
            )
            AND NOT EXISTS (
                SELECT 1
                  FROM model_call_transition_outbox_event
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND model_call_id = $4
                   AND call_state_kind IN ('in_flight', 'cancellation_requested')
            )
            AND EXISTS (
                SELECT 1
                  FROM model_call_transition_outbox_event
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND model_call_id = $4
                   AND call_state_kind = 'terminal'
                   AND terminal_disposition_kind = 'cancelled'
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn)
    .bind(attempt)
    .bind(call)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(terminal_frontier) = terminal_frontier else {
        return Ok(false);
    };
    let source_frontier = load_frontier_members(connection, session, source_frontier).await?;
    let terminal_members = load_terminal_frontier(connection, session, terminal_frontier).await?;
    if terminal_members.len() != source_frontier.len() + 1
        || terminal_members
            .iter()
            .zip(&source_frontier)
            .any(|(stored, expected)| (stored.source_session, stored.entry) != *expected)
    {
        return Ok(false);
    }
    let cancellation = &terminal_members[source_frontier.len()];
    if cancellation.source_session != session_id_to_uuid(session)
        || cancellation.payload_kind != "turn_cancelled"
        || cancellation.assistant_text.is_some()
        || cancellation.producing_call.is_some()
        || cancellation.completed_turn.is_some()
        || cancellation.failed_turn.is_some()
        || cancellation.cancelled_turn != Some(turn)
    {
        return Ok(false);
    }
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM turn_terminal_outbox_event
             WHERE disposition_kind = 'cancelled'
             AND session_id = $1
               AND turn_id = $2
               AND cancellation_entry_id = $3
               AND terminal_frontier_id = $4
        )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn)
    .bind(cancellation.entry)
    .bind(terminal_frontier)
    .fetch_one(&mut *connection)
    .await?)
}

async fn failed_turn_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    turn: Uuid,
    attempt: Uuid,
    call: Uuid,
    source_frontier: Uuid,
) -> Result<bool, ModelCallRepositoryError> {
    let terminal_frontier = sqlx::query_scalar::<_, Uuid>(
        "SELECT terminal_frontier_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'failed'
            AND terminal_attempt_id = $3
            AND terminal_model_call_id = $4
            AND active_phase_kind IS NULL
            AND current_attempt_id IS NULL
            AND recovery_model_call_id IS NULL
            AND EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND turn_attempt_id = $3
                   AND state_kind = 'ended'
                   AND end_disposition = 'known_failure'
                   AND (
                        end_variant = 'without_stop'
                        OR (
                            end_variant = 'after_cancellation'
                            AND interrupt_command_id IS NOT NULL
                            AND interrupt_predecessor_turn_id = $2
                        )
                   )
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn)
    .bind(attempt)
    .bind(call)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(terminal_frontier) = terminal_frontier else {
        return Ok(false);
    };
    let source_frontier = load_frontier_members(connection, session, source_frontier).await?;
    let terminal_members = load_terminal_frontier(connection, session, terminal_frontier).await?;
    if !failed_terminal_frontier_matches(
        &source_frontier,
        &terminal_members,
        session_id_to_uuid(session),
        turn,
    ) {
        return Ok(false);
    }
    let Some(failure_entry) = terminal_members.last().map(|member| member.entry) else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM turn_terminal_outbox_event
             WHERE disposition_kind = 'failed'
             AND session_id = $1
               AND turn_id = $2
               AND failure_entry_id = $3
               AND terminal_frontier_id = $4
        )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn)
    .bind(failure_entry)
    .bind(terminal_frontier)
    .fetch_one(&mut *connection)
    .await?)
}

async fn refused_terminal_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    let correlation = observation.correlation();
    let terminal_frontier = sqlx::query_scalar::<_, Uuid>(
        "SELECT terminal_frontier_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'refused'
            AND terminal_attempt_id = $3
            AND terminal_model_call_id = $4
            AND active_phase_kind IS NULL
            AND current_attempt_id IS NULL
            AND recovery_model_call_id IS NULL
            AND EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND turn_attempt_id = $3
                   AND state_kind = 'ended'
                   AND end_disposition = 'turn_refused'
                   AND (
                        end_variant = 'without_stop'
                        OR (
                            end_variant = 'after_cancellation'
                            AND interrupt_command_id IS NOT NULL
                            AND interrupt_predecessor_turn_id = $2
                        )
                   )
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(correlation.turn()))
    .bind(correlation.attempt().into_uuid())
    .bind(observation.call().into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(terminal_frontier) = terminal_frontier else {
        return Ok(false);
    };
    let source_frontier =
        load_frontier_members(connection, session, correlation.frontier().into_uuid()).await?;
    let terminal_members = load_terminal_frontier(connection, session, terminal_frontier).await?;
    if terminal_members.len() != source_frontier.len()
        || terminal_members
            .iter()
            .zip(&source_frontier)
            .any(|(stored, expected)| (stored.source_session, stored.entry) != *expected)
    {
        return Ok(false);
    }
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM turn_terminal_outbox_event
             WHERE disposition_kind = 'refused'
             AND session_id = $1
               AND turn_id = $2
               AND model_call_id = $3
               AND terminal_frontier_id = $4
        )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(correlation.turn()))
    .bind(observation.call().into_uuid())
    .bind(terminal_frontier)
    .fetch_one(&mut *connection)
    .await?)
}

async fn ambiguous_terminal_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    let correlation = observation.correlation();
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT
            EXISTS (
                SELECT 1
                  FROM turn_lifecycle
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND state_kind = 'active'
                   AND terminal_disposition_kind IS NULL
                   AND terminal_frontier_id IS NULL
                   AND terminal_attempt_id IS NULL
                   AND terminal_model_call_id IS NULL
                   AND active_phase_kind = 'awaiting_model_call_recovery'
                   AND current_attempt_id = $3
                   AND recovery_model_call_id = $4
                   AND EXISTS (
                        SELECT 1
                          FROM turn_attempt
                         WHERE session_id = $1
                           AND turn_id = $2
                           AND turn_attempt_id = $3
                           AND state_kind = 'ended'
                           AND end_variant = 'without_stop'
                           AND end_disposition = 'ambiguous'
                   )
            )
            OR EXISTS (
                SELECT 1
                  FROM turn_lifecycle AS lifecycle
                 WHERE lifecycle.session_id = $1
                   AND lifecycle.turn_id = $2
                   AND lifecycle.state_kind = 'terminal'
                   AND lifecycle.terminal_disposition_kind =
                       'reconciliation_required'
                   AND lifecycle.terminal_attempt_id = $3
                   AND lifecycle.terminal_model_call_id = $4
                   AND lifecycle.active_phase_kind IS NULL
                   AND lifecycle.current_attempt_id IS NULL
                   AND lifecycle.recovery_model_call_id IS NULL
                   AND (
                        EXISTS (
                            SELECT 1
                              FROM turn_attempt
                             WHERE session_id = $1
                               AND turn_id = $2
                               AND turn_attempt_id = $3
                               AND state_kind = 'ended'
                               AND end_variant = 'after_cancellation'
                               AND end_disposition = 'ambiguous'
                               AND interrupt_command_id IS NOT NULL
                               AND interrupt_predecessor_turn_id = $2
                        )
                        OR (
                            EXISTS (
                                SELECT 1
                                  FROM turn_attempt
                                 WHERE session_id = $1
                                   AND turn_id = $2
                                   AND turn_attempt_id = $3
                                   AND state_kind = 'ended'
                                   AND end_variant = 'without_stop'
                                   AND end_disposition = 'ambiguous'
                                   AND interrupt_command_id IS NULL
                                   AND interrupt_predecessor_turn_id IS NULL
                            )
                            AND EXISTS (
                                SELECT 1
                                  FROM submit_input_command AS command
                                  JOIN accepted_input AS accepted
                                    ON accepted.accepting_command_id =
                                        command.command_id
                                   AND accepted.accepted_input_id =
                                        command.result_accepted_input_id
                                   AND accepted.session_id =
                                        command.result_session_id
                                   AND accepted.origin_turn_id =
                                        command.result_turn_id
                                  JOIN queued_input_origin AS successor
                                    ON successor.accepted_input_id =
                                        accepted.accepted_input_id
                                   AND successor.turn_id =
                                        accepted.origin_turn_id
                                   AND successor.session_id =
                                        accepted.session_id
                                   AND successor.priority_kind =
                                        'interrupt_immediately_after'
                                   AND successor.interrupt_predecessor_turn_id =
                                        $2
                                 WHERE command.session_id = $1
                                   AND command.delivery_kind = 'interrupt'
                                   AND command.expected_active_turn_id = $2
                                   AND command.result_kind = 'applied'
                                   AND command.rejection_kind IS NULL
                                   AND accepted.disposition_kind = 'origin_of'
                            )
                        )
                   )
                   AND EXISTS (
                        SELECT 1
                          FROM turn_terminal_outbox_event
                         WHERE disposition_kind = 'reconciliation_required'
                         AND session_id = $1
                           AND turn_id = $2
                           AND model_call_id = $4
                           AND terminal_frontier_id =
                               lifecycle.terminal_frontier_id
                   )
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(correlation.turn()))
    .bind(correlation.attempt().into_uuid())
    .bind(observation.call().into_uuid())
    .fetch_one(&mut *connection)
    .await?)
}

async fn load_frontier_members(
    connection: &mut PgConnection,
    session: SessionId,
    frontier: Uuid,
) -> Result<Vec<(Uuid, Uuid)>, ModelCallRepositoryError> {
    Ok(sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT source_session_id, semantic_entry_id
           FROM resolve_context_frontier_members($1, $2)
          ORDER BY member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier)
    .fetch_all(&mut *connection)
    .await?)
}

async fn load_terminal_frontier(
    connection: &mut PgConnection,
    session: SessionId,
    frontier: Uuid,
) -> Result<Vec<StoredTerminalFrontierMember>, ModelCallRepositoryError> {
    sqlx::query(
        "SELECT member.source_session_id, member.semantic_entry_id,
                entry.payload_kind, entry.assistant_text_value,
                entry.producing_model_call_id, entry.completed_turn_id,
                entry.failed_turn_id, entry.cancelled_turn_id
           FROM resolve_context_frontier_members($1, $2) AS member
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
          ORDER BY member.member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier)
    .fetch_all(&mut *connection)
    .await?
    .into_iter()
    .map(|row| {
        Ok(StoredTerminalFrontierMember {
            source_session: required(&row, "source_session_id")?,
            entry: required(&row, "semantic_entry_id")?,
            payload_kind: required(&row, "payload_kind")?,
            assistant_text: row.try_get("assistant_text_value")?,
            producing_call: row.try_get("producing_model_call_id")?,
            completed_turn: row.try_get("completed_turn_id")?,
            failed_turn: row.try_get("failed_turn_id")?,
            cancelled_turn: row.try_get("cancelled_turn_id")?,
        })
    })
    .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredTerminalFrontierMember {
    source_session: Uuid,
    entry: Uuid,
    payload_kind: String,
    assistant_text: Option<String>,
    producing_call: Option<Uuid>,
    completed_turn: Option<Uuid>,
    failed_turn: Option<Uuid>,
    cancelled_turn: Option<Uuid>,
}

fn completed_terminal_frontier_matches(
    source_frontier: &[(Uuid, Uuid)],
    terminal_frontier: &[StoredTerminalFrontierMember],
    session: Uuid,
    turn: Uuid,
    call: Uuid,
    response: &[AssistantResponsePart],
) -> bool {
    if terminal_frontier.len() != source_frontier.len() + response.len() + 1 {
        return false;
    }
    if terminal_frontier
        .iter()
        .zip(source_frontier)
        .any(|(stored, expected)| (stored.source_session, stored.entry) != *expected)
    {
        return false;
    }
    let assistant_start = source_frontier.len();
    if terminal_frontier[assistant_start..assistant_start + response.len()]
        .iter()
        .zip(response)
        .any(|(stored, expected)| {
            let content_matches = match expected {
                AssistantResponsePart::Text(text) => {
                    stored.payload_kind == "assistant_text"
                        && stored.assistant_text.as_deref() == Some(text.as_str())
                }
                AssistantResponsePart::ProviderCompaction(block) => {
                    stored.payload_kind == "provider_compaction"
                        && stored.assistant_text.as_deref() == Some(block.as_json())
                }
                AssistantResponsePart::ToolCall(_) => false,
            };
            stored.source_session != session
                || !content_matches
                || stored.producing_call != Some(call)
                || stored.completed_turn.is_some()
                || stored.failed_turn.is_some()
                || stored.cancelled_turn.is_some()
        })
    {
        return false;
    }
    let completion = &terminal_frontier[assistant_start + response.len()];
    completion.source_session == session
        && completion.payload_kind == "turn_completed"
        && completion.assistant_text.is_none()
        && completion.producing_call.is_none()
        && completion.completed_turn == Some(turn)
        && completion.failed_turn.is_none()
        && completion.cancelled_turn.is_none()
}

fn failed_terminal_frontier_matches(
    source_frontier: &[(Uuid, Uuid)],
    terminal_frontier: &[StoredTerminalFrontierMember],
    session: Uuid,
    turn: Uuid,
) -> bool {
    if terminal_frontier.len() != source_frontier.len() + 1
        || terminal_frontier
            .iter()
            .zip(source_frontier)
            .any(|(stored, expected)| (stored.source_session, stored.entry) != *expected)
    {
        return false;
    }
    let failure = &terminal_frontier[source_frontier.len()];
    failure.source_session == session
        && failure.payload_kind == "turn_failed"
        && failure.assistant_text.is_none()
        && failure.producing_call.is_none()
        && failure.completed_turn.is_none()
        && failure.failed_turn == Some(turn)
        && failure.cancelled_turn.is_none()
}

fn pending_reclassification_candidates(
    execution: &ModelCallExecution,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<Vec<PendingSteeringReclassificationIdentity>, ModelCallRepositoryError> {
    pending_reclassification_candidates_from_parts(
        execution.active_turn().turn(),
        execution.active_turn().pending_steering(),
        next_turn,
    )
}

fn pending_reclassification_candidates_for_active(
    active_turn: &signalbox_domain::ActivatedAcceptedInputTurn,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<Vec<PendingSteeringReclassificationIdentity>, ModelCallRepositoryError> {
    pending_reclassification_candidates_from_parts(
        active_turn.turn(),
        active_turn.pending_steering(),
        next_turn,
    )
}

fn pending_reclassification_candidates_for_activated(
    active_turn: &signalbox_domain::ActivatedTurn,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<Vec<PendingSteeringReclassificationIdentity>, ModelCallRepositoryError> {
    pending_reclassification_candidates_from_parts(
        active_turn.turn(),
        active_turn.pending_steering(),
        next_turn,
    )
}

fn pending_reclassification_candidates_from_parts(
    source_turn: TurnId,
    pending_steering: &[signalbox_domain::PendingSteeringInput],
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<Vec<PendingSteeringReclassificationIdentity>, ModelCallRepositoryError> {
    let mut proposed_turns = BTreeSet::new();
    let mut reclassifications = Vec::new();
    for pending in pending_steering {
        let accepted_input = pending.accepted_input();
        let proposed_turn = next_turn(accepted_input);
        record_reclassified_turn_candidate(source_turn, proposed_turn, &mut proposed_turns)?;
        reclassifications.push(PendingSteeringReclassificationIdentity::new(
            accepted_input,
            proposed_turn,
        ));
    }
    Ok(reclassifications)
}

pub(crate) fn attach_interrupt_reclassification_candidates(
    identities: signalbox_domain::CancelledModelCallTurnIdentities,
    execution: &ModelCallExecution,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<signalbox_domain::CancelledModelCallTurnIdentities, ModelCallRepositoryError> {
    Ok(
        identities.with_pending_steering_reclassifications(pending_reclassification_candidates(
            execution, next_turn,
        )?),
    )
}

pub(crate) fn attach_interrupt_reclassification_candidates_for_active(
    identities: signalbox_domain::CancelledModelCallTurnIdentities,
    active_turn: &signalbox_domain::ActivatedAcceptedInputTurn,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<signalbox_domain::CancelledModelCallTurnIdentities, ModelCallRepositoryError> {
    Ok(identities.with_pending_steering_reclassifications(
        pending_reclassification_candidates_for_active(active_turn, next_turn)?,
    ))
}

pub(crate) fn attach_interrupt_reclassification_candidates_for_activated(
    identities: signalbox_domain::CancelledModelCallTurnIdentities,
    active_turn: &signalbox_domain::ActivatedTurn,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<signalbox_domain::CancelledModelCallTurnIdentities, ModelCallRepositoryError> {
    Ok(identities.with_pending_steering_reclassifications(
        pending_reclassification_candidates_for_activated(active_turn, next_turn)?,
    ))
}

pub(crate) fn attach_recovery_interrupt_reclassification_candidates(
    identities: signalbox_domain::AmbiguousModelCallTurnIdentities,
    active_turn: &signalbox_domain::ActivatedAcceptedInputTurn,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<signalbox_domain::AmbiguousModelCallTurnIdentities, ModelCallRepositoryError> {
    Ok(identities.with_pending_steering_reclassifications(
        pending_reclassification_candidates_for_active(active_turn, next_turn)?,
    ))
}

pub(crate) fn attach_recovery_interrupt_reclassification_candidates_for_activated(
    identities: signalbox_domain::AmbiguousModelCallTurnIdentities,
    active_turn: &signalbox_domain::ActivatedTurn,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<signalbox_domain::AmbiguousModelCallTurnIdentities, ModelCallRepositoryError> {
    Ok(identities.with_pending_steering_reclassifications(
        pending_reclassification_candidates_for_activated(active_turn, next_turn)?,
    ))
}

fn record_reclassified_turn_candidate(
    source_turn: TurnId,
    proposed_turn: TurnId,
    proposed_turns: &mut BTreeSet<TurnId>,
) -> Result<(), ModelCallRepositoryError> {
    if proposed_turn == source_turn || !proposed_turns.insert(proposed_turn) {
        return Err(ModelCallRepositoryError::IdentityCollision(
            ModelCallIdentityCollision::ReclassifiedTurn,
        ));
    }
    Ok(())
}

fn select_terminal_identity_candidates(
    identities: ModelCallTerminalIdentityCandidates,
    execution: &ModelCallExecution,
) -> ModelCallTerminalIdentityCandidates {
    match identities {
        ModelCallTerminalIdentityCandidates::Exact(identities) => {
            ModelCallTerminalIdentityCandidates::Exact(identities)
        }
        ModelCallTerminalIdentityCandidates::ToolRound {
            continuing,
            stopped,
        } => {
            if matches!(
                execution.current_attempt().state(),
                signalbox_domain::CurrentTurnAttemptState::StopRequested { .. }
            ) {
                ModelCallTerminalIdentityCandidates::Exact(
                    ModelCallTerminalIdentities::StoppedToolRound(stopped),
                )
            } else {
                ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::ToolRound(
                    continuing,
                ))
            }
        }
        // A pending stop is handled inside the availability branch rather
        // than by downgrading here. Converting to `Exact` closed the turn
        // correctly but skipped the frozen policy entirely, so a racing stop
        // silently dropped a configured switch_next_turn, avoid_new_sessions,
        // or quarantine and left the failed credential selectable. The branch
        // suppresses only successor creation, which is what the stop forbids.
        ModelCallTerminalIdentityCandidates::Availability {
            failed,
            successor_attempt,
        } => ModelCallTerminalIdentityCandidates::Availability {
            failed,
            successor_attempt,
        },
    }
}

fn attach_pending_reclassification_candidates(
    identities: ModelCallTerminalIdentityCandidates,
    execution: &ModelCallExecution,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<ModelCallTerminalIdentityCandidates, ModelCallRepositoryError> {
    let reclassifications = pending_reclassification_candidates(execution, next_turn)?;
    let identities = match identities {
        ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Completed(
            identities,
        )) => ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Completed(
            identities.with_pending_steering_reclassifications(reclassifications),
        )),
        ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::ToolRound(
            identities,
        )) => ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::ToolRound(
            identities,
        )),
        ModelCallTerminalIdentityCandidates::Exact(
            ModelCallTerminalIdentities::StoppedToolRound(identities),
        ) => ModelCallTerminalIdentityCandidates::Exact(
            ModelCallTerminalIdentities::StoppedToolRound(
                identities.with_pending_steering_reclassifications(reclassifications),
            ),
        ),
        ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Failed(
            identities,
        )) => ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Failed(
            identities.with_pending_steering_reclassifications(reclassifications),
        )),
        ModelCallTerminalIdentityCandidates::Exact(
            ModelCallTerminalIdentities::PhysicalCancellation(identities),
        ) => ModelCallTerminalIdentityCandidates::Exact(
            ModelCallTerminalIdentities::PhysicalCancellation(
                identities.with_pending_steering_reclassifications(reclassifications),
            ),
        ),
        ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Refused(
            identities,
        )) => ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Refused(
            identities.with_pending_steering_reclassifications(reclassifications),
        )),
        ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Ambiguous(
            identities,
        )) => ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Ambiguous(
            identities.with_pending_steering_reclassifications(reclassifications),
        )),
        ModelCallTerminalIdentityCandidates::Availability {
            failed,
            successor_attempt,
        } => ModelCallTerminalIdentityCandidates::Availability {
            failed: failed.with_pending_steering_reclassifications(reclassifications),
            successor_attempt,
        },
        ModelCallTerminalIdentityCandidates::ToolRound { .. } => {
            return Err(ModelCallRepositoryError::InvalidTransition(
                "tool terminal candidates were not selected before reclassification",
            ));
        }
    };
    Ok(identities)
}

fn prepared_matches_authorized(
    prepared: &PreparedModelCallRequest,
    authorized: &AuthorizedModelCall,
) -> bool {
    prepared.session() == authorized.session()
        && prepared.turn() == authorized.turn()
        && prepared.attempt() == authorized.attempt().id()
        && prepared.call().id() == authorized.call().id()
        && prepared.call().selection() == authorized.call().selection()
        && prepared.call().target() == authorized.call().target()
        && prepared.call().frontier() == authorized.call().frontier()
        && prepared
            .frontier_entries()
            .eq(authorized.frontier_entries())
        && prepared
            .frontier_entries()
            .all(|entry| match entry.payload() {
                SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
                | SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input, ..
                } => {
                    prepared.origin_content(*accepted_input)
                        == authorized.origin_content(*accepted_input)
                }
                _ => true,
            })
}

fn prepared_matches_stopped(
    prepared: &PreparedModelCallRequest,
    execution: &ModelCallExecution,
    stopped: &StopRequestedModelCallTurn,
) -> bool {
    prepared.session() == stopped.session()
        && prepared.turn() == stopped.turn()
        && prepared.attempt() == stopped.attempt().id()
        && prepared.call().id() == stopped.call().id()
        && prepared.call().selection() == stopped.call().selection()
        && prepared.call().target() == stopped.call().target()
        && prepared.call().frontier() == stopped.call().frontier()
        && prepared.frontier_entries().eq(execution.frontier_entries())
        && prepared
            .frontier_entries()
            .all(|entry| match entry.payload() {
                SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
                | SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input, ..
                } => {
                    prepared.origin_content(*accepted_input)
                        == execution.origin_content(*accepted_input)
                }
                _ => true,
            })
}

pub(crate) async fn lock_session(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<(), ModelCallRepositoryError> {
    let (session_exists, scheduler): (bool, Option<Uuid>) =
        sqlx::query_as(crate::lock_inventory::START_ELIGIBLE_TURN)
            .bind(session_id_to_uuid(session))
            .fetch_one(connection)
            .await?;
    match (session_exists, scheduler) {
        (true, Some(_)) => Ok(()),
        (true, None) => Err(ModelCallCorruption::Missing("session scheduler row").into()),
        (false, None) => Err(ModelCallRepositoryError::NoLiveExecution),
        (false, Some(_)) => Err(ModelCallCorruption::Inconsistent("orphan scheduler row").into()),
    }
}

async fn require_live_execution(
    connection: &mut PgConnection,
    requested_session: SessionId,
    targets: &ModelTargetCatalog,
) -> Result<ModelCallExecution, ModelCallRepositoryError> {
    // Boxed because a debug `async fn` frame carries every future it awaits
    // inline: this one-line wrapper otherwise puts the whole reconstitution
    // state machine on the caller stack.
    Box::pin(require_live_execution_with_targets(
        connection,
        requested_session,
        Some(targets),
        None,
        None,
    ))
    .await
}

pub(crate) async fn require_live_execution_for_restart(
    connection: &mut PgConnection,
    requested_session: SessionId,
) -> Result<ModelCallExecution, ModelCallRepositoryError> {
    // Boxed because a debug `async fn` frame carries every future it awaits
    // inline: this one-line wrapper otherwise puts the whole reconstitution
    // state machine on the caller stack.
    Box::pin(require_live_execution_with_targets(
        connection,
        requested_session,
        None,
        None,
        None,
    ))
    .await
}

struct LoadedDelegatedLiveTurn {
    active: signalbox_domain::ActivatedTurn,
    starting_snapshot: ResolvedContextFrontierSnapshot,
    recovery: Option<DelegatedModelCallRecovery>,
}

pub(crate) struct DelegatedModelCallRecovery {
    pub(crate) active: signalbox_domain::ActivatedTurn,
    pub(crate) call: signalbox_domain::EndedModelCall,
    pub(crate) attempt: signalbox_domain::EndedTurnAttempt,
    pub(crate) source_snapshot: ResolvedContextFrontierSnapshot,
}

pub(crate) async fn load_delegated_runner_recovery_for_interrupt(
    connection: &mut PgConnection,
    requested_session: SessionId,
) -> Result<
    Option<(
        signalbox_domain::ActivatedTurn,
        ResolvedContextFrontierSnapshot,
    )>,
    ModelCallRepositoryError,
> {
    let has_delegated_runner_recovery = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM turn_lifecycle
             WHERE session_id = $1
               AND origin_kind = 'delegation'
               AND state_kind = 'active'
               AND active_phase_kind = 'awaiting_runner_recovery'
               AND NOT delegation_runtime_terminal
               AND goal_turn_is_runtime_relevant(session_id, turn_id)
        )",
    )
    .bind(session_id_to_uuid(requested_session))
    .fetch_one(&mut *connection)
    .await?;
    if !has_delegated_runner_recovery {
        return Ok(None);
    }

    let session = match load_session_from_connection(connection, requested_session).await {
        Ok(Some(session)) => session,
        Ok(None) => return Ok(None),
        Err(SessionRepositoryError::Database(error)) => return Err(error.into()),
        Err(SessionRepositoryError::Corruption(error)) => {
            return Err(ModelCallCorruption::CurrentSession(error).into());
        }
    };
    // Boxed for the same reason: the scheduling projection reconstitutes the
    // whole accepted-input order, and awaiting it inline places that frame on
    // top of this one.
    let scheduling = Box::pin(load_scheduling_projection(connection, session))
        .await
        .map_err(map_scheduling_error)?;
    Ok(
        load_delegated_live_turn(connection, requested_session, &scheduling)
            .await?
            .filter(|loaded| {
                matches!(
                    loaded.active.phase(),
                    signalbox_domain::ActiveTurnPhase::AwaitingRunnerRecovery { .. }
                )
            })
            .map(|loaded| (loaded.active, loaded.starting_snapshot)),
    )
}

pub(crate) async fn load_delegated_model_call_recovery(
    connection: &mut PgConnection,
    session: SessionId,
    scheduling: &signalbox_domain::AcceptedInputSchedulingProjection,
) -> Result<Option<DelegatedModelCallRecovery>, ModelCallRepositoryError> {
    Ok(load_delegated_live_turn(connection, session, scheduling)
        .await?
        .and_then(|loaded| loaded.recovery))
}

async fn require_live_execution_with_targets(
    connection: &mut PgConnection,
    requested_session: SessionId,
    configured_targets: Option<&ModelTargetCatalog>,
    continuation_snapshot: Option<ResolvedContextFrontierReconstitutionInput>,
    uncommitted_tool_result_projection: Option<PreparedToolResultProjection>,
) -> Result<ModelCallExecution, ModelCallRepositoryError> {
    let session = match load_session_from_connection(connection, requested_session).await {
        Ok(Some(session)) => session,
        Ok(None) => return Err(ModelCallRepositoryError::NoLiveExecution),
        Err(SessionRepositoryError::Database(error)) => return Err(error.into()),
        Err(SessionRepositoryError::Corruption(error)) => {
            return Err(ModelCallCorruption::CurrentSession(error).into());
        }
    };
    // Boxed for the same reason: the scheduling projection reconstitutes the
    // whole accepted-input order, and awaiting it inline places that frame on
    // top of this one.
    let scheduling = Box::pin(load_scheduling_projection(connection, session))
        .await
        .map_err(map_scheduling_error)?;
    let delegated = load_delegated_live_turn(connection, requested_session, &scheduling).await?;
    let (active_turn, starting_snapshot) = match delegated {
        Some(loaded) => (loaded.active, loaded.starting_snapshot),
        None => {
            let active = scheduling
                .active_turn_execution()
                .ok_or(ModelCallRepositoryError::NoLiveExecution)?;
            let starting = scheduling
                .resolved_snapshot(active.start().frontier().snapshot())
                .cloned()
                .ok_or(ModelCallCorruption::Missing("starting snapshot"))?;
            (active.into(), starting)
        }
    };
    if !matches!(
        active_turn.phase(),
        signalbox_domain::ActiveTurnPhase::Running { .. }
    ) {
        return Err(ModelCallRepositoryError::NoLiveExecution);
    }
    let (pinned_target, calls) =
        load_live_turn_calls(connection, requested_session, active_turn.turn()).await?;
    let signalbox_domain::ActiveTurnPhase::Running { current_attempt } = active_turn.phase() else {
        return Err(ModelCallRepositoryError::NoLiveExecution);
    };
    let availability_successor: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM credential_pool_availability_successor
              WHERE successor_turn_attempt_id = $1
         )",
    )
    .bind(current_attempt.id().into_uuid())
    .fetch_one(&mut *connection)
    .await?;
    let call_snapshot = match calls
        .first()
        .filter(|call| call.frontier() != starting_snapshot.frontier().snapshot())
    {
        Some(call) => {
            Some(load_call_snapshot(connection, requested_session, call.frontier()).await?)
        }
        None => None,
    };
    let successor_snapshot = if availability_successor && call_snapshot.is_none() {
        load_availability_predecessor_snapshot(
            connection,
            requested_session,
            current_attempt.id(),
            starting_snapshot.frontier().snapshot(),
        )
        .await?
    } else {
        None
    };
    let current_snapshot = call_snapshot
        .as_ref()
        .or(continuation_snapshot.as_ref())
        .or(successor_snapshot.as_ref());
    let frontier_references = current_snapshot.as_ref().map_or_else(
        || starting_snapshot.ordered_entries().collect::<Vec<_>>(),
        |snapshot| snapshot.ordered_entries().to_vec(),
    );
    let frontier_entries = frontier_references
        .iter()
        .map(|reference| {
            scheduling
                .semantic_entry(*reference)
                .cloned()
                .ok_or(ModelCallCorruption::Missing("frontier semantic entry"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let origin_contents = load_origin_contents(
        connection,
        &frontier_entries,
        active_turn.pending_steering(),
        active_turn.consumed_steering(),
    )
    .await?;
    let attachment_blob_facts = load_attachment_blob_facts(connection, &origin_contents).await?;
    let tool_result_correlations =
        load_tool_result_correlations(connection, &frontier_entries).await?;
    let tool_denial_correlations =
        load_tool_denial_correlations(connection, &frontier_entries).await?;
    let recovered_targets;
    let targets = if let Some(targets) = configured_targets {
        targets.clone()
    } else {
        let mut definitions = calls
            .iter()
            .map(|call| {
                let direct = match call.selection() {
                    FrozenModelSelection::Direct(direct) => direct,
                    FrozenModelSelection::FrozenAlias { definition, .. } => definition.selected(),
                };
                ModelTargetDefinition::new(direct, call.target())
            })
            .collect::<Vec<_>>();
        if let Some(pinned) = pinned_target
            && calls.is_empty()
        {
            let direct = match *active_turn.configuration().effective().model() {
                FrozenModelSelection::Direct(direct) => direct,
                FrozenModelSelection::FrozenAlias { definition, .. } => definition.selected(),
            };
            definitions.push(ModelTargetDefinition::new(direct, pinned.target()));
        }
        recovered_targets = ModelTargetCatalog::try_from_definitions(definitions)
            .map_err(|_| ModelCallCorruption::Inconsistent("recovery model-target catalog"))?;
        recovered_targets
    };

    let mut input = ModelCallExecutionReconstitutionInput::new(
        active_turn,
        targets,
        starting_snapshot,
        frontier_entries,
        origin_contents,
        pinned_target,
        calls,
    )
    .with_attachment_blob_facts(attachment_blob_facts)
    .with_tool_result_correlations(tool_result_correlations)
    .with_tool_denial_correlations(tool_denial_correlations);
    if availability_successor {
        input = input.with_availability_successor();
    }
    if let Some(projection) = uncommitted_tool_result_projection {
        input = input.with_uncommitted_tool_result_projection(projection);
    }
    if let Some(call_snapshot) = call_snapshot {
        input = input.with_call_snapshot(call_snapshot);
    } else if let Some(snapshot) = continuation_snapshot.or(successor_snapshot) {
        input = input.with_continuation_snapshot(snapshot);
    }
    input.reconstitute().map_err(|error| {
        let (_, failure) = error.into_parts();
        ModelCallCorruption::Execution(failure).into()
    })
}

async fn load_delegated_live_turn(
    connection: &mut PgConnection,
    session: SessionId,
    scheduling: &signalbox_domain::AcceptedInputSchedulingProjection,
) -> Result<Option<LoadedDelegatedLiveTurn>, ModelCallRepositoryError> {
    let row = sqlx::query(
        "SELECT
            task.spawning_tool_request_id,
            task.turn_id,
            task.semantic_entry_id,
            task.task_content,
            relation.parent_session_id,
            relation.parent_turn_id,
            lifecycle.starting_frontier_id,
            attempt.turn_attempt_id AS projection_attempt_id,
            attempt.state_kind AS attempt_state_kind,
            attempt.end_variant,
            attempt.end_disposition,
            attempt.interrupt_command_id,
            attempt.interrupt_predecessor_turn_id AS attempt_interrupt_predecessor_turn_id,
            lifecycle.active_phase_kind,
            lifecycle.recovery_model_call_id,
            lifecycle.pinned_provider_model_identity_id,
            lifecycle.runner_recovery_runner_id,
            lifecycle.runner_recovery_placement_revision,
            lifecycle.runner_recovery_tool_attempt_id,
            defaults.session_id AS goal_defaults_session_id,
            task.defaults_version AS queued_defaults_version,
            defaults.version AS goal_defaults_version,
            defaults.model_selection_kind AS goal_defaults_model_kind,
            defaults.direct_model_selection_id AS goal_defaults_direct_id,
            defaults.model_alias_id AS goal_defaults_alias_id,
            defaults.dangerous_tool_auto_approval AS goal_defaults_tool_auto_approval,
            defaults.model_settings AS goal_defaults_model_settings,
            task.requested_model_kind,
            task.requested_direct_model_selection_id,
            task.requested_model_alias_id,
            task.frozen_model_kind,
            task.frozen_direct_model_selection_id,
            task.frozen_model_alias_id,
            task.frozen_alias_selected_direct_id
         FROM turn_lifecycle AS lifecycle
         JOIN session_delegation_initial_task AS task
           ON task.turn_id = lifecycle.turn_id
          AND task.child_session_id = lifecycle.session_id
          AND task.admission_position = lifecycle.acceptance_position
         JOIN session_delegation AS relation
           ON relation.spawning_tool_request_id = task.spawning_tool_request_id
          AND relation.child_session_id = task.child_session_id
         JOIN turn_attempt AS attempt
           ON attempt.turn_id = lifecycle.turn_id
          AND attempt.session_id = lifecycle.session_id
          AND (
                (
                    lifecycle.active_phase_kind = 'running'
                    AND attempt.turn_attempt_id = lifecycle.current_attempt_id
                )
                OR (
                    lifecycle.active_phase_kind = 'awaiting_runner_recovery'
                    AND attempt.state_kind = 'ended'
                    AND attempt.end_variant = 'without_stop'
                    AND attempt.end_disposition = 'yielded_to_durable_wait'
                    AND attempt.interrupt_command_id IS NULL
                    AND attempt.interrupt_predecessor_turn_id IS NULL
                    AND NOT EXISTS (
                        SELECT 1
                          FROM turn_attempt AS continuation
                         WHERE continuation.continued_from_attempt_id =
                                attempt.turn_attempt_id
                    )
                )
                OR (
                    lifecycle.active_phase_kind = 'awaiting_model_call_recovery'
                    AND attempt.turn_attempt_id = lifecycle.current_attempt_id
                    AND attempt.state_kind = 'ended'
                    AND attempt.end_variant IN ('without_stop', 'after_cancellation')
                    AND attempt.end_disposition IN ('ambiguous', 'lost')
                )
          )
         JOIN session_defaults_version AS defaults
           ON defaults.session_id = task.child_session_id
          AND defaults.version = task.defaults_version
        WHERE lifecycle.session_id = $1
          AND lifecycle.origin_kind = 'delegation'
          AND lifecycle.state_kind = 'active'
          AND NOT lifecycle.delegation_runtime_terminal
          AND lifecycle.active_phase_kind IN (
                'running', 'awaiting_runner_recovery',
                'awaiting_model_call_recovery'
          )
          AND goal_turn_is_runtime_relevant(
                lifecycle.session_id, lifecycle.turn_id
          )",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return load_delegated_live_wake_turn(connection, session, scheduling).await;
    };
    let turn = TurnId::from_uuid(required(&row, "turn_id")?);
    let spawning_request =
        signalbox_domain::ToolRequestId::from_uuid(required(&row, "spawning_tool_request_id")?);
    let task = DelegationContent::try_new(required(&row, "task_content")?)
        .map_err(|_| ModelCallCorruption::Inconsistent("delegated task content"))?;
    let configuration =
        decode_goal_origin_configuration(&row, session).map_err(map_scheduling_error)?;
    let starting_frontier =
        signalbox_domain::ContextFrontierId::from_uuid(required(&row, "starting_frontier_id")?);
    let initial_attempt =
        signalbox_domain::TurnAttemptId::from_uuid(required(&row, "projection_attempt_id")?);
    let task_entry = SemanticTranscriptEntryReconstitutionInput::new(
        SemanticTranscriptEntryId::from_uuid(required(&row, "semantic_entry_id")?),
        session,
        SemanticTranscriptEntryPayload::DelegatedTask {
            spawning_request,
            parent_session: SessionId::from_uuid(required(&row, "parent_session_id")?),
            parent_turn: TurnId::from_uuid(required(&row, "parent_turn_id")?),
            content: task.clone(),
        },
    );
    let prepared = PreparedDelegatedTurnActivation::prepare(DelegatedTurnActivationInput {
        session,
        turn,
        spawning_request,
        task,
        task_entry,
        configuration,
        starting_frontier,
        initial_attempt,
    })
    .ok_or(ModelCallCorruption::Inconsistent(
        "delegated live-turn projection",
    ))?;
    let recovery_call = (required::<String>(&row, "active_phase_kind")?
        == "awaiting_model_call_recovery")
        .then(|| required::<Uuid>(&row, "recovery_model_call_id").map(ModelCallId::from_uuid))
        .transpose()?;
    let phase = decode_delegated_active_phase(connection, &row, turn, initial_attempt).await?;
    finish_loaded_delegated_turn(
        connection,
        session,
        turn,
        starting_frontier,
        prepared,
        phase,
        recovery_call,
    )
    .await
    .map(Some)
}

async fn finish_loaded_delegated_turn(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    starting_frontier: signalbox_domain::ContextFrontierId,
    prepared: PreparedDelegatedTurnActivation,
    phase: ActiveTurnSchedulingReconstitutionInput,
    recovery_call: Option<ModelCallId>,
) -> Result<LoadedDelegatedLiveTurn, ModelCallRepositoryError> {
    let pending = load_delegated_pending_steering(connection, session, turn).await?;
    let consumed = load_delegated_consumed_steering(connection, session, turn).await?;
    let stored_snapshot = load_call_snapshot(connection, session, starting_frontier)
        .await?
        .reconstitute()
        .ok_or(ModelCallCorruption::Inconsistent(
            "delegated starting snapshot",
        ))?;

    if let Some(recovery_call_id) = recovery_call {
        let (pinned, calls) = load_live_turn_calls(connection, session, turn).await?;
        let pinned = pinned.ok_or(ModelCallCorruption::Missing(
            "delegated recovery pinned target",
        ))?;
        let recovery_call = calls
            .into_iter()
            .find(|call| {
                call.id() == recovery_call_id
                    && call.state()
                        == ModelCallReconstitutionState::Terminal(ModelCallDisposition::Ambiguous)
            })
            .ok_or(ModelCallCorruption::Missing(
                "delegated recovery model call",
            ))?;
        let source_snapshot =
            load_call_snapshot(connection, session, recovery_call.frontier()).await?;
        let (active, call, attempt, source_snapshot, prepared_snapshot) = prepared
            .with_reconstituted_model_call_recovery(
                DelegatedModelCallRecoveryReconstitutionInput::new(
                    phase,
                    pinned,
                    recovery_call,
                    source_snapshot,
                    pending,
                    consumed,
                ),
            )
            .ok_or(ModelCallCorruption::Inconsistent(
                "delegated recovery evidence",
            ))?;
        if stored_snapshot != prepared_snapshot {
            return Err(ModelCallCorruption::Inconsistent("delegated starting snapshot").into());
        }
        return Ok(LoadedDelegatedLiveTurn {
            active: active.clone(),
            starting_snapshot: stored_snapshot,
            recovery: Some(DelegatedModelCallRecovery {
                active,
                call,
                attempt,
                source_snapshot,
            }),
        });
    }

    let (active, _, prepared_snapshot) =
        prepared
            .with_reconstituted_phase(phase)
            .ok_or(ModelCallCorruption::Inconsistent(
                "delegated live-turn phase",
            ))?;
    let active = active
        .with_pending_steering(pending)
        .ok_or(ModelCallCorruption::Inconsistent(
            "delegated pending steering",
        ))?
        .with_consumed_steering(consumed)
        .ok_or(ModelCallCorruption::Inconsistent(
            "delegated consumed steering",
        ))?;
    if stored_snapshot != prepared_snapshot {
        return Err(ModelCallCorruption::Inconsistent("delegated starting snapshot").into());
    }
    Ok(LoadedDelegatedLiveTurn {
        active: active.into(),
        starting_snapshot: stored_snapshot,
        recovery: None,
    })
}

async fn decode_delegated_active_phase(
    connection: &mut PgConnection,
    row: &PgRow,
    turn: TurnId,
    projection_attempt: signalbox_domain::TurnAttemptId,
) -> Result<ActiveTurnSchedulingReconstitutionInput, ModelCallRepositoryError> {
    match required::<String>(row, "active_phase_kind")?.as_str() {
        "running" => match required::<String>(row, "attempt_state_kind")?.as_str() {
            "prepared" => Ok(ActiveTurnSchedulingReconstitutionInput::prepared(
                turn,
                projection_attempt,
            )),
            "running" => Ok(ActiveTurnSchedulingReconstitutionInput::running(
                turn,
                projection_attempt,
            )),
            value => Err(ModelCallCorruption::Unsupported {
                field: "delegated turn attempt state",
                value: value.to_owned(),
            }
            .into()),
        },
        "awaiting_runner_recovery" => {
            let runner =
                signalbox_domain::RunnerId::from_uuid(required(row, "runner_recovery_runner_id")?);
            let revision =
                positive_u64_from_numeric(required(row, "runner_recovery_placement_revision")?)
                    .ok()
                    .and_then(signalbox_domain::RunnerGeneration::try_from_u64)
                    .ok_or(ModelCallCorruption::Inconsistent(
                        "delegated runner recovery placement revision",
                    ))?;
            Ok(
                ActiveTurnSchedulingReconstitutionInput::awaiting_runner_recovery(
                    turn,
                    runner,
                    revision,
                    row.try_get::<Option<Uuid>, _>("runner_recovery_tool_attempt_id")?
                        .map(signalbox_domain::ToolAttemptId::from_uuid),
                    None,
                ),
            )
        }
        "awaiting_model_call_recovery" => {
            let call = ModelCallId::from_uuid(required(row, "recovery_model_call_id")?);
            if required::<String>(row, "attempt_state_kind")? != "ended" {
                return Err(
                    ModelCallCorruption::Inconsistent("delegated recovery attempt state").into(),
                );
            }
            match (
                required::<String>(row, "end_variant")?.as_str(),
                required::<String>(row, "end_disposition")?.as_str(),
            ) {
                ("without_stop", "ambiguous") => Ok(
                    ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery(
                        turn,
                        projection_attempt,
                        call,
                    ),
                ),
                ("without_stop", "lost") => Ok(
                    ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_restart(
                        turn,
                        projection_attempt,
                        call,
                    ),
                ),
                ("after_cancellation", disposition @ ("ambiguous" | "lost")) => {
                    let command = durable_command_id_from_uuid(required(
                        row,
                        "interrupt_command_id",
                    )?)
                    .map_err(|_| {
                        ModelCallCorruption::Inconsistent(
                            "delegated recovery interrupt identity",
                        )
                    })?;
                    let recorded = require_recorded_batch(connection, &[command])
                        .await
                        .map_err(map_scheduling_error)?;
                    let interrupt = require_applied_interrupt_from_attempt(row, turn, &recorded)
                        .map_err(map_scheduling_error)?;
                    if disposition == "ambiguous" {
                        Ok(ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_cancellation(
                            turn,
                            projection_attempt,
                            call,
                            interrupt,
                        ))
                    } else {
                        Ok(ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_cancellation_restart(
                            turn,
                            projection_attempt,
                            call,
                            interrupt,
                        ))
                    }
                }
                _ => Err(ModelCallCorruption::Inconsistent(
                    "delegated recovery attempt end",
                )
                .into()),
            }
        }
        value => Err(ModelCallCorruption::Unsupported {
            field: "delegated active phase",
            value: value.to_owned(),
        }
        .into()),
    }
}

async fn load_delegated_live_wake_turn(
    connection: &mut PgConnection,
    session: SessionId,
    scheduling: &signalbox_domain::AcceptedInputSchedulingProjection,
) -> Result<Option<LoadedDelegatedLiveTurn>, ModelCallRepositoryError> {
    let row = sqlx::query(
        "SELECT
            wake.turn_id,
            wake.first_delivery_sequence,
            wake.through_delivery_sequence,
            predecessor.turn_id AS predecessor_turn_id,
            turn_lifecycle_effective_terminal_frontier(
                predecessor.session_id, predecessor.turn_id
            ) AS predecessor_frontier_id,
            lifecycle.starting_frontier_id,
            attempt.turn_attempt_id AS projection_attempt_id,
            attempt.state_kind AS attempt_state_kind,
            attempt.end_variant,
            attempt.end_disposition,
            attempt.interrupt_command_id,
            attempt.interrupt_predecessor_turn_id AS attempt_interrupt_predecessor_turn_id,
            lifecycle.active_phase_kind,
            lifecycle.recovery_model_call_id,
            lifecycle.pinned_provider_model_identity_id,
            lifecycle.runner_recovery_runner_id,
            lifecycle.runner_recovery_placement_revision,
            lifecycle.runner_recovery_tool_attempt_id,
            defaults.session_id AS goal_defaults_session_id,
            wake.defaults_version AS queued_defaults_version,
            defaults.version AS goal_defaults_version,
            defaults.model_selection_kind AS goal_defaults_model_kind,
            defaults.direct_model_selection_id AS goal_defaults_direct_id,
            defaults.model_alias_id AS goal_defaults_alias_id,
            defaults.dangerous_tool_auto_approval AS goal_defaults_tool_auto_approval,
            defaults.model_settings AS goal_defaults_model_settings,
            wake.requested_model_kind,
            wake.requested_direct_model_selection_id,
            wake.requested_model_alias_id,
            wake.frozen_model_kind,
            wake.frozen_direct_model_selection_id,
            wake.frozen_model_alias_id,
            wake.frozen_alias_selected_direct_id
         FROM session_delegation_wake_turn_origin AS wake
         JOIN turn_lifecycle AS lifecycle
           ON lifecycle.turn_id = wake.turn_id
          AND lifecycle.session_id = wake.recipient_session_id
          AND lifecycle.acceptance_position = wake.admission_position
          AND lifecycle.origin_kind = 'delegation'
          AND lifecycle.state_kind = 'active'
          AND NOT lifecycle.delegation_runtime_terminal
          AND lifecycle.active_phase_kind IN (
                'running', 'awaiting_runner_recovery',
                'awaiting_model_call_recovery'
          )
         JOIN turn_lifecycle AS predecessor
           ON predecessor.turn_id = lifecycle.immediate_predecessor_turn_id
          AND predecessor.session_id = lifecycle.session_id
          AND (
                predecessor.delegation_runtime_terminal
                OR (
                    predecessor.state_kind = 'terminal'
                    AND predecessor.terminal_disposition_kind IN (
                        'failed', 'completed', 'refused', 'cancelled',
                        'reconciliation_required'
                    )
                )
          )
         JOIN turn_attempt AS attempt
           ON attempt.turn_id = lifecycle.turn_id
          AND attempt.session_id = lifecycle.session_id
          AND (
                (
                    lifecycle.active_phase_kind = 'running'
                    AND attempt.turn_attempt_id = lifecycle.current_attempt_id
                )
                OR (
                    lifecycle.active_phase_kind = 'awaiting_runner_recovery'
                    AND attempt.state_kind = 'ended'
                    AND attempt.end_variant = 'without_stop'
                    AND attempt.end_disposition = 'yielded_to_durable_wait'
                    AND attempt.interrupt_command_id IS NULL
                    AND attempt.interrupt_predecessor_turn_id IS NULL
                    AND NOT EXISTS (
                        SELECT 1
                          FROM turn_attempt AS continuation
                         WHERE continuation.continued_from_attempt_id =
                                attempt.turn_attempt_id
                    )
                )
                OR (
                    lifecycle.active_phase_kind = 'awaiting_model_call_recovery'
                    AND attempt.turn_attempt_id = lifecycle.current_attempt_id
                    AND attempt.state_kind = 'ended'
                    AND attempt.end_variant IN ('without_stop', 'after_cancellation')
                    AND attempt.end_disposition IN ('ambiguous', 'lost')
                )
          )
         JOIN session_defaults_version AS defaults
           ON defaults.session_id = wake.recipient_session_id
          AND defaults.version = wake.defaults_version
        WHERE wake.recipient_session_id = $1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let first_numeric: Decimal = required(&row, "first_delivery_sequence")?;
    let through_numeric: Decimal = required(&row, "through_delivery_sequence")?;
    let first = NonZeroU64::new(
        positive_u64_from_numeric(first_numeric)
            .map_err(|_| ModelCallCorruption::Inconsistent("wake delivery range"))?,
    )
    .ok_or(ModelCallCorruption::Inconsistent("wake delivery range"))?;
    let through = NonZeroU64::new(
        positive_u64_from_numeric(through_numeric)
            .map_err(|_| ModelCallCorruption::Inconsistent("wake delivery range"))?,
    )
    .ok_or(ModelCallCorruption::Inconsistent("wake delivery range"))?;
    let delivery_rows = sqlx::query_as::<_, (Decimal, Uuid)>(
        "SELECT pending.delivery_sequence,
                delegation_delivery_semantic_entry(
                    pending.recipient_session_id, pending.delivery_sequence
                )
           FROM session_pending_delivery AS pending
          WHERE pending.recipient_session_id = $1
            AND pending.delivery_sequence BETWEEN $2 AND $3
          ORDER BY pending.delivery_sequence",
    )
    .bind(session_id_to_uuid(session))
    .bind(first_numeric)
    .bind(through_numeric)
    .fetch_all(&mut *connection)
    .await?;
    let mut deliveries = Vec::with_capacity(delivery_rows.len());
    for (_, entry) in delivery_rows {
        let reference = SemanticTranscriptEntryRef::from_source(
            session,
            SemanticTranscriptEntryId::from_uuid(entry),
        );
        let semantic = scheduling
            .semantic_entry(reference)
            .ok_or(ModelCallCorruption::Missing("wake semantic entry"))?;
        deliveries.push(SemanticTranscriptEntryReconstitutionInput::new(
            semantic.identity(),
            semantic.source_session(),
            semantic.payload().clone(),
        ));
    }
    let turn = TurnId::from_uuid(required(&row, "turn_id")?);
    let predecessor = TurnId::from_uuid(required(&row, "predecessor_turn_id")?);
    let predecessor_frontier =
        signalbox_domain::ContextFrontierId::from_uuid(required(&row, "predecessor_frontier_id")?);
    let predecessor_snapshot = scheduling
        .resolved_snapshot(predecessor_frontier)
        .cloned()
        .ok_or(ModelCallCorruption::Missing("wake predecessor snapshot"))?;
    let configuration =
        decode_goal_origin_configuration(&row, session).map_err(map_scheduling_error)?;
    let starting_frontier =
        signalbox_domain::ContextFrontierId::from_uuid(required(&row, "starting_frontier_id")?);
    let initial_attempt =
        signalbox_domain::TurnAttemptId::from_uuid(required(&row, "projection_attempt_id")?);
    let prepared =
        PreparedDelegatedTurnActivation::prepare_wake(DelegatedWakeTurnActivationInput {
            session,
            turn,
            first_delivery_sequence: first,
            through_delivery_sequence: through,
            deliveries,
            predecessor,
            predecessor_snapshot,
            configuration,
            starting_frontier,
            initial_attempt,
        })
        .ok_or(ModelCallCorruption::Inconsistent(
            "delegated wake live-turn projection",
        ))?;
    let recovery_call = (required::<String>(&row, "active_phase_kind")?
        == "awaiting_model_call_recovery")
        .then(|| required::<Uuid>(&row, "recovery_model_call_id").map(ModelCallId::from_uuid))
        .transpose()?;
    let phase = decode_delegated_active_phase(connection, &row, turn, initial_attempt).await?;
    finish_loaded_delegated_turn(
        connection,
        session,
        turn,
        starting_frontier,
        prepared,
        phase,
        recovery_call,
    )
    .await
    .map(Some)
}

async fn load_delegated_pending_steering(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<Vec<PendingSteeringInput>, ModelCallRepositoryError> {
    let rows = sqlx::query(
        "SELECT accepted_input_id, acceptance_position
           FROM accepted_input
          WHERE session_id = $1
            AND disposition_kind = 'pending_steering'
            AND expected_active_turn_id = $2
          ORDER BY acceptance_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_all(&mut *connection)
    .await?;
    rows.into_iter()
        .map(|row| {
            let accepted_input = accepted_input_id_from_uuid(required(&row, "accepted_input_id")?);
            let position = input_position_from_numeric(required(&row, "acceptance_position")?)
                .map_err(|_| ModelCallCorruption::Inconsistent("delegated steering position"))?;
            PendingSteeringInput::reconstitute(
                AcceptedInputLifecycle::new(
                    accepted_input,
                    AcceptedInputDisposition::PendingSteering {
                        binding: signalbox_domain::SteeringBinding::new(turn),
                    },
                ),
                position,
                turn,
            )
            .ok_or_else(|| ModelCallCorruption::Inconsistent("delegated pending steering").into())
        })
        .collect()
}

async fn load_delegated_consumed_steering(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<Vec<ConsumedSteeringReconstitutionInput>, ModelCallRepositoryError> {
    let rows = sqlx::query(
        "SELECT accepted_input_id, acceptance_position, consuming_model_call_id
           FROM accepted_input
          WHERE session_id = $1
            AND disposition_kind = 'consumed_as_steering'
            AND expected_active_turn_id = $2
          ORDER BY acceptance_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_all(&mut *connection)
    .await?;
    rows.into_iter()
        .map(|row| {
            let accepted_input = accepted_input_id_from_uuid(required(&row, "accepted_input_id")?);
            let position = input_position_from_numeric(required(&row, "acceptance_position")?)
                .map_err(|_| ModelCallCorruption::Inconsistent("delegated steering position"))?;
            let call = ModelCallId::from_uuid(required(&row, "consuming_model_call_id")?);
            Ok(ConsumedSteeringReconstitutionInput::new(
                session,
                AcceptedInputLifecycle::new(
                    accepted_input,
                    AcceptedInputDisposition::ConsumedAsSteering { call },
                ),
                position,
                turn,
            ))
        })
        .collect()
}

async fn load_tool_denial_correlations(
    connection: &mut PgConnection,
    frontier_entries: &[SemanticTranscriptEntry],
) -> Result<Vec<ToolApprovalResolution>, ModelCallRepositoryError> {
    let requests = frontier_entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::ToolDenied { request } => Some(request.into_uuid()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT approval.request_id, approval.decision_kind,
                approval.decision_source, approval.denial_reason,
                approval.user_command_id,
                approval.delegate_model_selection_id,
                approval.delegate_model_call_id, approval.rationale
           FROM tool_approval_decision AS approval
          WHERE approval.request_id = ANY($1)",
    )
    .bind(&requests)
    .fetch_all(&mut *connection)
    .await?;
    if rows.len() != requests.len() {
        return Err(ModelCallCorruption::Inconsistent("tool-denial resolution ownership").into());
    }
    crate::tool_loop::decode_approvals(connection, rows)
        .await
        .map_err(map_tool_evidence_error)
}

async fn load_tool_result_correlations(
    connection: &mut PgConnection,
    frontier_entries: &[SemanticTranscriptEntry],
) -> Result<Vec<ToolResultAttemptCorrelation>, ModelCallRepositoryError> {
    let attempts = frontier_entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::ToolExecutionResult { attempt } => {
                Some(attempt.into_uuid())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if attempts.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(
        "SELECT attempt.attempt_id,
                request.request_id,
                request.producing_model_call_id
           FROM tool_attempt AS attempt
           JOIN tool_request AS request
             ON request.request_id = attempt.request_id
            AND request.session_id = attempt.session_id
            AND request.turn_id = attempt.turn_id
          WHERE attempt.attempt_id = ANY($1)",
    )
    .bind(&attempts)
    .fetch_all(connection)
    .await?;
    if rows.len() != attempts.len() {
        return Err(ModelCallCorruption::Inconsistent("tool-result attempt ownership").into());
    }
    Ok(rows
        .into_iter()
        .map(|(attempt, request, producing_call)| {
            ToolResultAttemptCorrelation::new(
                signalbox_domain::ToolAttemptId::from_uuid(attempt),
                signalbox_domain::ToolRequestId::from_uuid(request),
                ModelCallId::from_uuid(producing_call),
            )
        })
        .collect())
}

/// Restores the frontier an availability predecessor was prepared against.
///
/// A successor attempt owns no call yet, and its predecessor is terminal, so
/// the live call set omits it and reconstitution would fall back to the turn's
/// starting snapshot. A `switch_now` after a tool continuation would then
/// prepare the replacement without the assistant tool use or its results, and a
/// predecessor that consumed steering would reconstitute without the durable
/// consumed-steering rows the frontier holds.
///
/// A predecessor prepared against the turn's own starting frontier adds
/// nothing, so that case keeps the ordinary starting-snapshot path.
async fn load_availability_predecessor_snapshot(
    connection: &mut PgConnection,
    session: SessionId,
    attempt: TurnAttemptId,
    starting_frontier: signalbox_domain::ContextFrontierId,
) -> Result<Option<ResolvedContextFrontierReconstitutionInput>, ModelCallRepositoryError> {
    let frontier: Option<Uuid> = sqlx::query_scalar(
        "SELECT predecessor.context_frontier_id
           FROM credential_pool_availability_successor AS successor
           JOIN model_call AS predecessor
             ON predecessor.model_call_id = successor.predecessor_model_call_id
          WHERE successor.successor_turn_attempt_id = $1",
    )
    .bind(attempt.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(frontier) = frontier.map(signalbox_domain::ContextFrontierId::from_uuid) else {
        return Ok(None);
    };
    if frontier == starting_frontier {
        return Ok(None);
    }
    load_call_snapshot(connection, session, frontier)
        .await
        .map(Some)
}

pub(crate) async fn load_call_snapshot(
    connection: &mut PgConnection,
    session: SessionId,
    frontier: signalbox_domain::ContextFrontierId,
) -> Result<ResolvedContextFrontierReconstitutionInput, ModelCallRepositoryError> {
    let declared_count = sqlx::query_scalar::<_, Decimal>(
        "SELECT member_count
           FROM context_frontier
          WHERE owning_session_id = $1
            AND context_frontier_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ModelCallCorruption::Missing("model-call snapshot"))?;
    let rows = sqlx::query_as::<_, (Decimal, Uuid, Uuid)>(
        "SELECT member_position, source_session_id, semantic_entry_id
           FROM resolve_context_frontier_members($1, $2)
          ORDER BY member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    let actual_count = u64::try_from(rows.len())
        .map_err(|_| ModelCallCorruption::Inconsistent("model-call snapshot member count"))?;
    if declared_count != Decimal::from(actual_count) {
        return Err(ModelCallCorruption::Inconsistent("model-call snapshot member count").into());
    }
    let ordered_entries = rows
        .into_iter()
        .enumerate()
        .map(|(index, (position, source_session, semantic_entry))| {
            let expected_position = u64::try_from(index + 1).map_err(|_| {
                ModelCallCorruption::Inconsistent("model-call snapshot member positions")
            })?;
            if position != Decimal::from(expected_position) {
                return Err(ModelCallCorruption::Inconsistent(
                    "model-call snapshot member positions",
                )
                .into());
            }
            Ok(SemanticTranscriptEntryRef::from_source(
                session_id_from_uuid(source_session),
                signalbox_domain::SemanticTranscriptEntryId::from_uuid(semantic_entry),
            ))
        })
        .collect::<Result<Vec<_>, ModelCallRepositoryError>>()?;
    Ok(ResolvedContextFrontierReconstitutionInput::new(
        session,
        frontier,
        ordered_entries,
    ))
}

enum StoredAcceptedInputProvenance {
    Command(DurableCommandId),
    Goal(UserContent),
}

async fn load_origin_contents(
    connection: &mut PgConnection,
    entries: &[SemanticTranscriptEntry],
    pending_steering: &[PendingSteeringInput],
    consumed_steering: &[signalbox_domain::ConsumedSteeringInput],
) -> Result<Vec<ModelCallOriginContent>, ModelCallRepositoryError> {
    let pending_by_accepted = pending_steering
        .iter()
        .map(|pending| (pending.accepted_input(), pending))
        .collect::<BTreeMap<_, _>>();
    let consumed_by_accepted = consumed_steering
        .iter()
        .map(|consumed| (consumed.accepted_input(), consumed))
        .collect::<BTreeMap<_, _>>();
    let accepted_inputs = entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
            | SemanticTranscriptEntryPayload::SteeringAcceptedInput { accepted_input, .. } => {
                Some(*accepted_input)
            }
            SemanticTranscriptEntryPayload::TurnFailed { .. }
            | SemanticTranscriptEntryPayload::DelegatedTask { .. }
            | SemanticTranscriptEntryPayload::DelegationMessage { .. }
            | SemanticTranscriptEntryPayload::DelegationResult { .. }
            | SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
            | SemanticTranscriptEntryPayload::ContextSummary { .. }
            | SemanticTranscriptEntryPayload::TurnCancelled { .. }
            | SemanticTranscriptEntryPayload::AssistantText { .. }
            | SemanticTranscriptEntryPayload::ProviderCompaction { .. }
            | SemanticTranscriptEntryPayload::AssistantToolUse { .. }
            | SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
            | SemanticTranscriptEntryPayload::ToolDenied { .. }
            | SemanticTranscriptEntryPayload::ToolClosed { .. }
            | SemanticTranscriptEntryPayload::TurnCompleted { .. }
            | SemanticTranscriptEntryPayload::Imported { .. } => None,
        })
        .chain(pending_by_accepted.keys().copied())
        .chain(consumed_by_accepted.keys().copied())
        .collect::<BTreeSet<_>>();
    if accepted_inputs.is_empty() {
        return Ok(Vec::new());
    }
    let accepted_input_uuids = accepted_inputs
        .iter()
        .map(|accepted_input| accepted_input.into_uuid())
        .collect::<Vec<_>>();
    let rows = sqlx::query(
        "SELECT accepted.accepted_input_id, accepted.accepting_command_id,
                accepted_input_content_parts_json(accepted.accepted_input_id)
                    AS content_parts,
                goal.turn_id AS goal_turn_id
           FROM accepted_input AS accepted
           LEFT JOIN goal_turn AS goal
             ON goal.accepted_input_id = accepted.accepted_input_id
          WHERE accepted.accepted_input_id = ANY($1)
          ORDER BY accepted.accepted_input_id",
    )
    .bind(&accepted_input_uuids)
    .fetch_all(&mut *connection)
    .await?;
    if rows.len() != accepted_input_uuids.len() {
        return Err(ModelCallCorruption::Missing("accepted input receipt").into());
    }
    let mut loaded = BTreeSet::new();
    let mut command_by_accepted = BTreeMap::new();
    let mut goal_content_by_accepted = BTreeMap::new();
    let mut steering_content_by_accepted = BTreeMap::new();
    for row in rows {
        let accepted: Uuid = required(&row, "accepted_input_id")?;
        if !accepted_input_uuids.contains(&accepted) || !loaded.insert(accepted) {
            return Err(ModelCallCorruption::Inconsistent("accepted receipt inventory").into());
        }
        let accepted = AcceptedInputId::from_uuid(accepted);
        if pending_by_accepted.contains_key(&accepted)
            || consumed_by_accepted.contains_key(&accepted)
        {
            let content = crate::user_content::decode(required(&row, "content_parts")?)
                .map_err(|_| ModelCallCorruption::Inconsistent("steering content"))?;
            if steering_content_by_accepted
                .insert(accepted, content)
                .is_some()
            {
                return Err(ModelCallCorruption::Inconsistent("accepted receipt inventory").into());
            }
            continue;
        }
        let command: Option<Uuid> = row.try_get("accepting_command_id")?;
        let goal_turn: Option<Uuid> = row.try_get("goal_turn_id")?;
        // An accepting command decides provenance whether or not a generation
        // owns the turn. A goal turn bound to a turn a command already accepted
        // — the shape repository-watch dispatch commits — has both, and its
        // text was authored by that command; the `goal_turn` row records which
        // generation the turn runs under, not where its input came from.
        let provenance = match (command, goal_turn) {
            (Some(command), _) => {
                let command = durable_command_id_from_uuid(command)
                    .map_err(|_| ModelCallCorruption::Inconsistent("accepting command identity"))?;
                StoredAcceptedInputProvenance::Command(command)
            }
            (None, Some(_)) => {
                let content = crate::user_content::decode(required(&row, "content_parts")?)
                    .map_err(|_| ModelCallCorruption::Inconsistent("goal input content"))?;
                StoredAcceptedInputProvenance::Goal(content)
            }
            (None, None) => {
                return Err(ModelCallCorruption::Inconsistent("accepted input provenance").into());
            }
        };
        match provenance {
            StoredAcceptedInputProvenance::Command(command) => {
                if command_by_accepted.insert(accepted, command).is_some() {
                    return Err(
                        ModelCallCorruption::Inconsistent("accepted receipt inventory").into(),
                    );
                }
            }
            StoredAcceptedInputProvenance::Goal(content) => {
                if goal_content_by_accepted.insert(accepted, content).is_some() {
                    return Err(
                        ModelCallCorruption::Inconsistent("accepted receipt inventory").into(),
                    );
                }
            }
        }
    }
    let commands = command_by_accepted.values().copied().collect::<Vec<_>>();
    let recorded = require_recorded_batch(connection, &commands)
        .await
        .map_err(map_scheduling_error)?;
    accepted_inputs
        .into_iter()
        .map(|accepted| {
            let content = match steering_content_by_accepted.remove(&accepted) {
                Some(content) if pending_by_accepted.contains_key(&accepted) => {
                    ModelCallOriginContent::from_pending_steering(
                        pending_by_accepted
                            .get(&accepted)
                            .ok_or(ModelCallCorruption::Missing("pending steering correlation"))?,
                        content,
                    )
                }
                Some(content) => ModelCallOriginContent::from_consumed_steering(
                    consumed_by_accepted
                        .get(&accepted)
                        .ok_or(ModelCallCorruption::Missing(
                            "consumed steering correlation",
                        ))?,
                    content,
                ),
                None => match goal_content_by_accepted.remove(&accepted) {
                    Some(content) => ModelCallOriginContent::from_goal_turn(accepted, content),
                    None => {
                        let command = command_by_accepted
                            .get(&accepted)
                            .ok_or(ModelCallCorruption::Missing("accepted command correlation"))?;
                        let submit = recorded
                            .get(command)
                            .ok_or(ModelCallCorruption::Missing("accepted submit command"))?;
                        ModelCallOriginContent::from_recorded_submit(submit)
                            .ok_or(ModelCallCorruption::Inconsistent("accepted input content"))?
                    }
                },
            };
            if content.accepted_input() != accepted {
                return Err(ModelCallCorruption::Inconsistent("accepted content identity").into());
            }
            Ok(content)
        })
        .collect()
}

async fn load_attachment_blob_facts(
    connection: &mut PgConnection,
    origin_contents: &[ModelCallOriginContent],
) -> Result<Vec<AttachmentBlobFact>, ModelCallRepositoryError> {
    let digests = origin_contents
        .iter()
        .flat_map(|origin| origin.content().parts())
        .filter_map(|part| match part {
            signalbox_domain::UserContentPart::Attachment { digest, .. } => Some(*digest),
            signalbox_domain::UserContentPart::Text { .. } => None,
        })
        .collect::<BTreeSet<_>>();
    if digests.is_empty() {
        return Ok(Vec::new());
    }
    let encoded = digests
        .iter()
        .map(|digest| digest.as_bytes().to_vec())
        .collect::<Vec<_>>();
    let rows = sqlx::query(
        "SELECT digest, byte_length
           FROM blob
          WHERE digest = ANY($1::bytea[])
          ORDER BY digest",
    )
    .bind(&encoded)
    .fetch_all(&mut *connection)
    .await?;
    if rows.len() != digests.len() {
        return Err(ModelCallCorruption::Missing("attachment blob catalog fact").into());
    }
    rows.into_iter()
        .map(|row| {
            let bytes: Vec<u8> = row.try_get("digest")?;
            let digest = BlobDigest::from_bytes(bytes.try_into().map_err(|_| {
                ModelCallCorruption::Inconsistent("attachment blob catalog digest")
            })?);
            if !digests.contains(&digest) {
                return Err(
                    ModelCallCorruption::Inconsistent("attachment blob catalog inventory").into(),
                );
            }
            let length = positive_u64_from_numeric(row.try_get("byte_length")?).map_err(|_| {
                ModelCallCorruption::Inconsistent("attachment blob catalog byte length")
            })?;
            let length = NonZeroU64::new(length).ok_or(ModelCallCorruption::Inconsistent(
                "attachment blob catalog byte length",
            ))?;
            Ok(AttachmentBlobFact::new(digest, length))
        })
        .collect()
}

async fn load_live_turn_calls(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<
    (
        Option<PinnedProviderTargetReconstitutionInput>,
        Vec<ModelCallReconstitutionInput>,
    ),
    ModelCallRepositoryError,
> {
    let lifecycle = sqlx::query(
        "SELECT pinned_provider_model_identity_id, recovery_model_call_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ModelCallCorruption::Missing("live turn lifecycle"))?;
    let pinned_identity: Option<Uuid> = lifecycle.try_get("pinned_provider_model_identity_id")?;
    let pinned_target = pinned_identity.map(|identity| {
        PinnedProviderTargetReconstitutionInput::new(
            turn,
            ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(identity)),
        )
    });
    let recovery_call: Option<Uuid> = lifecycle.try_get("recovery_model_call_id")?;
    let rows = sqlx::query(
        "SELECT call.model_call_id, call.turn_id, call.turn_attempt_id,
                call.selection_kind, call.direct_model_selection_id,
                call.frozen_model_alias_id, call.frozen_alias_selected_direct_id,
                call.resolved_provider_model_identity_id, call.context_frontier_id,
                call.state_kind, call.terminal_disposition_kind,
                manifest.turn_instruction_manifest_id,
                manifest.boundary_kind AS instruction_manifest_boundary_kind,
                manifest.eligibility_hash_algorithm
                    AS instruction_eligibility_hash_algorithm,
                manifest.eligibility_hash AS instruction_eligibility_hash,
                manifest.admitted_set_hash_algorithm
                    AS instruction_admitted_set_hash_algorithm,
                manifest.admitted_set_hash AS instruction_admitted_set_hash,
                manifest.manifest_hash_algorithm
                    AS instruction_manifest_hash_algorithm,
                manifest.manifest_hash AS instruction_manifest_hash,
                discovery.scan_complete AS instruction_discovery_complete
           FROM model_call AS call
      LEFT JOIN turn_instruction_manifest AS manifest
             ON manifest.turn_instruction_manifest_id = call.turn_instruction_manifest_id
            AND manifest.session_id = call.session_id
            AND manifest.turn_id = call.turn_id
      LEFT JOIN instruction_discovery AS discovery
             ON discovery.instruction_discovery_id = manifest.instruction_discovery_id
         WHERE call.session_id = $1
            AND call.turn_id = $2
            AND (
                call.state_kind <> 'terminal'
                OR call.model_call_id = $3
            )
          ORDER BY call.model_call_id",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(recovery_call)
    .fetch_all(&mut *connection)
    .await?;
    Ok((
        pinned_target,
        rows.into_iter()
            .map(|row| decode_model_call(row, session))
            .collect::<Result<_, _>>()?,
    ))
}

fn decode_model_call(
    row: PgRow,
    session: SessionId,
) -> Result<ModelCallReconstitutionInput, ModelCallRepositoryError> {
    let turn = TurnId::from_uuid(required(&row, "turn_id")?);
    authenticate_model_call_instruction_manifest(&row, session, turn)?;
    let state_kind: String = required(&row, "state_kind")?;
    let terminal: Option<String> = row.try_get("terminal_disposition_kind")?;
    let state = match (state_kind.as_str(), terminal.as_deref()) {
        ("prepared", None) => ModelCallReconstitutionState::Prepared,
        ("in_flight", None) => ModelCallReconstitutionState::InFlight,
        ("cancellation_requested", None) => ModelCallReconstitutionState::CancellationRequested,
        ("terminal", Some(value)) => {
            ModelCallReconstitutionState::Terminal(decode_disposition(value)?)
        }
        ("prepared" | "in_flight" | "cancellation_requested" | "terminal", _) => {
            return Err(ModelCallCorruption::Inconsistent("model-call state payload").into());
        }
        (value, _) => {
            return Err(ModelCallCorruption::Unsupported {
                field: "model_call.state_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };
    Ok(ModelCallReconstitutionInput::new(
        ModelCallId::from_uuid(required(&row, "model_call_id")?),
        turn,
        signalbox_domain::TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?),
        decode_selection(
            required(&row, "selection_kind")?,
            row.try_get("direct_model_selection_id")?,
            row.try_get("frozen_model_alias_id")?,
            row.try_get("frozen_alias_selected_direct_id")?,
        )?,
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(required(
            &row,
            "resolved_provider_model_identity_id",
        )?)),
        signalbox_domain::ContextFrontierId::from_uuid(required(&row, "context_frontier_id")?),
        state,
    ))
}

pub(crate) fn authenticate_model_call_instruction_manifest(
    row: &PgRow,
    session: SessionId,
    turn: TurnId,
) -> Result<(), ModelCallRepositoryError> {
    let manifest_id =
        TurnInstructionManifestId::from_uuid(required(row, "turn_instruction_manifest_id")?);
    let boundary_kind: String = required(row, "instruction_manifest_boundary_kind")?;
    if boundary_kind != "turn_start" {
        return Err(ModelCallCorruption::Inconsistent("turn instruction manifest boundary").into());
    }
    if !required::<bool>(row, "instruction_discovery_complete")? {
        return Err(ModelCallCorruption::Inconsistent("instruction discovery completeness").into());
    }
    if required::<String>(row, "instruction_eligibility_hash_algorithm")? != "sha256_v1"
        || required::<String>(row, "instruction_admitted_set_hash_algorithm")? != "sha256_v1"
        || required::<String>(row, "instruction_manifest_hash_algorithm")? != "sha256_v1"
    {
        return Err(
            ModelCallCorruption::Inconsistent("turn instruction manifest hash algorithm").into(),
        );
    }
    let eligibility_hash: Vec<u8> = required(row, "instruction_eligibility_hash")?;
    let admitted_set_hash: Vec<u8> = required(row, "instruction_admitted_set_hash")?;
    let manifest_hash: Vec<u8> = required(row, "instruction_manifest_hash")?;
    let eligibility_hash: [u8; 32] = eligibility_hash
        .try_into()
        .map_err(|_| ModelCallCorruption::Inconsistent("instruction eligibility hash"))?;
    let admitted_set_hash: [u8; 32] = admitted_set_hash
        .try_into()
        .map_err(|_| ModelCallCorruption::Inconsistent("instruction admitted-set hash"))?;
    let manifest_hash: [u8; 32] = manifest_hash
        .try_into()
        .map_err(|_| ModelCallCorruption::Inconsistent("instruction manifest hash"))?;
    TurnInstructionManifest::reconstitute_empty_turn_start(
        manifest_id,
        session,
        turn,
        EmptyTurnInstructionManifestEvidence {
            eligibility_hash: InstructionDigest::from_sha256(eligibility_hash),
            admitted_set_hash: InstructionDigest::from_sha256(admitted_set_hash),
            manifest_hash: InstructionDigest::from_sha256(manifest_hash),
        },
    )
    .ok_or(ModelCallCorruption::Inconsistent(
        "turn instruction manifest authentication",
    ))?;
    Ok(())
}

fn decode_selection(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    alias_selected: Option<Uuid>,
) -> Result<FrozenModelSelection, ModelCallRepositoryError> {
    match (kind.as_str(), direct, alias, alias_selected) {
        ("direct", Some(direct), None, None) => Ok(FrozenModelSelection::Direct(
            DirectModelSelection::from_uuid(direct),
        )),
        ("frozen_alias", None, Some(alias), Some(selected)) => {
            Ok(FrozenModelSelection::FrozenAlias {
                alias: ModelAlias::from_uuid(alias),
                definition: FrozenAliasDefinition::selecting(DirectModelSelection::from_uuid(
                    selected,
                )),
            })
        }
        ("direct" | "frozen_alias", _, _, _) => {
            Err(ModelCallCorruption::Inconsistent("frozen selection payload").into())
        }
        (value, _, _, _) => Err(ModelCallCorruption::Unsupported {
            field: "model_call.selection_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn decode_disposition(value: &str) -> Result<ModelCallDisposition, ModelCallRepositoryError> {
    match value {
        "completed" => Ok(ModelCallDisposition::Completed),
        "known_failed" => Ok(ModelCallDisposition::KnownFailed),
        "refused" => Ok(ModelCallDisposition::Refused),
        "cancelled" => Ok(ModelCallDisposition::Cancelled),
        "ambiguous" => Ok(ModelCallDisposition::Ambiguous),
        value => Err(ModelCallCorruption::Unsupported {
            field: "model_call.terminal_disposition_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn require_exact_call(
    execution: ModelCallExecution,
    call: ModelCallId,
) -> Result<ModelCallExecution, ModelCallRepositoryError> {
    if matches!(execution.current_call(), Some(current) if current.id() == call) {
        Ok(execution)
    } else {
        Err(ModelCallRepositoryError::InvalidTransition(
            "fresh execution does not contain the expected call",
        ))
    }
}

/// Resolves the target whose credential pool governs this call.
///
/// Fast mode can route a selectable model to an alternate serving target with a
/// different credential family and a different pool. Looking the pool up under
/// the base target would replace the correctly resolved serving credential with
/// a member of an unrelated pool and freeze that unrelated failover policy.
fn serving_pool_target(
    families: Option<&crate::ModelCredentialFamilyCatalog>,
    selected: ResolvedProviderTarget,
    fast_mode: FastMode,
) -> ResolvedProviderTarget {
    families.map_or(selected, |families| {
        families.serving_target_for_call(selected, fast_mode)
    })
}

struct SelectedRuntimePoolCredential {
    reference: Option<ModelCallCredentialReference>,
    policy: Option<CredentialPoolRuntimePolicy>,
    /// Uncommitted `switch_next_turn` rows this selection would satisfy.
    ///
    /// Selection cannot consume them itself: preparation can still fail after
    /// selection succeeds, and a member that never carried a call must leave
    /// its displacement durable for the next turn.
    pending_consumed_actions: Vec<i64>,
}

/// Marks the displacement rows a prepared call has now satisfied.
async fn consume_pool_member_actions(
    connection: &mut PgConnection,
    turn: TurnId,
    actions: &[i64],
) -> Result<(), ModelCallRepositoryError> {
    if actions.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE credential_pool_member_action
            SET consumed_turn_id = $1
          WHERE action_id = ANY($2)
            AND consumed_turn_id IS NULL",
    )
    .bind(turn_id_to_uuid(turn))
    .bind(actions)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// Reports the remaining successor delay when this call already substituted.
///
/// The successor row is written in the same transaction that terminalizes its
/// predecessor, so its presence proves the commit landed. The delay is
/// recovered from the successor attempt's own durable deadline; an elapsed
/// deadline yields zero rather than absence.
async fn committed_availability_successor_backoff(
    connection: &mut PgConnection,
    predecessor: ModelCallId,
) -> Result<Option<Duration>, ModelCallRepositoryError> {
    let successor: Option<Uuid> = sqlx::query_scalar(
        "SELECT successor_turn_attempt_id
           FROM credential_pool_availability_successor
          WHERE predecessor_model_call_id = $1",
    )
    .bind(predecessor.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(successor) = successor else {
        return Ok(None);
    };
    let remaining =
        load_availability_successor_backoff(connection, TurnAttemptId::from_uuid(successor))
            .await?;
    Ok(Some(remaining.unwrap_or(Duration::ZERO)))
}

async fn load_availability_successor_backoff(
    connection: &mut PgConnection,
    attempt: TurnAttemptId,
) -> Result<Option<Duration>, ModelCallRepositoryError> {
    let remaining: Option<i64> = sqlx::query_scalar(
        "SELECT GREATEST(
                    0,
                    CEIL(EXTRACT(EPOCH FROM (retry_not_before - clock_timestamp())) * 1000)
                )::bigint
           FROM credential_pool_availability_successor
          WHERE successor_turn_attempt_id = $1
            AND retry_not_before > clock_timestamp()",
    )
    .bind(attempt.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    remaining
        .map(|milliseconds| {
            u64::try_from(milliseconds)
                .map(Duration::from_millis)
                .map_err(|_| {
                    ModelCallCorruption::Inconsistent("availability successor backoff").into()
                })
        })
        .transpose()
}

/// Every member one pool currently excludes, with the rows a call would satisfy.
struct DurablePoolExclusions {
    excluded: HashSet<String>,
    pending_consumed_actions: Vec<i64>,
}

/// Serializes action-head reads and writes for one credential profile.
///
/// Quarantine and membership exclusion are global to a profile rather than to a
/// pool, so the profile reference alone is the lock key. Callers needing
/// several profiles take them in sorted order, so two sessions preparing calls
/// over the same pool cannot deadlock against each other.
async fn lock_credential_pool_action_head(
    connection: &mut PgConnection,
    credential_reference: &str,
) -> Result<(), ModelCallRepositoryError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "credential_pool_action_head:{credential_reference}"
        ))
        .execute(&mut *connection)
        .await?;
    Ok(())
}

/// Serializes model-call transactions before either credential or outbox locks.
pub(crate) async fn acquire_model_call_outbox_order_guard(
    connection: &mut PgConnection,
) -> Result<ModelCallOutboxOrderGuard, ModelCallRepositoryError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(MODEL_CALL_OUTBOX_ORDER_GUARD)
        .execute(&mut *connection)
        .await?;
    Ok(ModelCallOutboxOrderGuard { _private: () })
}

/// Takes every action-head lock for one pool in deterministic profile order.
async fn lock_credential_pool_action_heads(
    connection: &mut PgConnection,
    policy: &CredentialPoolRuntimePolicy,
) -> Result<(), ModelCallRepositoryError> {
    let members = credential_pool_member_references(policy);
    let mut locked = members.iter().copied().collect::<Vec<_>>();
    locked.sort_unstable();
    for reference in locked {
        lock_credential_pool_action_head(connection, reference).await?;
    }
    Ok(())
}

fn credential_pool_member_references(policy: &CredentialPoolRuntimePolicy) -> HashSet<&str> {
    policy
        .members()
        .iter()
        .map(CredentialPoolRuntimeMember::credential_reference)
        .collect()
}

/// Reads the durable exclusions governing one pool under its members' locks.
///
/// Selection and the availability-successor test must apply exactly the same
/// predicate. Reading only same-turn chain exclusions let the observation
/// commit create a successor no member could serve, and reading action rows
/// without the profile locks let a concurrent quarantine commit between the
/// read and the dispatch it was supposed to prevent.
async fn load_durable_pool_exclusions(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    policy: &CredentialPoolRuntimePolicy,
) -> Result<DurablePoolExclusions, ModelCallRepositoryError> {
    let members = credential_pool_member_references(policy);
    lock_credential_pool_action_heads(connection, policy).await?;
    let mut excluded = sqlx::query_scalar::<_, String>(
        "SELECT credential_reference
           FROM credential_pool_chain_exclusion
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_all(&mut *connection)
    .await?
    .into_iter()
    .collect::<HashSet<_>>();
    let completed_references = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT call.credential_reference
           FROM model_call AS call
           JOIN model_call_credential_pool_policy AS call_policy
             ON call_policy.model_call_id = call.model_call_id
          WHERE call.session_id = $1
            AND call_policy.pool_name = $2
            AND call.state_kind = 'terminal'
            AND call.terminal_disposition_kind = 'completed'",
    )
    .bind(session_id_to_uuid(session))
    .bind(policy.name())
    .fetch_all(&mut *connection)
    .await?
    .into_iter()
    .collect::<HashSet<_>>();
    let actions = sqlx::query_as::<_, (i64, String, String, Uuid, Uuid)>(
        "SELECT action_id, credential_reference, action_kind,
                observed_session_id, observed_turn_id
           FROM credential_pool_member_action
          WHERE consumed_turn_id IS NULL
            AND (pool_name = $1 OR action_kind = 'quarantine')",
    )
    .bind(policy.name())
    .fetch_all(&mut *connection)
    .await?;
    let mut pending_consumed_actions = Vec::new();
    for (action_id, reference, action_kind, observed_session, observed_turn) in actions {
        let applies = match action_kind.as_str() {
            "quarantine" => true,
            "avoid_new_sessions" => !completed_references.contains(&reference),
            "switch_next_turn" => {
                observed_session == session_id_to_uuid(session)
                    && observed_turn != turn_id_to_uuid(turn)
            }
            _ => {
                return Err(ModelCallCorruption::Unsupported {
                    field: "credential_pool_member_action action_kind",
                    value: action_kind,
                }
                .into());
            }
        };
        // A global quarantine can name a profile this pool never ranked.
        // Selection would ignore it, but the successor backoff is derived from
        // the size of this set, so an unrelated quarantine elsewhere must not
        // push the first rotation onto a later exponential tier.
        if applies && members.contains(reference.as_str()) {
            excluded.insert(reference);
            if action_kind == "switch_next_turn" {
                pending_consumed_actions.push(action_id);
            }
        }
    }
    Ok(DurablePoolExclusions {
        excluded,
        pending_consumed_actions,
    })
}

/// Returns the member this session most recently prepared a call on.
///
/// Selection is sticky across turns: once a displacement moves a session off
/// its preferred member, the following turn must stay on the replacement while
/// it remains admissible instead of returning to a member whose immediate
/// exclusion merely expired with the turn.
async fn load_session_sticky_pool_member(
    connection: &mut PgConnection,
    session: SessionId,
    pool_name: &str,
) -> Result<Option<String>, ModelCallRepositoryError> {
    sqlx::query_scalar::<_, String>(
        "SELECT call.credential_reference
           FROM model_call AS call
           JOIN model_call_credential_pool_policy AS call_policy
             ON call_policy.model_call_id = call.model_call_id
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.turn_id = call.turn_id
            AND lifecycle.session_id = call.session_id
          WHERE call.session_id = $1
            AND call_policy.pool_name = $2
          ORDER BY lifecycle.acceptance_position DESC,
                   EXISTS (
                       SELECT 1
                         FROM credential_pool_availability_successor AS successor
                        WHERE successor.predecessor_model_call_id = call.model_call_id
                   ) ASC
          LIMIT 1",
    )
    .bind(session_id_to_uuid(session))
    .bind(pool_name)
    .fetch_optional(&mut *connection)
    .await
    .map_err(Into::into)
}

async fn select_runtime_pool_credential(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    attempt: TurnAttemptId,
    target: ResolvedProviderTarget,
    default_reference: ModelCallCredentialReference,
    policies: &CredentialPoolRuntimeCatalog,
) -> Result<SelectedRuntimePoolCredential, ModelCallRepositoryError> {
    let predecessor: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT successor.predecessor_model_call_id,
                EXISTS (
                    SELECT 1
                      FROM credential_pool_chain_exclusion AS exclusion
                     WHERE exclusion.predecessor_model_call_id =
                           successor.predecessor_model_call_id
                ) AS rotated
           FROM credential_pool_availability_successor AS successor
          WHERE successor.successor_turn_attempt_id = $1",
    )
    .bind(attempt.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let (policy, predecessor_reference, predecessor_rotated) = match predecessor {
        Some((predecessor, rotated)) => {
            let policy = load_call_pool_policy(connection, predecessor)
                .await?
                .ok_or(ModelCallCorruption::Missing(
                    "availability successor predecessor pool policy",
                ))?;
            let reference: String = sqlx::query_scalar(
                "SELECT credential_reference
                   FROM model_call
                  WHERE model_call_id = $1",
            )
            .bind(predecessor)
            .fetch_one(&mut *connection)
            .await?;
            (Some(policy), Some(reference), rotated)
        }
        None => (policies.get(&target).cloned(), None, false),
    };
    let Some(policy) = policy else {
        return Ok(SelectedRuntimePoolCredential {
            reference: Some(default_reference),
            policy: None,
            pending_consumed_actions: Vec::new(),
        });
    };
    let DurablePoolExclusions {
        excluded,
        pending_consumed_actions: next_turn_actions,
    } = load_durable_pool_exclusions(connection, session, turn, &policy).await?;
    let sticky_reference = match predecessor_reference {
        // An availability successor continues its predecessor's chain, so the
        // chain position rather than session stickiness governs it.
        Some(_) => None,
        None => load_session_sticky_pool_member(connection, session, policy.name()).await?,
    };
    let start = predecessor_reference
        .as_deref()
        .and_then(|reference| {
            policy
                .members()
                .iter()
                .position(|member| member.credential_reference() == reference)
        })
        .map_or(0, |position| position.saturating_add(1));
    let selected = predecessor_reference
        .as_deref()
        .filter(|reference| !excluded.contains(*reference))
        .and_then(|reference| {
            policy
                .members()
                .iter()
                .find(|member| member.credential_reference() == reference)
        })
        .or_else(|| {
            if predecessor_reference.is_some() && !predecessor_rotated {
                return None;
            }
            policy
                .members()
                .iter()
                .find(|member| {
                    sticky_reference.as_deref() == Some(member.credential_reference())
                        && !excluded.contains(member.credential_reference())
                })
                .or_else(|| {
                    policy
                        .members()
                        .iter()
                        .skip(start)
                        .chain(policy.members().iter().take(start))
                        .find(|member| !excluded.contains(member.credential_reference()))
                })
        })
        .map(|member| ModelCallCredentialReference::new(member.credential_reference()));
    let pending_consumed_actions = if selected.is_some() {
        next_turn_actions
    } else {
        Vec::new()
    };
    Ok(SelectedRuntimePoolCredential {
        reference: selected,
        policy: Some(policy),
        pending_consumed_actions,
    })
}

async fn persist_call_pool_policy(
    connection: &mut PgConnection,
    call: ModelCallId,
    policy: &CredentialPoolRuntimePolicy,
) -> Result<(), ModelCallRepositoryError> {
    sqlx::query(
        "INSERT INTO model_call_credential_pool_policy
            (model_call_id, pool_name, on_pool_exhausted,
             on_quota_exhausted, on_rate_limited, on_overloaded,
             on_credential_rejected)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(call.into_uuid())
    .bind(policy.name())
    .bind(policy.on_pool_exhausted.as_str())
    .bind(policy.quota_exhausted.as_str())
    .bind(policy.rate_limited.as_str())
    .bind(policy.overloaded.as_str())
    .bind(policy.credential_rejected.as_str())
    .execute(&mut *connection)
    .await?;
    for (ordinal, member) in policy.members().iter().enumerate() {
        let ordinal = i32::try_from(ordinal).map_err(|_| {
            ModelCallRepositoryError::InvalidTransition("credential pool ordinal overflow")
        })?;
        sqlx::query(
            "INSERT INTO model_call_credential_pool_member
                (model_call_id, member_ordinal, credential_reference, priority)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(call.into_uuid())
        .bind(ordinal)
        .bind(member.credential_reference())
        .bind(i64::from(member.priority().get()))
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

/// Records one durable exclusion under the profile's action-head lock.
async fn persist_credential_pool_member_action(
    connection: &mut PgConnection,
    policy: &CredentialPoolRuntimePolicy,
    action: CredentialPoolRuntimeAction,
    credential_reference: String,
    observation: &CorrelatedModelCallTerminalObservation,
    cause: ProviderModelCallFailureCause,
) -> Result<(), ModelCallRepositoryError> {
    if action == CredentialPoolRuntimeAction::Stay
        || action == CredentialPoolRuntimeAction::SwitchNow
    {
        return Err(ModelCallRepositoryError::InvalidTransition(
            "non-durable pool action reached durable action persistence",
        ));
    }
    lock_credential_pool_action_head(connection, &credential_reference).await?;
    sqlx::query(
        "INSERT INTO credential_pool_member_action
            (pool_name, credential_reference, action_kind,
             observed_session_id, observed_turn_id,
             observation_model_call_id, cause_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(policy.name())
    .bind(credential_reference)
    .bind(action.as_str())
    .bind(session_id_to_uuid(observation.correlation().session()))
    .bind(turn_id_to_uuid(observation.correlation().turn()))
    .bind(observation.call().into_uuid())
    .bind(encode_provider_failure_cause(cause))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// Loads the policy frozen onto one call, or `None` when it carried no pool.
///
/// Deployments without credential pools prepare calls with no policy row at
/// all, so absence is an ordinary shape rather than durable corruption.
async fn load_call_pool_policy(
    connection: &mut PgConnection,
    call: Uuid,
) -> Result<Option<CredentialPoolRuntimePolicy>, ModelCallRepositoryError> {
    let Some(row) = sqlx::query(
        "SELECT pool_name, on_pool_exhausted,
                on_quota_exhausted, on_rate_limited, on_overloaded,
                on_credential_rejected
           FROM model_call_credential_pool_policy
          WHERE model_call_id = $1",
    )
    .bind(call)
    .fetch_optional(&mut *connection)
    .await?
    else {
        return Ok(None);
    };
    let members = sqlx::query_as::<_, (String, i64)>(
        "SELECT credential_reference, priority
           FROM model_call_credential_pool_member
          WHERE model_call_id = $1
          ORDER BY member_ordinal",
    )
    .bind(call)
    .fetch_all(&mut *connection)
    .await?
    .into_iter()
    .map(|(reference, priority)| {
        let priority = u32::try_from(priority)
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or(ModelCallCorruption::Inconsistent(
                "credential pool member priority",
            ))?;
        Ok(CredentialPoolRuntimeMember::new(reference, priority))
    })
    .collect::<Result<Vec<_>, ModelCallRepositoryError>>()?;
    Ok(Some(CredentialPoolRuntimePolicy::new(
        row.try_get::<String, _>("pool_name")?,
        Arc::<[CredentialPoolRuntimeMember]>::from(members),
        CredentialPoolRuntimeExhaustion::parse(row.try_get("on_pool_exhausted")?)?,
        CredentialPoolRuntimeAction::parse(row.try_get("on_quota_exhausted")?)?,
        CredentialPoolRuntimeAction::parse(row.try_get("on_rate_limited")?)?,
        CredentialPoolRuntimeAction::parse(row.try_get("on_overloaded")?)?,
        CredentialPoolRuntimeAction::parse(row.try_get("on_credential_rejected")?)?,
    )))
}

pub(crate) async fn insert_prepared_call(
    connection: &mut PgConnection,
    prepared: &signalbox_domain::PreparedInitialModelCall,
    credential_reference: &ModelCallCredentialReference,
    credential_pool_policy: Option<&CredentialPoolRuntimePolicy>,
    input_includes_cache_tokens: bool,
) -> Result<(), ModelCallRepositoryError> {
    crate::convergence_sweep::lock_model_activity_fence(connection, prepared.session()).await?;
    let call = prepared.call();
    let (kind, direct, alias, alias_selected) = encode_selection(call.selection());
    for steering in prepared.consumed_steering() {
        let SemanticTranscriptEntryPayload::SteeringAcceptedInput {
            accepted_input,
            source_turn,
        } = steering.semantic_entry().payload()
        else {
            return Err(ModelCallCorruption::Inconsistent("steering semantic payload").into());
        };
        if *source_turn != prepared.turn()
            || *accepted_input != steering.accepted_input().id()
            || !matches!(
                steering.accepted_input().disposition(),
                AcceptedInputDisposition::ConsumedAsSteering {
                    call: consuming_call
                } if *consuming_call == call.id()
            )
        {
            return Err(
                ModelCallCorruption::Inconsistent("steering consumption correlation").into(),
            );
        }
        sqlx::query(
            "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 origin_accepted_input_id, steering_source_turn_id)
             VALUES ($1, $2, 'steering_accepted_input', $3, $4)",
        )
        .bind(session_id_to_uuid(
            steering.semantic_entry().source_session(),
        ))
        .bind(steering.semantic_entry().identity().into_uuid())
        .bind(accepted_input.into_uuid())
        .bind(turn_id_to_uuid(*source_turn))
        .execute(&mut *connection)
        .await?;
    }
    if let Some(snapshot) = prepared.steering_snapshot() {
        insert_snapshot(connection, snapshot).await?;
    }
    for steering in prepared.consumed_steering() {
        let command: Option<Option<Uuid>> = sqlx::query_scalar(
            "UPDATE accepted_input
                SET disposition_kind = 'consumed_as_steering',
                    consuming_model_call_id = $1
              WHERE accepted_input_id = $2
                AND session_id = $3
                AND disposition_kind = 'pending_steering'
                AND origin_turn_id IS NULL
                AND consuming_model_call_id IS NULL
                AND delivery_kind = 'next_safe_point'
                AND expected_active_turn_id = $4
            RETURNING accepting_command_id",
        )
        .bind(call.id().into_uuid())
        .bind(steering.accepted_input().id().into_uuid())
        .bind(session_id_to_uuid(prepared.session()))
        .bind(turn_id_to_uuid(prepared.turn()))
        .fetch_optional(&mut *connection)
        .await?;
        let command = command.ok_or(ModelCallCorruption::Inconsistent(
            "consumed steering accepted input",
        ))?;
        settle_injection(
            connection,
            prepared.session(),
            command,
            InjectionOutcomeOutbox::Delivered {
                turn: Some(prepared.turn()),
            },
        )
        .await?;
    }
    let pinned_rows = sqlx::query(
        "UPDATE turn_lifecycle
            SET pinned_provider_model_identity_id = $1
          WHERE turn_id = $2
            AND session_id = $3
            AND current_attempt_id = $4
            AND state_kind = 'active'
            AND active_phase_kind = 'running'
            AND (
                pinned_provider_model_identity_id IS NULL
                OR pinned_provider_model_identity_id = $1
            )",
    )
    .bind(call.target().identity().into_uuid())
    .bind(turn_id_to_uuid(prepared.turn()))
    .bind(session_id_to_uuid(prepared.session()))
    .bind(prepared.attempt().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(pinned_rows, "turn-level provider target pin")?;
    let instruction_manifest = sqlx::query(
        "SELECT m.turn_instruction_manifest_id,
                m.eligibility_hash_algorithm, m.eligibility_hash,
                m.admitted_set_hash_algorithm, m.admitted_set_hash,
                m.manifest_hash_algorithm, m.manifest_hash, d.scan_complete
           FROM turn_instruction_manifest AS m
           JOIN instruction_discovery AS d
             ON d.instruction_discovery_id = m.instruction_discovery_id
          WHERE m.session_id = $1
            AND m.turn_id = $2
            AND m.boundary_kind = 'turn_start'",
    )
    .bind(session_id_to_uuid(prepared.session()))
    .bind(turn_id_to_uuid(prepared.turn()))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ModelCallCorruption::Missing("turn instruction manifest"))?;
    if !instruction_manifest.try_get::<bool, _>("scan_complete")? {
        return Err(ModelCallCorruption::Inconsistent("instruction discovery completeness").into());
    }
    if instruction_manifest.try_get::<String, _>("eligibility_hash_algorithm")? != "sha256_v1"
        || instruction_manifest.try_get::<String, _>("admitted_set_hash_algorithm")? != "sha256_v1"
        || instruction_manifest.try_get::<String, _>("manifest_hash_algorithm")? != "sha256_v1"
    {
        return Err(
            ModelCallCorruption::Inconsistent("turn instruction manifest hash algorithm").into(),
        );
    }
    let instruction_manifest_id = TurnInstructionManifestId::from_uuid(
        instruction_manifest.try_get("turn_instruction_manifest_id")?,
    );
    let eligibility_hash: Vec<u8> = instruction_manifest.try_get("eligibility_hash")?;
    let admitted_set_hash: Vec<u8> = instruction_manifest.try_get("admitted_set_hash")?;
    let manifest_hash: Vec<u8> = instruction_manifest.try_get("manifest_hash")?;
    let eligibility_hash: [u8; 32] = eligibility_hash
        .try_into()
        .map_err(|_| ModelCallCorruption::Inconsistent("instruction eligibility hash"))?;
    let admitted_set_hash: [u8; 32] = admitted_set_hash
        .try_into()
        .map_err(|_| ModelCallCorruption::Inconsistent("instruction admitted-set hash"))?;
    let manifest_hash: [u8; 32] = manifest_hash
        .try_into()
        .map_err(|_| ModelCallCorruption::Inconsistent("instruction manifest hash"))?;
    TurnInstructionManifest::reconstitute_empty_turn_start(
        instruction_manifest_id,
        prepared.session(),
        prepared.turn(),
        EmptyTurnInstructionManifestEvidence {
            eligibility_hash: InstructionDigest::from_sha256(eligibility_hash),
            admitted_set_hash: InstructionDigest::from_sha256(admitted_set_hash),
            manifest_hash: InstructionDigest::from_sha256(manifest_hash),
        },
    )
    .ok_or(ModelCallCorruption::Inconsistent(
        "turn instruction manifest authentication",
    ))?;
    sqlx::query(
        "INSERT INTO model_call
            (model_call_id, turn_id, session_id, turn_attempt_id,
             selection_kind, direct_model_selection_id, frozen_model_alias_id,
             frozen_alias_selected_direct_id, resolved_provider_model_identity_id,
             context_frontier_id, credential_reference,
             usage_input_includes_cache_tokens, turn_instruction_manifest_id, state_kind,
             terminal_disposition_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, 'prepared', NULL)",
    )
    .bind(call.id().into_uuid())
    .bind(turn_id_to_uuid(prepared.turn()))
    .bind(session_id_to_uuid(prepared.session()))
    .bind(prepared.attempt().into_uuid())
    .bind(kind)
    .bind(direct)
    .bind(alias)
    .bind(alias_selected)
    .bind(call.target().identity().into_uuid())
    .bind(call.frontier().snapshot().into_uuid())
    .bind(credential_reference.as_str())
    .bind(input_includes_cache_tokens)
    .bind(instruction_manifest_id.into_uuid())
    .execute(&mut *connection)
    .await?;
    freeze_recorded_user_overrides(connection, prepared.session(), call.id()).await?;
    if let Some(policy) = credential_pool_policy {
        persist_call_pool_policy(connection, call.id(), policy).await?;
    }
    outbox::append(
        connection,
        OutboxEvent::ModelCallTransition {
            session: prepared.session(),
            turn: prepared.turn(),
            call: call.id(),
            state: ModelCallOutboxState::Prepared,
        },
    )
    .await?;
    Ok(())
}

async fn load_tool_conversation_entries(
    connection: &mut PgConnection,
    request: &PreparedModelCallRequest,
) -> Result<Box<[ResolvedToolConversationEntry]>, ModelCallRepositoryError> {
    let mut request_ids = BTreeSet::new();
    let mut attempt_ids = BTreeSet::new();
    let mut approval_ids = BTreeSet::new();
    for entry in request.frontier_entries() {
        match entry.payload() {
            SemanticTranscriptEntryPayload::AssistantToolUse { request, .. }
            | SemanticTranscriptEntryPayload::ToolClosed { request } => {
                request_ids.insert(*request);
            }
            SemanticTranscriptEntryPayload::ToolDenied { request } => {
                request_ids.insert(*request);
                approval_ids.insert(*request);
            }
            SemanticTranscriptEntryPayload::ToolExecutionResult { attempt } => {
                attempt_ids.insert(*attempt);
            }
            SemanticTranscriptEntryPayload::OriginAcceptedInput { .. }
            | SemanticTranscriptEntryPayload::DelegatedTask { .. }
            | SemanticTranscriptEntryPayload::DelegationMessage { .. }
            | SemanticTranscriptEntryPayload::DelegationResult { .. }
            | SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
            | SemanticTranscriptEntryPayload::ContextSummary { .. }
            | SemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
            | SemanticTranscriptEntryPayload::Imported { .. }
            | SemanticTranscriptEntryPayload::AssistantText { .. }
            | SemanticTranscriptEntryPayload::ProviderCompaction { .. }
            | SemanticTranscriptEntryPayload::TurnFailed { .. }
            | SemanticTranscriptEntryPayload::TurnCancelled { .. }
            | SemanticTranscriptEntryPayload::TurnCompleted { .. } => {}
        }
    }
    let attempts = crate::tool_loop::load_attempts_by_id(
        connection,
        &attempt_ids.iter().copied().collect::<Vec<_>>(),
    )
    .await
    .map_err(map_tool_evidence_error)?;
    for attempt in attempts.values() {
        let request = match attempt {
            signalbox_domain::ReconstitutedToolAttempt::Current(current) => current.request(),
            signalbox_domain::ReconstitutedToolAttempt::Ended(ended) => ended.request(),
        };
        request_ids.insert(request);
    }
    let requests = crate::tool_loop::load_requests_by_id(
        connection,
        &request_ids.iter().copied().collect::<Vec<_>>(),
    )
    .await
    .map_err(map_tool_evidence_error)?;
    let approvals = crate::tool_loop::load_approvals_by_request(
        connection,
        &approval_ids.iter().copied().collect::<Vec<_>>(),
    )
    .await
    .map_err(map_tool_evidence_error)?;

    let mut resolved = Vec::new();
    for entry in request.frontier_entries() {
        let source = entry.reference();
        match entry.payload() {
            SemanticTranscriptEntryPayload::AssistantToolUse {
                request: request_id,
                ..
            } => {
                let request = requests
                    .get(request_id)
                    .cloned()
                    .ok_or(ModelCallCorruption::Missing("tool request evidence"))?;
                resolved.push(ResolvedToolConversationEntry::AssistantToolUse { source, request });
            }
            SemanticTranscriptEntryPayload::ToolExecutionResult { attempt } => {
                let attempt = attempts
                    .get(attempt)
                    .cloned()
                    .ok_or(ModelCallCorruption::Missing("tool attempt evidence"))?;
                let signalbox_domain::ReconstitutedToolAttempt::Ended(attempt) = attempt else {
                    return Err(
                        ModelCallCorruption::Inconsistent("tool result attempt is live").into(),
                    );
                };
                let request = requests
                    .get(&attempt.request())
                    .cloned()
                    .ok_or(ModelCallCorruption::Missing("tool result request evidence"))?;
                resolved.push(ResolvedToolConversationEntry::ExecutionResult {
                    source,
                    request,
                    attempt,
                });
            }
            SemanticTranscriptEntryPayload::ToolDenied {
                request: request_id,
            } => {
                let request = requests
                    .get(request_id)
                    .cloned()
                    .ok_or(ModelCallCorruption::Missing("denied tool request evidence"))?;
                let approval = approvals
                    .get(request_id)
                    .cloned()
                    .ok_or(ModelCallCorruption::Missing("tool denial evidence"))?;
                resolved.push(ResolvedToolConversationEntry::Denied {
                    source,
                    request,
                    approval,
                });
            }
            SemanticTranscriptEntryPayload::ToolClosed {
                request: request_id,
            } => {
                let request = requests
                    .get(request_id)
                    .cloned()
                    .ok_or(ModelCallCorruption::Missing("closed tool request evidence"))?;
                resolved.push(ResolvedToolConversationEntry::Closed { source, request });
            }
            SemanticTranscriptEntryPayload::OriginAcceptedInput { .. }
            | SemanticTranscriptEntryPayload::DelegatedTask { .. }
            | SemanticTranscriptEntryPayload::DelegationMessage { .. }
            | SemanticTranscriptEntryPayload::DelegationResult { .. }
            | SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
            | SemanticTranscriptEntryPayload::ContextSummary { .. }
            | SemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
            | SemanticTranscriptEntryPayload::Imported { .. }
            | SemanticTranscriptEntryPayload::AssistantText { .. }
            | SemanticTranscriptEntryPayload::ProviderCompaction { .. }
            | SemanticTranscriptEntryPayload::TurnFailed { .. }
            | SemanticTranscriptEntryPayload::TurnCancelled { .. }
            | SemanticTranscriptEntryPayload::TurnCompleted { .. } => {}
        }
    }
    Ok(resolved.into_boxed_slice())
}

fn map_tool_evidence_error(
    error: crate::tool_loop::ToolLoopRepositoryError,
) -> ModelCallRepositoryError {
    match error {
        crate::tool_loop::ToolLoopRepositoryError::Database {
            source,
            commit_ambiguous,
        } => ModelCallRepositoryError::from_database(source, commit_ambiguous),
        crate::tool_loop::ToolLoopRepositoryError::IdentityCollision
        | crate::tool_loop::ToolLoopRepositoryError::Corruption(_)
        | crate::tool_loop::ToolLoopRepositoryError::DifferentCommandKind
        | crate::tool_loop::ToolLoopRepositoryError::ConflictingCommandReuse
        | crate::tool_loop::ToolLoopRepositoryError::InvalidTransition(_) => {
            ModelCallCorruption::Inconsistent("tool conversation evidence").into()
        }
    }
}

async fn load_call_credential_reference(
    connection: &mut PgConnection,
    session: SessionId,
    call: ModelCallId,
) -> Result<ModelCallCredentialReference, ModelCallRepositoryError> {
    let reference = sqlx::query_scalar::<_, String>(
        "SELECT credential_reference
           FROM model_call
          WHERE session_id = $1
            AND model_call_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(call.into_uuid())
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ModelCallCorruption::Missing("prepared model call"))?;
    Ok(ModelCallCredentialReference::new(reference))
}

/// Freezes the session's recorded, still-effective user overrides for one newly
/// checkpointed model call.
///
/// Two things retire a recorded override. The first is the consuming
/// `user_override` decision that names it through its UNIQUE column — the
/// durable one-shot boundary. The second is an approval of the identical
/// command recorded by any other authority after the denial: the judge
/// approving the re-proposal it previously denied, a user decision after
/// escalation, or a policy approval. Retiring on the second matters because the
/// first call after a denial can never carry that denial's override (it is
/// checkpointed by the transaction that materializes the denied result), so its
/// re-proposal is decided without the override; leaving the override standing
/// would let a later call pre-approve a repeat of a side-effecting command the
/// session has already let through once.
///
/// "After the denial" is a structural ordering, not a clock — none of these
/// append-only tables carries one. Across turns it is `acceptance_position`,
/// the per-session position of the input that opened each turn. Within the
/// denial's own turn it is the attempt chain: each tool round continues into a
/// fresh `turn_attempt` through `continued_from_attempt_id`, so walking that
/// chain forward from the attempt that produced the denied proposal names the
/// later proposals of the same turn. Both are needed — the re-proposal this
/// override exists for is normally made in the denial's own turn, while a
/// later turn's proposal is ordered only by acceptance.
///
/// That scoping is load-bearing rather than decoration. The same command is
/// routinely approved and executed earlier in a session, long before a later
/// proposal of it is denied; retiring on an approval anywhere in the session
/// would retire most overrides at the instant they were recorded and leave the
/// command with nothing to authorize.
async fn freeze_recorded_user_overrides(
    connection: &mut PgConnection,
    session: SessionId,
    call: ModelCallId,
) -> Result<(), ModelCallRepositoryError> {
    sqlx::query(
        "WITH RECURSIVE effective AS (
            SELECT recorded.denied_request_id,
                   denied_turn.acceptance_position AS denied_turn_position,
                   producing.turn_attempt_id AS denied_attempt_id,
                   denied.tool_name, denied.arguments_kind, denied.arguments_text
              FROM tool_approval_user_override AS recorded
              JOIN tool_request AS denied
                ON denied.request_id = recorded.denied_request_id
              JOIN turn_lifecycle AS denied_turn
                ON denied_turn.turn_id = denied.turn_id
              JOIN model_call AS producing
                ON producing.model_call_id = denied.producing_model_call_id
             WHERE recorded.session_id = $1
               AND NOT EXISTS (
                   SELECT 1
                     FROM tool_approval_decision AS consumed
                    WHERE consumed.override_denied_request_id
                          = recorded.denied_request_id
               )
         ),
         -- The attempts the denial's own turn ran after the denied proposal's.
         later_attempt AS (
            SELECT effective.denied_request_id, successor.turn_attempt_id
              FROM effective
              JOIN turn_attempt AS successor
                ON successor.continued_from_attempt_id
                   = effective.denied_attempt_id
             UNION
            SELECT walked.denied_request_id, successor.turn_attempt_id
              FROM later_attempt AS walked
              JOIN turn_attempt AS successor
                ON successor.continued_from_attempt_id = walked.turn_attempt_id
         )
         INSERT INTO model_call_user_override
            (model_call_id, denied_request_id)
         SELECT $2, effective.denied_request_id
           FROM effective
          WHERE NOT EXISTS (
              SELECT 1
                FROM tool_request AS matching
                JOIN tool_approval_decision AS decision
                  ON decision.request_id = matching.request_id
                 AND decision.decision_kind = 'approve'
                JOIN turn_lifecycle AS matching_turn
                  ON matching_turn.turn_id = matching.turn_id
                JOIN model_call AS proposing
                  ON proposing.model_call_id = matching.producing_model_call_id
               WHERE matching.session_id = $1
                 AND matching.tool_name = effective.tool_name
                 AND matching.arguments_kind = effective.arguments_kind
                 AND matching.arguments_text = effective.arguments_text
                 AND (
                     matching_turn.acceptance_position
                         > effective.denied_turn_position
                     OR EXISTS (
                         SELECT 1
                           FROM later_attempt
                          WHERE later_attempt.denied_request_id
                                = effective.denied_request_id
                            AND later_attempt.turn_attempt_id
                                = proposing.turn_attempt_id
                     )
                 )
          )",
    )
    .bind(session_id_to_uuid(session))
    .bind(call.into_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

/// Reloads exactly the override inventory frozen when this call was
/// checkpointed, irrespective of overrides recorded or consumed afterward.
async fn load_call_user_overrides(
    connection: &mut PgConnection,
    session: SessionId,
    call: ModelCallId,
) -> Result<Box<[signalbox_domain::RecordedUserOverride]>, ModelCallRepositoryError> {
    let rows = sqlx::query(
        "SELECT recorded.command_id, recorded.denied_request_id, recorded.judge_model_call_id,
                request.tool_name, request.arguments_kind, request.arguments_text
           FROM model_call_user_override AS frozen
           JOIN tool_approval_user_override AS recorded
             ON recorded.denied_request_id = frozen.denied_request_id
           JOIN tool_request AS request
             ON request.request_id = recorded.denied_request_id
          WHERE recorded.session_id = $1
            AND frozen.model_call_id = $2
          ORDER BY recorded.denied_request_id",
    )
    .bind(session_id_to_uuid(session))
    .bind(call.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    rows.into_iter()
        .map(|row| {
            let command: Uuid = row.try_get("command_id")?;
            let denied_request: Uuid = row.try_get("denied_request_id")?;
            let judge_call: Uuid = row.try_get("judge_model_call_id")?;
            let tool = signalbox_domain::ToolName::try_new(row.try_get("tool_name")?)
                .map_err(|_| ModelCallCorruption::Inconsistent("recorded override tool name"))?;
            let arguments_kind = match row.try_get::<String, _>("arguments_kind")?.as_str() {
                "json" => signalbox_domain::ToolArgumentsKind::Json,
                "undecodable" => signalbox_domain::ToolArgumentsKind::Undecodable,
                _ => {
                    return Err(ModelCallCorruption::Inconsistent(
                        "recorded override arguments kind",
                    )
                    .into());
                }
            };
            let arguments = signalbox_domain::NormalizedToolArguments::try_from_stored(
                arguments_kind,
                row.try_get("arguments_text")?,
            )
            .map_err(|_| ModelCallCorruption::Inconsistent("recorded override arguments"))?;
            Ok(signalbox_domain::RecordedUserOverride::new(
                durable_command_id_from_uuid(command)
                    .map_err(|_| ModelCallCorruption::Inconsistent("recorded override command"))?,
                session,
                signalbox_domain::ToolRequestId::from_uuid(denied_request),
                ModelCallId::from_uuid(judge_call),
                tool,
                arguments,
            ))
        })
        .collect::<Result<Box<[_]>, ModelCallRepositoryError>>()
}

/// Loads the optional session system prompt from the exact immutable defaults
/// epoch the calling turn froze at origin acceptance.
///
/// The epoch row must exist for a live execution; its absence or an
/// inadmissible stored prompt fails closed as corruption rather than sending
/// a call without the instructions the epoch records.
async fn load_frozen_epoch_system_prompt(
    connection: &mut PgConnection,
    session: SessionId,
    defaults_version: signalbox_domain::SessionConfigurationDefaultsVersion,
) -> Result<Option<signalbox_domain::SessionSystemPrompt>, ModelCallRepositoryError> {
    let row = sqlx::query_scalar::<_, Option<String>>(
        "SELECT system_prompt
           FROM session_defaults_version
          WHERE session_id = $1
            AND version = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(defaults_version_to_numeric(defaults_version))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ModelCallCorruption::Missing("frozen defaults epoch"))?;
    row.map(|value| {
        signalbox_domain::SessionSystemPrompt::try_new(value)
            .map_err(|_| ModelCallCorruption::Inconsistent("system prompt admission").into())
    })
    .transpose()
}

async fn persist_authorization(
    connection: &mut PgConnection,
    authorized: &AuthorizedModelCall,
) -> Result<(), ModelCallRepositoryError> {
    let attempt_rows = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'running'
          WHERE turn_attempt_id = $1
            AND turn_id = $2
            AND session_id = $3
            AND state_kind IN ('prepared', 'running')
            AND end_variant IS NULL
            AND end_disposition IS NULL",
    )
    .bind(authorized.attempt().id().into_uuid())
    .bind(turn_id_to_uuid(authorized.turn()))
    .bind(session_id_to_uuid(authorized.session()))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    let call_rows = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'in_flight'
          WHERE model_call_id = $1
            AND turn_id = $2
            AND session_id = $3
            AND turn_attempt_id = $4
            AND state_kind = 'prepared'
            AND terminal_disposition_kind IS NULL",
    )
    .bind(authorized.call().id().into_uuid())
    .bind(turn_id_to_uuid(authorized.turn()))
    .bind(session_id_to_uuid(authorized.session()))
    .bind(authorized.attempt().id().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(attempt_rows, "send-authorization attempt")?;
    require_single(call_rows, "send-authorization call")?;
    outbox::append(
        connection,
        OutboxEvent::ModelCallTransition {
            session: authorized.session(),
            turn: authorized.turn(),
            call: authorized.call().id(),
            state: ModelCallOutboxState::InFlight,
        },
    )
    .await?;
    Ok(())
}

pub(crate) async fn persist_stop_requested(
    connection: &mut PgConnection,
    stopped: &StopRequestedModelCallTurn,
) -> Result<(), ModelCallRepositoryError> {
    let proof = stopped.interrupt();
    let attempt_rows = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'stop_requested',
                interrupt_command_id = $1,
                interrupt_predecessor_turn_id = $2
          WHERE turn_attempt_id = $3
            AND turn_id = $4
            AND session_id = $5
            AND state_kind = 'running'
            AND end_variant IS NULL
            AND end_disposition IS NULL
            AND interrupt_command_id IS NULL
            AND interrupt_predecessor_turn_id IS NULL",
    )
    .bind(durable_command_id_to_uuid(proof.command()))
    .bind(turn_id_to_uuid(proof.predecessor()))
    .bind(stopped.attempt().id().into_uuid())
    .bind(turn_id_to_uuid(stopped.turn()))
    .bind(session_id_to_uuid(stopped.session()))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(attempt_rows, "stop-requested turn attempt")?;

    let call_rows = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'cancellation_requested'
          WHERE model_call_id = $1
            AND turn_id = $2
            AND session_id = $3
            AND turn_attempt_id = $4
            AND state_kind = 'in_flight'
            AND terminal_disposition_kind IS NULL",
    )
    .bind(stopped.call().id().into_uuid())
    .bind(turn_id_to_uuid(stopped.turn()))
    .bind(session_id_to_uuid(stopped.session()))
    .bind(stopped.attempt().id().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(call_rows, "cancellation-requested model call")?;
    outbox::append(
        connection,
        OutboxEvent::ModelCallTransition {
            session: stopped.session(),
            turn: stopped.turn(),
            call: stopped.call().id(),
            state: ModelCallOutboxState::CancellationRequested,
        },
    )
    .await?;
    Ok(())
}

/// Persists one terminal outcome, recording `failure_cause` when the outcome
/// is `Failed`.
///
/// The cause is optional because two callers construct only non-failing
/// outcomes; a `Failed` outcome that names none is a typed corruption rather
/// than a silently unclassified terminalization.
pub(crate) async fn persist_terminal_outcome(
    connection: &mut PgConnection,
    outcome: &ModelCallTerminalOutcome,
    failure_cause: Option<TurnTerminalCause>,
) -> Result<(), ModelCallRepositoryError> {
    persist_terminal_outcome_with_usage(
        connection,
        outcome,
        failure_cause,
        ProviderReportedTokenUsage::unreported(),
        None,
    )
    .await
}

async fn persist_terminal_outcome_with_usage(
    connection: &mut PgConnection,
    outcome: &ModelCallTerminalOutcome,
    failure_cause: Option<TurnTerminalCause>,
    usage: ProviderReportedTokenUsage,
    provider_failure_cause: Option<ProviderModelCallFailureCause>,
) -> Result<(), ModelCallRepositoryError> {
    match outcome {
        ModelCallTerminalOutcome::Completed(completed) => {
            lock_delegated_child_result_frontier(connection, completed.session(), completed.turn())
                .await?;
            persist_completed(connection, completed, usage).await?;
            persist_delegated_child_result(
                connection,
                &DelegationOutcome::from_completed_child(completed),
            )
            .await
        }
        ModelCallTerminalOutcome::ToolRound(round) => {
            persist_tool_round(connection, round, usage).await
        }
        ModelCallTerminalOutcome::CancelledWithToolResponse(cancelled) => {
            lock_delegated_child_result_frontier(connection, cancelled.session(), cancelled.turn())
                .await?;
            persist_cancelled_tool_round(connection, cancelled, usage).await?;
            persist_delegated_child_result(
                connection,
                &DelegationOutcome::from_cancelled_tool_round_child(cancelled),
            )
            .await
        }
        ModelCallTerminalOutcome::Failed(failed) => {
            let cause = failure_cause.ok_or(ModelCallCorruption::Inconsistent(
                "failed terminal outcome without a terminal cause",
            ))?;
            persist_failed_with_delegated_child_result(
                connection,
                failed,
                cause,
                usage,
                provider_failure_cause,
                None,
            )
            .await
        }
        ModelCallTerminalOutcome::Cancelled(cancelled) => {
            lock_delegated_child_result_frontier(connection, cancelled.session(), cancelled.turn())
                .await?;
            persist_cancelled(connection, cancelled, usage).await?;
            persist_delegated_child_result(
                connection,
                &DelegationOutcome::from_cancelled_child(cancelled),
            )
            .await
        }
        ModelCallTerminalOutcome::Refused(refused) => {
            lock_delegated_child_result_frontier(connection, refused.session(), refused.turn())
                .await?;
            persist_refused(connection, refused, usage).await?;
            persist_delegated_child_result(
                connection,
                &DelegationOutcome::from_refused_child(refused),
            )
            .await
        }
        ModelCallTerminalOutcome::ReconciliationRequired(reconciliation) => {
            persist_reconciliation_required(connection, reconciliation, usage).await
        }
        ModelCallTerminalOutcome::AwaitingRecovery(ambiguous) => {
            persist_ambiguous(connection, ambiguous, usage).await
        }
    }
}

async fn persist_failed_with_delegated_child_result(
    connection: &mut PgConnection,
    failed: &FailedModelCallTurn,
    cause: TurnTerminalCause,
    usage: ProviderReportedTokenUsage,
    provider_failure_cause: Option<ProviderModelCallFailureCause>,
    attachment_failure: Option<AttachmentPreparationFailure>,
) -> Result<(), ModelCallRepositoryError> {
    lock_delegated_child_result_frontier(connection, failed.session(), failed.turn()).await?;
    persist_failed(
        connection,
        failed,
        cause,
        usage,
        provider_failure_cause,
        attachment_failure,
    )
    .await?;
    persist_delegated_child_result(connection, &DelegationOutcome::from_failed_child(failed)).await
}

/// Encodes the durable evidence a definitive attachment-preparation failure
/// carries into the statement that terminalizes its call.
///
/// `model_call_changes_are_guarded` raises on every update whose OLD row is
/// already terminal, so this evidence is only writable by the same
/// Prepared-to-terminal `UPDATE` that closes the call; a follow-up statement
/// would abort the whole failure transaction and leave the call open.
/// `Unavailable` is retryable and so never terminalizes a prepared call.
fn encode_attachment_preparation_failure(
    failure: AttachmentPreparationFailure,
) -> Result<(&'static str, Option<Decimal>), ModelCallRepositoryError> {
    match failure {
        AttachmentPreparationFailure::TooLarge { maximum_bytes } => {
            Ok(("too_large", Some(Decimal::from(maximum_bytes))))
        }
        AttachmentPreparationFailure::Missing => Ok(("missing", None)),
        AttachmentPreparationFailure::Corrupt => Ok(("corrupt", None)),
        AttachmentPreparationFailure::Unavailable => {
            Err(ModelCallRepositoryError::InvalidTransition(
                "retryable attachment unavailability cannot terminalize a prepared call",
            ))
        }
    }
}

pub(crate) async fn persist_automatic_reconciliation(
    connection: &mut PgConnection,
    reconciliation: &ReconciliationRequiredModelCallTurn,
) -> Result<(), ModelCallRepositoryError> {
    lock_delegated_child_result_frontier(
        connection,
        reconciliation.session(),
        reconciliation.turn(),
    )
    .await?;
    persist_reconciliation_required(
        connection,
        reconciliation,
        ProviderReportedTokenUsage::unreported(),
    )
    .await?;
    persist_delegated_child_result(
        connection,
        &DelegationOutcome::from_reconciliation_required_child(reconciliation),
    )
    .await
}

async fn lock_delegated_child_result_frontier(
    connection: &mut PgConnection,
    child: SessionId,
    turn: TurnId,
) -> Result<(), ModelCallRepositoryError> {
    let relation = load_delegation_terminal_relation(
        connection,
        crate::lock_inventory::DELEGATION_TERMINAL_RELATION_IDENTITY,
        session_id_to_uuid(child),
        turn_id_to_uuid(turn),
    )
    .await?;
    let Some(relation) = relation else {
        return Ok(());
    };
    sqlx::query(crate::lock_inventory::DELEGATION_TERMINAL_ENDPOINT_SESSION)
        .bind(relation.parent_session_id)
        .execute(&mut *connection)
        .await?;
    let locked = load_delegation_terminal_relation(
        connection,
        crate::lock_inventory::DELEGATION_TERMINAL_RELATION,
        session_id_to_uuid(child),
        turn_id_to_uuid(turn),
    )
    .await?;
    if locked != Some(relation) {
        return Err(ModelCallCorruption::Inconsistent(
            "delegated terminal relationship changed while locking",
        )
        .into());
    }
    Ok(())
}

/// Locks the immutable parent/child endpoint pair before a child scheduler can
/// be locked by a transaction that may terminalize the delegated child.
pub(crate) async fn lock_delegated_child_endpoint_sessions(
    connection: &mut PgConnection,
    child: SessionId,
) -> Result<(), ModelCallRepositoryError> {
    let parent: Option<Uuid> = sqlx::query_scalar(
        "SELECT parent_session_id
           FROM session_delegation
          WHERE child_session_id = $1",
    )
    .bind(session_id_to_uuid(child))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(parent) = parent else {
        return Ok(());
    };
    let parent = session_id_from_uuid(parent);
    let (first, second) = crate::lock_inventory::ordered_session_pair(child, parent);
    sqlx::query(crate::lock_inventory::DELEGATION_TERMINAL_ENDPOINT_SESSION)
        .bind(session_id_to_uuid(first))
        .execute(&mut *connection)
        .await?;
    if second != first {
        sqlx::query(crate::lock_inventory::DELEGATION_TERMINAL_ENDPOINT_SESSION)
            .bind(session_id_to_uuid(second))
            .execute(&mut *connection)
            .await?;
    }
    Ok(())
}

async fn persist_delegated_child_result(
    connection: &mut PgConnection,
    outcome: &DelegationOutcome,
) -> Result<(), ModelCallRepositoryError> {
    let (child, turn) =
        outcome
            .provenance()
            .child_turn()
            .ok_or(ModelCallCorruption::Inconsistent(
                "delegated child result provenance",
            ))?;
    // The match admits only the outcome/reason pairings a terminal child result
    // may carry; the spellings themselves come from the canonical encoders, so
    // this states the pairing rule without restating the durable vocabulary.
    let (outcome_pairing, reason_pairing) = match (outcome.kind(), outcome.reason()) {
        pairing @ (
            DelegationOutcomeKind::ResultReturned,
            DelegationOutcomeReason::ChildCompleted,
        )
        | pairing @ (
            DelegationOutcomeKind::ChildFailed,
            DelegationOutcomeReason::ChildExecutionFailed,
        )
        | pairing @ (
            DelegationOutcomeKind::ChildFailed,
            DelegationOutcomeReason::ChildResultUnavailable,
        )
        | pairing @ (
            DelegationOutcomeKind::ChildCancelled,
            DelegationOutcomeReason::ChildCancelled,
        ) => pairing,
        _ => {
            return Err(
                ModelCallCorruption::Inconsistent("terminal delegated child outcome").into(),
            );
        }
    };
    let outcome_kind = delegation_outcome_kind_to_str(outcome_pairing);
    let reason_kind = delegation_outcome_reason_to_str(reason_pairing).ok_or(
        ModelCallCorruption::Inconsistent("terminal delegated child outcome"),
    )?;
    let content = outcome.content().map(DelegationContent::as_str);
    let relation = load_delegation_terminal_relation(
        connection,
        crate::lock_inventory::DELEGATION_TERMINAL_RELATION,
        session_id_to_uuid(child),
        turn_id_to_uuid(turn),
    )
    .await?;
    let Some(relation) = relation else {
        return Ok(());
    };
    let spawning_request = relation.spawning_tool_request_id;
    let parent = relation.parent_session_id;
    if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1 FROM session_child_result
             WHERE spawning_tool_request_id = $1
        )",
    )
    .bind(relation.spawning_tool_request_id)
    .fetch_one(&mut *connection)
    .await?
    {
        return Ok(());
    }
    let event_ordinal = sqlx::query_scalar::<_, Decimal>(
        "SELECT COALESCE(max(event_ordinal), 0) + 1
           FROM session_delegation_event
          WHERE spawning_tool_request_id = $1",
    )
    .bind(spawning_request)
    .fetch_one(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, reason_kind, provenance_kind,
             provenance_session_id, provenance_turn_id)
         VALUES ($1, $2, 'outcome_recorded', $3,
                 $4, 'child_turn', $5, $6)",
    )
    .bind(spawning_request)
    .bind(event_ordinal)
    .bind(outcome_kind)
    .bind(reason_kind)
    .bind(session_id_to_uuid(child))
    .bind(turn_id_to_uuid(turn))
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_child_result
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, content_text)
         VALUES ($1, $2, 'outcome_recorded', $3, $4)",
    )
    .bind(spawning_request)
    .bind(event_ordinal)
    .bind(outcome_kind)
    .bind(content)
    .execute(&mut *connection)
    .await?;

    let waits = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT awaiting_tool_request_id, wait_mode
           FROM session_delegation_wait
          WHERE spawning_tool_request_id = $1
          ORDER BY awaiting_tool_request_id",
    )
    .bind(spawning_request)
    .fetch_all(&mut *connection)
    .await?;
    for (awaiting_request, mode) in waits {
        let delivery_sequence = match mode.as_str() {
            "foreground" => None,
            "background" => Some(
                sqlx::query_scalar::<_, Decimal>(
                    "WITH next_delivery AS (
                        SELECT COALESCE(max(delivery_sequence), 0) + 1 AS sequence
                          FROM session_pending_delivery
                         WHERE recipient_session_id = $1
                     )
                     INSERT INTO session_pending_delivery
                        (recipient_session_id, delivery_sequence, delivery_kind)
                     SELECT $1, sequence, 'background_result'
                       FROM next_delivery
                     RETURNING delivery_sequence",
                )
                .bind(parent)
                .fetch_one(&mut *connection)
                .await?,
            ),
            value => {
                return Err(ModelCallCorruption::Unsupported {
                    field: "delegation wait mode",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        sqlx::query(
            "INSERT INTO session_child_result_delivery
                (awaiting_tool_request_id, spawning_tool_request_id,
                 parent_session_id, delivery_sequence, delivery_kind)
             VALUES ($1, $2, $3, $4,
                     CASE WHEN $4::numeric IS NULL
                          THEN NULL ELSE 'background_result' END)",
        )
        .bind(awaiting_request)
        .bind(relation.spawning_tool_request_id)
        .bind(relation.parent_session_id)
        .bind(delivery_sequence)
        .execute(&mut *connection)
        .await?;
    }

    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event
                (event_kind, storage_version, session_id)
            VALUES ('delegation_update', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_update_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             update_kind, spawning_tool_request_id, child_session_id,
             outcome_kind, reason_kind, provenance_kind,
             provenance_session_id, provenance_turn_id,
             result_spawning_request_id, content_text)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $8::text, $2, $3, $4, $5,
                'child_turn', $3, $6, $2, $7
           FROM header",
    )
    .bind(parent)
    .bind(spawning_request)
    .bind(session_id_to_uuid(child))
    .bind(outcome_kind)
    .bind(reason_kind)
    .bind(turn_id_to_uuid(turn))
    .bind(content)
    .bind(delegation_update_kind_to_str(
        DelegationUpdateStorageKind::ChildResult,
    ))
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event
                (event_kind, storage_version, session_id)
            VALUES ('delegation_wake', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_wake_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             spawning_tool_request_id, subject_kind,
             result_spawning_request_id, awaiting_tool_request_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $2, $3::text, $2, NULL
           FROM header",
    )
    .bind(parent)
    .bind(spawning_request)
    .bind(delegation_wake_subject_to_str(
        DelegationWakeStorageKind::Result,
    ))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

pub(crate) async fn persist_reconciliation_required(
    connection: &mut PgConnection,
    reconciliation: &ReconciliationRequiredModelCallTurn,
    usage: ProviderReportedTokenUsage,
) -> Result<(), ModelCallRepositoryError> {
    let call_already_ambiguous = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM model_call
             WHERE model_call_id = $1
               AND turn_id = $2
               AND session_id = $3
               AND turn_attempt_id = $4
               AND state_kind = 'terminal'
               AND terminal_disposition_kind = 'ambiguous'
        )",
    )
    .bind(reconciliation.call().id().into_uuid())
    .bind(turn_id_to_uuid(reconciliation.turn()))
    .bind(session_id_to_uuid(reconciliation.session()))
    .bind(reconciliation.attempt().id().into_uuid())
    .fetch_one(&mut *connection)
    .await?;
    if !call_already_ambiguous {
        persist_ended_call(
            connection,
            reconciliation.session(),
            reconciliation.turn(),
            reconciliation.call(),
            usage,
        )
        .await?;
    }
    if !call_already_ambiguous {
        persist_ended_attempt(
            connection,
            reconciliation.session(),
            reconciliation.turn(),
            reconciliation.attempt(),
        )
        .await?;
    }
    insert_snapshot(connection, reconciliation.terminal_snapshot()).await?;
    persist_reclassified_pending_steering(
        connection,
        reconciliation.session(),
        reconciliation.turn(),
        reconciliation.reclassified_pending_steering(),
    )
    .await?;
    terminalize_lifecycle(
        connection,
        reconciliation.session(),
        reconciliation.turn(),
        "reconciliation_required",
        TurnTerminalCause::ModelCallAmbiguous,
        reconciliation.terminal_snapshot().frontier().snapshot(),
        Some(reconciliation.attempt().id()),
        Some(reconciliation.call().id()),
    )
    .await?;
    if !call_already_ambiguous {
        append_terminal_call_event(
            connection,
            reconciliation.session(),
            reconciliation.turn(),
            reconciliation.call(),
        )
        .await?;
    }
    outbox::append(
        connection,
        OutboxEvent::TurnTerminal {
            session: reconciliation.session(),
            turn: reconciliation.turn(),
            disposition: TurnTerminalOutboxDisposition::ModelCallReconciliationRequired {
                call: reconciliation.call().id(),
                terminal_frontier: reconciliation.terminal_snapshot().frontier().snapshot(),
            },
        },
    )
    .await?;
    Ok(())
}

pub(crate) async fn persist_tool_reconciliation_required(
    connection: &mut PgConnection,
    reconciliation: &ReconciliationRequiredToolTurn,
) -> Result<(), ModelCallRepositoryError> {
    crate::tool_loop::persist_result_entry_slice(connection, reconciliation.tool_result_entries())
        .await
        .map_err(map_tool_evidence_error)?;
    insert_snapshot(connection, reconciliation.terminal_snapshot()).await?;
    persist_reclassified_pending_steering(
        connection,
        reconciliation.session(),
        reconciliation.turn(),
        reconciliation.reclassified_pending_steering(),
    )
    .await?;
    let rows = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                terminal_frontier_id = $1,
                active_phase_kind = NULL,
                current_attempt_id = NULL,
                recovery_model_call_id = NULL,
                active_tool_round_call_id = NULL,
                approval_tool_request_id = NULL,
                recovery_tool_attempt_id = NULL,
                runner_recovery_runner_id = NULL,
                runner_recovery_placement_revision = NULL,
                runner_recovery_tool_attempt_id = NULL,
                terminal_attempt_id = $2,
                terminal_model_call_id = NULL,
                terminal_tool_attempt_id = $3,
                terminal_disposition_kind = 'reconciliation_required',
                terminal_cause_kind = $6
          WHERE turn_id = $4
            AND session_id = $5
            AND state_kind = 'active'
            AND (
                (
                    active_phase_kind = 'awaiting_tool_recovery'
                    AND current_attempt_id = $2
                    AND recovery_tool_attempt_id = $3
                )
                OR (
                    active_phase_kind = 'awaiting_runner_recovery'
                    AND current_attempt_id IS NULL
                    AND runner_recovery_tool_attempt_id = $3
                    AND EXISTS (
                        SELECT 1
                          FROM turn_attempt AS yielded_attempt
                         WHERE yielded_attempt.turn_attempt_id = $2
                           AND yielded_attempt.turn_id = turn_lifecycle.turn_id
                           AND yielded_attempt.session_id = turn_lifecycle.session_id
                           AND yielded_attempt.state_kind = 'ended'
                           AND yielded_attempt.end_variant = 'without_stop'
                           AND yielded_attempt.end_disposition =
                                'yielded_to_durable_wait'
                           AND NOT EXISTS (
                                SELECT 1
                                  FROM turn_attempt AS continuation
                                 WHERE continuation.continued_from_attempt_id =
                                        yielded_attempt.turn_attempt_id
                           )
                    )
                )
            )",
    )
    .bind(
        reconciliation
            .terminal_snapshot()
            .frontier()
            .snapshot()
            .into_uuid(),
    )
    .bind(reconciliation.attempt().id().into_uuid())
    .bind(reconciliation.tool_attempt().attempt().into_uuid())
    .bind(turn_id_to_uuid(reconciliation.turn()))
    .bind(session_id_to_uuid(reconciliation.session()))
    .bind(turn_terminal_cause_to_str(
        TurnTerminalCause::ToolAttemptAmbiguous,
    ))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "terminal tool-reconciliation lifecycle")?;
    outbox::append(
        connection,
        OutboxEvent::TurnTerminal {
            session: reconciliation.session(),
            turn: reconciliation.turn(),
            disposition: TurnTerminalOutboxDisposition::ToolAttemptReconciliationRequired {
                attempt: reconciliation.tool_attempt().attempt(),
                terminal_frontier: reconciliation.terminal_snapshot().frontier().snapshot(),
            },
        },
    )
    .await?;
    Ok(())
}

async fn persist_tool_round(
    connection: &mut PgConnection,
    round: &ToolRoundModelCallTurn,
    usage: ProviderReportedTokenUsage,
) -> Result<(), ModelCallRepositoryError> {
    persist_ended_call(
        connection,
        round.session(),
        round.turn(),
        round.call(),
        usage,
    )
    .await?;
    persist_ended_attempt(connection, round.session(), round.turn(), round.attempt()).await?;
    persist_tool_round_authority(
        connection,
        round.session(),
        round.turn(),
        round.call().id(),
        "continuing",
        round.yielded_snapshot().frontier().snapshot(),
        round.assistant_entries(),
        round.requests(),
    )
    .await?;
    for approval in round.automatic_approvals() {
        let (decision_kind, denial_reason) = encode_tool_approval(approval.decision());
        let source = encode_tool_decision_source(approval.source())?;
        let override_denied_request = match (approval.source(), approval.decider()) {
            (
                ToolDecisionSource::UserOverride,
                Some(signalbox_domain::ToolApprovalDecider::UserOverride {
                    denied_request, ..
                }),
            ) => Some(tool_request_id_to_uuid(*denied_request)),
            (ToolDecisionSource::UserOverride, _) => {
                return Err(
                    ModelCallCorruption::Inconsistent("user-override decision provenance").into(),
                );
            }
            _ => None,
        };
        sqlx::query(
            "INSERT INTO tool_approval_decision
                (request_id, decision_kind, decision_source, denial_reason,
                 user_command_id, override_denied_request_id)
             VALUES ($1, $2, $3, $4, NULL, $5)",
        )
        .bind(tool_request_id_to_uuid(approval.request()))
        .bind(decision_kind)
        .bind(source)
        .bind(denial_reason)
        .bind(override_denied_request)
        .execute(&mut *connection)
        .await?;
        if approval.source() == ToolDecisionSource::UserOverride {
            outbox::append(
                connection,
                OutboxEvent::ToolApprovalDecided {
                    session: round.session(),
                    turn: round.turn(),
                    request: approval.request(),
                },
            )
            .await?;
        }
    }
    insert_snapshot(connection, round.yielded_snapshot()).await?;

    match round.next_phase() {
        ActiveTurnPhase::AwaitingApproval { request } => {
            let rows = sqlx::query(
                "UPDATE turn_lifecycle
                    SET active_phase_kind = 'awaiting_tool_approval',
                        current_attempt_id = NULL,
                        active_tool_round_call_id = $1,
                        approval_tool_request_id = $2,
                        recovery_tool_attempt_id = NULL
                  WHERE turn_id = $3
                    AND session_id = $4
                    AND state_kind = 'active'
                    AND active_phase_kind = 'running'
                    AND current_attempt_id = $5
                    AND active_tool_round_call_id IS NULL
                    AND approval_tool_request_id IS NULL
                    AND recovery_tool_attempt_id IS NULL",
            )
            .bind(round.call().id().into_uuid())
            .bind(tool_request_id_to_uuid(*request))
            .bind(turn_id_to_uuid(round.turn()))
            .bind(session_id_to_uuid(round.session()))
            .bind(round.attempt().id().into_uuid())
            .execute(&mut *connection)
            .await?
            .rows_affected();
            require_single(rows, "tool-round approval wait")?;
        }
        ActiveTurnPhase::Running { current_attempt } => {
            if !matches!(
                current_attempt.state(),
                signalbox_domain::CurrentTurnAttemptState::Prepared
            ) {
                return Err(
                    ModelCallCorruption::Inconsistent("tool continuation attempt state").into(),
                );
            }
            sqlx::query(
                "INSERT INTO turn_attempt
                    (turn_attempt_id, turn_id, session_id,
                     continued_from_attempt_id, state_kind)
                 VALUES ($1, $2, $3, $4, 'prepared')",
            )
            .bind(current_attempt.id().into_uuid())
            .bind(turn_id_to_uuid(round.turn()))
            .bind(session_id_to_uuid(round.session()))
            .bind(round.attempt().id().into_uuid())
            .execute(&mut *connection)
            .await?;
            let rows = sqlx::query(
                "UPDATE turn_lifecycle
                    SET active_phase_kind = 'running',
                        current_attempt_id = $1,
                        active_tool_round_call_id = $2,
                        approval_tool_request_id = NULL,
                        recovery_tool_attempt_id = NULL
                  WHERE turn_id = $3
                    AND session_id = $4
                    AND state_kind = 'active'
                    AND active_phase_kind = 'running'
                    AND current_attempt_id = $5
                    AND active_tool_round_call_id IS NULL
                    AND approval_tool_request_id IS NULL
                    AND recovery_tool_attempt_id IS NULL",
            )
            .bind(current_attempt.id().into_uuid())
            .bind(round.call().id().into_uuid())
            .bind(turn_id_to_uuid(round.turn()))
            .bind(session_id_to_uuid(round.session()))
            .bind(round.attempt().id().into_uuid())
            .execute(&mut *connection)
            .await?
            .rows_affected();
            require_single(rows, "auto-approved tool execution phase")?;
        }
        ActiveTurnPhase::AwaitingChild { .. }
        | ActiveTurnPhase::AwaitingRecoveryDecision { .. }
        | ActiveTurnPhase::AwaitingRunnerRecovery { .. } => {
            return Err(
                ModelCallCorruption::Inconsistent("fresh tool round recovery phase").into(),
            );
        }
    }
    outbox::append(
        connection,
        OutboxEvent::ToolBatchTransition {
            session: round.session(),
            turn: round.turn(),
            producing_call: round.call().id(),
            state: ToolBatchOutboxState::Proposed(round.yielded_snapshot().frontier().snapshot()),
        },
    )
    .await?;
    append_terminal_call_event(connection, round.session(), round.turn(), round.call()).await
}

async fn persist_availability_successor(
    connection: &mut PgConnection,
    successor: &AvailabilitySuccessorModelCallTurn,
    usage: ProviderReportedTokenUsage,
    cause: ProviderModelCallFailureCause,
    backoff: Duration,
) -> Result<(), ModelCallRepositoryError> {
    persist_ended_call_with_provider_failure_cause(
        connection,
        successor.session(),
        successor.turn(),
        successor.predecessor_call(),
        usage,
        Some(cause),
        None,
    )
    .await?;
    persist_ended_attempt(
        connection,
        successor.session(),
        successor.turn(),
        successor.predecessor_attempt(),
    )
    .await?;
    if successor.successor_attempt().state() != &signalbox_domain::CurrentTurnAttemptState::Prepared
    {
        return Err(
            ModelCallCorruption::Inconsistent("availability successor attempt state").into(),
        );
    }
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id,
             continued_from_attempt_id, state_kind)
         VALUES ($1, $2, $3, $4, 'prepared')",
    )
    .bind(successor.successor_attempt().id().into_uuid())
    .bind(turn_id_to_uuid(successor.turn()))
    .bind(session_id_to_uuid(successor.session()))
    .bind(successor.predecessor_attempt().id().into_uuid())
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO credential_pool_availability_successor
            (predecessor_model_call_id, successor_turn_attempt_id, cause_kind,
             retry_backoff_milliseconds, retry_not_before)
         VALUES ($1, $2, $3, $4,
                 transaction_timestamp() + ($4 * interval '1 millisecond'))",
    )
    .bind(successor.predecessor_call().id().into_uuid())
    .bind(successor.successor_attempt().id().into_uuid())
    .bind(encode_provider_failure_cause(cause))
    .bind(i64::try_from(backoff.as_millis()).map_err(|_| {
        ModelCallRepositoryError::InvalidTransition("availability backoff overflow")
    })?)
    .execute(&mut *connection)
    .await?;
    let rows = sqlx::query(
        "UPDATE turn_lifecycle
            SET current_attempt_id = $1
          WHERE turn_id = $2
            AND session_id = $3
            AND state_kind = 'active'
            AND active_phase_kind = 'running'
            AND current_attempt_id = $4",
    )
    .bind(successor.successor_attempt().id().into_uuid())
    .bind(turn_id_to_uuid(successor.turn()))
    .bind(session_id_to_uuid(successor.session()))
    .bind(successor.predecessor_attempt().id().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "availability successor attempt")?;
    append_terminal_call_event(
        connection,
        successor.session(),
        successor.turn(),
        successor.predecessor_call(),
    )
    .await
}

/// Writes the single terminal-exhaustion row shared by both exhaustion shapes.
///
/// Pre-call exhaustion has no failing call to name; post-call exhaustion pins
/// the member call that consumed the last admissible member and its cause.
async fn insert_credential_pool_terminal_exhaustion(
    connection: &mut PgConnection,
    attempt: TurnAttemptId,
    session: SessionId,
    turn: TurnId,
    pool_name: &str,
    last_call: Option<ModelCallId>,
    last_cause: Option<ProviderModelCallFailureCause>,
) -> Result<(), ModelCallRepositoryError> {
    sqlx::query(
        "INSERT INTO credential_pool_terminal_exhaustion
            (terminal_attempt_id, terminal_model_call_id,
             session_id, turn_id, pool_name, cause_kind)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(attempt.into_uuid())
    .bind(last_call.map(ModelCallId::into_uuid))
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(pool_name)
    .bind(last_cause.map(encode_provider_failure_cause))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn persist_credential_pool_exhaustion(
    connection: &mut PgConnection,
    exhausted: &CredentialPoolExhaustedModelCallTurn,
) -> Result<(), ModelCallRepositoryError> {
    persist_failed_with_delegated_child_result(
        connection,
        exhausted.failed(),
        TurnTerminalCause::CredentialPoolExhausted,
        ProviderReportedTokenUsage::unreported(),
        None,
        None,
    )
    .await?;
    insert_credential_pool_terminal_exhaustion(
        connection,
        exhausted.failed().attempt().id(),
        exhausted.failed().session(),
        exhausted.failed().turn(),
        exhausted.pool_name(),
        None,
        None,
    )
    .await
}

async fn persist_tool_continuation_headroom_exhaustion(
    connection: &mut PgConnection,
    required: &ContextHeadroomExhaustedModelCallTurn,
    evidence: ToolContinuationHeadroomEvidence,
) -> Result<(), ModelCallRepositoryError> {
    let usage = encode_token_usage(evidence.usage);
    sqlx::query(
        "INSERT INTO tool_continuation_context_headroom
            (terminal_attempt_id, producing_model_call_id, session_id, turn_id,
             usage_input_includes_cache_tokens, usage_input_tokens,
             usage_output_tokens, usage_cache_creation_input_tokens,
             usage_cache_read_input_tokens, projected_result_content_bytes,
             max_output_tokens, context_window_tokens)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(required.failed().attempt().id().into_uuid())
    .bind(required.producing_call().into_uuid())
    .bind(session_id_to_uuid(required.failed().session()))
    .bind(turn_id_to_uuid(required.failed().turn()))
    .bind(evidence.input_includes_cache_tokens)
    .bind(usage.input_tokens)
    .bind(usage.output_tokens)
    .bind(usage.cache_creation_input_tokens)
    .bind(usage.cache_read_input_tokens)
    .bind(Decimal::from(evidence.projected_result_content_bytes))
    .bind(Decimal::from(evidence.limit.max_output_tokens()))
    .bind(Decimal::from(evidence.limit.context_window_tokens()))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

const MAX_AVAILABILITY_BACKOFF: Duration = Duration::from_secs(300);
const MAX_EXPONENTIAL_BACKOFF: Duration = Duration::from_secs(60);

const fn is_same_credential_retry_cause(cause: ProviderModelCallFailureCause) -> bool {
    matches!(
        cause,
        ProviderModelCallFailureCause::RateLimited
            | ProviderModelCallFailureCause::Overloaded
            | ProviderModelCallFailureCause::ProviderInternal
    )
}

async fn count_turn_credential_attempts(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    credential_reference: &str,
) -> Result<usize, ModelCallRepositoryError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM model_call
          WHERE session_id = $1
            AND turn_id = $2
            AND credential_reference = $3",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(credential_reference)
    .fetch_one(&mut *connection)
    .await?;
    usize::try_from(count)
        .map_err(|_| ModelCallCorruption::Inconsistent("same-credential attempt count").into())
}

fn availability_retry_backoff(
    cause: ProviderModelCallFailureCause,
    retry_after: Option<Duration>,
    failed_attempts: usize,
    call: ModelCallId,
) -> Duration {
    if !is_same_credential_retry_cause(cause) {
        return Duration::ZERO;
    }
    let exponent = u32::try_from(failed_attempts.saturating_sub(1).min(6)).unwrap_or(6);
    let ceiling = Duration::from_secs(1_u64 << exponent).min(MAX_EXPONENTIAL_BACKOFF);
    let sample = u64::try_from(call.as_uuid().as_u128() & 1023).unwrap_or(0);
    let ceiling_millis = u64::try_from(ceiling.as_millis()).unwrap_or(u64::MAX);
    let jittered_millis = ceiling_millis * (512 + sample) / 1024;
    let jittered = Duration::from_millis(jittered_millis);
    jittered
        .max(retry_after.unwrap_or(Duration::ZERO))
        .min(MAX_AVAILABILITY_BACKOFF)
}

async fn persist_cancelled_tool_round(
    connection: &mut PgConnection,
    cancelled: &CancelledToolRoundModelCallTurn,
    usage: ProviderReportedTokenUsage,
) -> Result<(), ModelCallRepositoryError> {
    persist_ended_call(
        connection,
        cancelled.session(),
        cancelled.turn(),
        cancelled.call(),
        usage,
    )
    .await?;
    persist_ended_attempt(
        connection,
        cancelled.session(),
        cancelled.turn(),
        cancelled.attempt(),
    )
    .await?;
    persist_tool_round_authority(
        connection,
        cancelled.session(),
        cancelled.turn(),
        cancelled.call().id(),
        "closed_by_turn_end",
        cancelled.terminal_snapshot().frontier().snapshot(),
        cancelled.assistant_entries(),
        cancelled.requests(),
    )
    .await?;
    for entry in cancelled.closed_result_entries() {
        let SemanticTranscriptEntryPayload::ToolClosed { request } = entry.payload() else {
            return Err(ModelCallCorruption::Inconsistent("closed tool-result payload").into());
        };
        sqlx::query(
            "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 tool_result_request_id)
             VALUES ($1, $2, 'tool_closed_by_turn_end', $3)",
        )
        .bind(session_id_to_uuid(entry.source_session()))
        .bind(entry.identity().into_uuid())
        .bind(tool_request_id_to_uuid(*request))
        .execute(&mut *connection)
        .await?;
    }
    let cancellation = cancelled.cancellation_entry();
    if !matches!(
        cancellation.payload(),
        SemanticTranscriptEntryPayload::TurnCancelled { turn }
            if *turn == cancelled.turn()
    ) {
        return Err(ModelCallCorruption::Inconsistent("cancellation entry payload").into());
    }
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             cancelled_turn_id)
         VALUES ($1, $2, 'turn_cancelled', $3)",
    )
    .bind(session_id_to_uuid(cancellation.source_session()))
    .bind(cancellation.identity().into_uuid())
    .bind(turn_id_to_uuid(cancelled.turn()))
    .execute(&mut *connection)
    .await?;
    insert_snapshot(connection, cancelled.terminal_snapshot()).await?;
    persist_reclassified_pending_steering(
        connection,
        cancelled.session(),
        cancelled.turn(),
        cancelled.reclassified_pending_steering(),
    )
    .await?;
    terminalize_lifecycle(
        connection,
        cancelled.session(),
        cancelled.turn(),
        "cancelled",
        TurnTerminalCause::InterruptApplied,
        cancelled.terminal_snapshot().frontier().snapshot(),
        Some(cancelled.attempt().id()),
        Some(cancelled.call().id()),
    )
    .await?;
    append_terminal_call_event(
        connection,
        cancelled.session(),
        cancelled.turn(),
        cancelled.call(),
    )
    .await?;
    outbox::append(
        connection,
        OutboxEvent::TurnTerminal {
            session: cancelled.session(),
            turn: cancelled.turn(),
            disposition: TurnTerminalOutboxDisposition::Cancelled {
                cancellation_entry: cancellation.identity(),
                terminal_frontier: cancelled.terminal_snapshot().frontier().snapshot(),
            },
        },
    )
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn persist_tool_round_authority(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call: ModelCallId,
    boundary_kind: &'static str,
    boundary_frontier: signalbox_domain::ContextFrontierId,
    assistant_entries: &[SemanticTranscriptEntry],
    requests: &[ToolRequest],
) -> Result<(), ModelCallRepositoryError> {
    let response_part_count = u64::try_from(assistant_entries.len())
        .map_err(|_| ModelCallCorruption::Inconsistent("tool response part count"))?;
    let request_count = u64::try_from(requests.len())
        .map_err(|_| ModelCallCorruption::Inconsistent("tool request count"))?;
    sqlx::query(
        "INSERT INTO tool_round
            (producing_model_call_id, session_id, turn_id, boundary_kind,
             boundary_frontier_id, response_part_count, request_count)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(call.into_uuid())
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(boundary_kind)
    .bind(boundary_frontier.into_uuid())
    .bind(Decimal::from(response_part_count))
    .bind(Decimal::from(request_count))
    .execute(&mut *connection)
    .await?;
    for request in requests {
        let arguments_kind = match request.arguments().kind() {
            signalbox_domain::ToolArgumentsKind::Json => "json",
            signalbox_domain::ToolArgumentsKind::Undecodable => "undecodable",
        };
        let approval_posture = tool_approval_posture_to_str(request.approval_posture());
        sqlx::query(
            "INSERT INTO tool_request
                (request_id, session_id, turn_id, producing_model_call_id,
                 request_ordinal, tool_name, arguments_kind, arguments_text, approval_posture)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(tool_request_id_to_uuid(request.id()))
        .bind(session_id_to_uuid(request.session()))
        .bind(turn_id_to_uuid(request.turn()))
        .bind(request.producing_call().into_uuid())
        .bind(Decimal::from(request.ordinal().as_u32()))
        .bind(request.name().as_str())
        .bind(arguments_kind)
        .bind(request.arguments().as_str())
        .bind(approval_posture)
        .execute(&mut *connection)
        .await?;
    }
    let mut response_text_start_bytes = 0_u64;
    for (response_part_ordinal, entry) in assistant_entries.iter().enumerate() {
        let response_part_ordinal = u64::try_from(response_part_ordinal)
            .map_err(|_| ModelCallCorruption::Inconsistent("tool response part ordinal"))?;
        match entry.payload() {
            SemanticTranscriptEntryPayload::AssistantText {
                producing_call,
                value,
            } => {
                sqlx::query(
                    "INSERT INTO semantic_transcript_entry
                        (source_session_id, semantic_entry_id, payload_kind,
                         assistant_text_value, producing_model_call_id,
                         assistant_response_part_ordinal,
                         assistant_response_text_start_bytes)
                     VALUES ($1, $2, 'assistant_text', $3, $4, $5, $6)",
                )
                .bind(session_id_to_uuid(entry.source_session()))
                .bind(entry.identity().into_uuid())
                .bind(value.as_str())
                .bind(producing_call.into_uuid())
                .bind(Decimal::from(response_part_ordinal))
                .bind(Decimal::from(response_text_start_bytes))
                .execute(&mut *connection)
                .await?;
                response_text_start_bytes = response_text_start_bytes
                    .checked_add(u64::try_from(value.as_str().len()).map_err(|_| {
                        ModelCallCorruption::Inconsistent("tool response text byte length")
                    })?)
                    .ok_or(ModelCallCorruption::Inconsistent(
                        "tool response text byte position",
                    ))?;
            }
            SemanticTranscriptEntryPayload::ProviderCompaction {
                producing_call,
                block,
            } => {
                sqlx::query(
                    "INSERT INTO semantic_transcript_entry
                        (source_session_id, semantic_entry_id, payload_kind,
                         assistant_text_value, producing_model_call_id,
                         assistant_response_part_ordinal)
                     VALUES ($1, $2, 'provider_compaction', $3, $4, $5)",
                )
                .bind(session_id_to_uuid(entry.source_session()))
                .bind(entry.identity().into_uuid())
                .bind(block.as_json())
                .bind(producing_call.into_uuid())
                .bind(Decimal::from(response_part_ordinal))
                .execute(&mut *connection)
                .await?;
            }
            SemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            } => {
                sqlx::query(
                    "INSERT INTO semantic_transcript_entry
                        (source_session_id, semantic_entry_id, payload_kind,
                         producing_model_call_id, assistant_tool_request_id,
                         assistant_response_part_ordinal)
                     VALUES ($1, $2, 'assistant_tool_use', $3, $4, $5)",
                )
                .bind(session_id_to_uuid(entry.source_session()))
                .bind(entry.identity().into_uuid())
                .bind(producing_call.into_uuid())
                .bind(tool_request_id_to_uuid(*request))
                .bind(Decimal::from(response_part_ordinal))
                .execute(&mut *connection)
                .await?;
            }
            _ => {
                return Err(
                    ModelCallCorruption::Inconsistent("tool round assistant payload").into(),
                );
            }
        }
    }
    Ok(())
}

fn encode_tool_approval(decision: &ToolApprovalDecision) -> (&'static str, Option<&str>) {
    match decision {
        ToolApprovalDecision::Approve => ("approve", None),
        ToolApprovalDecision::Deny { reason } => (
            "deny",
            reason
                .as_ref()
                .map(signalbox_domain::ToolDenialReason::as_str),
        ),
    }
}

fn encode_tool_decision_source(
    source: ToolDecisionSource,
) -> Result<&'static str, ModelCallRepositoryError> {
    let storage_kind = match source {
        ToolDecisionSource::UserCommand => ToolApprovalDecisionSourceStorageKind::UserCommand,
        ToolDecisionSource::PolicyAuto => ToolApprovalDecisionSourceStorageKind::PolicyAuto,
        ToolDecisionSource::SessionBlanket => ToolApprovalDecisionSourceStorageKind::SessionBlanket,
        ToolDecisionSource::RuntimeSafety => ToolApprovalDecisionSourceStorageKind::RuntimeSafety,
        ToolDecisionSource::LifecycleClosure => {
            ToolApprovalDecisionSourceStorageKind::LifecycleClosure
        }
        ToolDecisionSource::UserOverride => ToolApprovalDecisionSourceStorageKind::UserOverride,
        ToolDecisionSource::SessionOverride | ToolDecisionSource::Delegate => {
            return Err(ModelCallRepositoryError::InvalidTransition(
                "unimplemented tool-decision source cannot be stored",
            ));
        }
    };
    Ok(tool_approval_decision_source_to_str(storage_kind))
}

async fn persist_cancelled(
    connection: &mut PgConnection,
    cancelled: &CancelledModelCallTurn,
    usage: ProviderReportedTokenUsage,
) -> Result<(), ModelCallRepositoryError> {
    if let Some(call) = cancelled.call() {
        persist_ended_call(
            connection,
            cancelled.session(),
            cancelled.turn(),
            call,
            usage,
        )
        .await?;
    }
    if let Some(attempt) = cancelled.attempt() {
        persist_ended_attempt(connection, cancelled.session(), cancelled.turn(), attempt).await?;
    }
    if !cancelled.tool_result_entries().is_empty() {
        crate::tool_loop::persist_result_entry_slice(connection, cancelled.tool_result_entries())
            .await
            .map_err(map_tool_evidence_error)?;
    }
    let entry = cancelled.cancellation_entry();
    if !matches!(
        entry.payload(),
        SemanticTranscriptEntryPayload::TurnCancelled { turn } if *turn == cancelled.turn()
    ) {
        return Err(ModelCallCorruption::Inconsistent("cancellation entry payload").into());
    }
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind, cancelled_turn_id)
         VALUES ($1, $2, 'turn_cancelled', $3)",
    )
    .bind(session_id_to_uuid(entry.source_session()))
    .bind(entry.identity().into_uuid())
    .bind(turn_id_to_uuid(cancelled.turn()))
    .execute(&mut *connection)
    .await?;
    insert_snapshot(connection, cancelled.terminal_snapshot()).await?;
    persist_reclassified_pending_steering(
        connection,
        cancelled.session(),
        cancelled.turn(),
        cancelled.reclassified_pending_steering(),
    )
    .await?;
    terminalize_lifecycle(
        connection,
        cancelled.session(),
        cancelled.turn(),
        "cancelled",
        TurnTerminalCause::InterruptApplied,
        cancelled.terminal_snapshot().frontier().snapshot(),
        cancelled
            .attempt()
            .map(signalbox_domain::EndedTurnAttempt::id),
        cancelled.call().map(signalbox_domain::EndedModelCall::id),
    )
    .await?;
    if let Some(call) = cancelled.call() {
        append_terminal_call_event(connection, cancelled.session(), cancelled.turn(), call).await?;
    }
    outbox::append(
        connection,
        OutboxEvent::TurnTerminal {
            session: cancelled.session(),
            turn: cancelled.turn(),
            disposition: TurnTerminalOutboxDisposition::Cancelled {
                cancellation_entry: entry.identity(),
                terminal_frontier: cancelled.terminal_snapshot().frontier().snapshot(),
            },
        },
    )
    .await?;
    Ok(())
}

async fn persist_completed(
    connection: &mut PgConnection,
    completed: &CompletedModelCallTurn,
    usage: ProviderReportedTokenUsage,
) -> Result<(), ModelCallRepositoryError> {
    persist_ended_call(
        connection,
        completed.session(),
        completed.turn(),
        completed.call(),
        usage,
    )
    .await?;
    persist_ended_attempt(
        connection,
        completed.session(),
        completed.turn(),
        completed.attempt(),
    )
    .await?;
    let mut response_text_start_bytes = 0_u64;
    for (response_part_ordinal, entry) in completed.assistant_entries().iter().enumerate() {
        let ordinal =
            Decimal::from(u64::try_from(response_part_ordinal).map_err(|_| {
                ModelCallCorruption::Inconsistent("completed response part ordinal")
            })?);
        match entry.payload() {
            SemanticTranscriptEntryPayload::AssistantText {
                producing_call,
                value,
            } => {
                sqlx::query(
                    "INSERT INTO semantic_transcript_entry
                        (source_session_id, semantic_entry_id, payload_kind,
                         assistant_text_value, producing_model_call_id,
                         assistant_response_part_ordinal,
                         assistant_response_text_start_bytes)
                     VALUES ($1, $2, 'assistant_text', $3, $4, $5, $6)",
                )
                .bind(session_id_to_uuid(entry.source_session()))
                .bind(entry.identity().into_uuid())
                .bind(value.as_str())
                .bind(producing_call.into_uuid())
                .bind(ordinal)
                .bind(Decimal::from(response_text_start_bytes))
                .execute(&mut *connection)
                .await?;
                response_text_start_bytes = response_text_start_bytes
                    .checked_add(u64::try_from(value.as_str().len()).map_err(|_| {
                        ModelCallCorruption::Inconsistent("completed response text byte length")
                    })?)
                    .ok_or(ModelCallCorruption::Inconsistent(
                        "completed response text byte position",
                    ))?;
            }
            SemanticTranscriptEntryPayload::ProviderCompaction {
                producing_call,
                block,
            } => {
                sqlx::query(
                    "INSERT INTO semantic_transcript_entry
                        (source_session_id, semantic_entry_id, payload_kind,
                         assistant_text_value, producing_model_call_id,
                         assistant_response_part_ordinal)
                     VALUES ($1, $2, 'provider_compaction', $3, $4, $5)",
                )
                .bind(session_id_to_uuid(entry.source_session()))
                .bind(entry.identity().into_uuid())
                .bind(block.as_json())
                .bind(producing_call.into_uuid())
                .bind(ordinal)
                .execute(&mut *connection)
                .await?;
            }
            _ => {
                return Err(
                    ModelCallCorruption::Inconsistent("completed assistant payload").into(),
                );
            }
        }
    }
    let completion = completed.completion_entry();
    if !matches!(
        completion.payload(),
        SemanticTranscriptEntryPayload::TurnCompleted { turn } if *turn == completed.turn()
    ) {
        return Err(ModelCallCorruption::Inconsistent("completion entry payload").into());
    }
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind, completed_turn_id)
         VALUES ($1, $2, 'turn_completed', $3)",
    )
    .bind(session_id_to_uuid(completion.source_session()))
    .bind(completion.identity().into_uuid())
    .bind(turn_id_to_uuid(completed.turn()))
    .execute(&mut *connection)
    .await?;
    insert_snapshot(connection, completed.terminal_snapshot()).await?;
    persist_reclassified_pending_steering(
        connection,
        completed.session(),
        completed.turn(),
        completed.reclassified_pending_steering(),
    )
    .await?;
    terminalize_lifecycle(
        connection,
        completed.session(),
        completed.turn(),
        "completed",
        TurnTerminalCause::Completed,
        completed.terminal_snapshot().frontier().snapshot(),
        Some(completed.attempt().id()),
        Some(completed.call().id()),
    )
    .await?;
    append_terminal_call_event(
        connection,
        completed.session(),
        completed.turn(),
        completed.call(),
    )
    .await?;
    outbox::append(
        connection,
        OutboxEvent::TurnTerminal {
            session: completed.session(),
            turn: completed.turn(),
            disposition: TurnTerminalOutboxDisposition::Completed {
                call: completed.call().id(),
                completion_entry: completion.identity(),
                terminal_frontier: completed.terminal_snapshot().frontier().snapshot(),
            },
        },
    )
    .await?;
    Ok(())
}

async fn persist_failed(
    connection: &mut PgConnection,
    failed: &FailedModelCallTurn,
    cause: TurnTerminalCause,
    usage: ProviderReportedTokenUsage,
    provider_failure_cause: Option<ProviderModelCallFailureCause>,
    attachment_failure: Option<AttachmentPreparationFailure>,
) -> Result<(), ModelCallRepositoryError> {
    if let Some(call) = failed.call() {
        persist_ended_call_with_provider_failure_cause(
            connection,
            failed.session(),
            failed.turn(),
            call,
            usage,
            provider_failure_cause,
            attachment_failure,
        )
        .await?;
    } else if attachment_failure.is_some() {
        return Err(ModelCallCorruption::Inconsistent(
            "attachment-preparation failure without a model call",
        )
        .into());
    }
    persist_ended_attempt(
        connection,
        failed.session(),
        failed.turn(),
        failed.attempt(),
    )
    .await?;
    let entry = failed.failure_entry();
    if !matches!(
        entry.payload(),
        SemanticTranscriptEntryPayload::TurnFailed { turn } if *turn == failed.turn()
    ) {
        return Err(ModelCallCorruption::Inconsistent("failure entry payload").into());
    }
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', $3)",
    )
    .bind(session_id_to_uuid(entry.source_session()))
    .bind(entry.identity().into_uuid())
    .bind(turn_id_to_uuid(failed.turn()))
    .execute(&mut *connection)
    .await?;
    insert_snapshot(connection, failed.terminal_snapshot()).await?;
    persist_reclassified_pending_steering(
        connection,
        failed.session(),
        failed.turn(),
        failed.reclassified_pending_steering(),
    )
    .await?;
    terminalize_lifecycle(
        connection,
        failed.session(),
        failed.turn(),
        "failed",
        cause,
        failed.terminal_snapshot().frontier().snapshot(),
        Some(failed.attempt().id()),
        failed.call().map(signalbox_domain::EndedModelCall::id),
    )
    .await?;
    if let Some(call) = failed.call() {
        append_terminal_call_event(connection, failed.session(), failed.turn(), call).await?;
    }
    outbox::append(
        connection,
        OutboxEvent::TurnTerminal {
            session: failed.session(),
            turn: failed.turn(),
            disposition: TurnTerminalOutboxDisposition::Failed {
                failure_entry: entry.identity(),
                terminal_frontier: failed.terminal_snapshot().frontier().snapshot(),
            },
        },
    )
    .await?;
    Ok(())
}

async fn persist_refused(
    connection: &mut PgConnection,
    refused: &RefusedModelCallTurn,
    usage: ProviderReportedTokenUsage,
) -> Result<(), ModelCallRepositoryError> {
    persist_ended_call(
        connection,
        refused.session(),
        refused.turn(),
        refused.call(),
        usage,
    )
    .await?;
    persist_ended_attempt(
        connection,
        refused.session(),
        refused.turn(),
        refused.attempt(),
    )
    .await?;
    insert_snapshot(connection, refused.terminal_snapshot()).await?;
    persist_reclassified_pending_steering(
        connection,
        refused.session(),
        refused.turn(),
        refused.reclassified_pending_steering(),
    )
    .await?;
    terminalize_lifecycle(
        connection,
        refused.session(),
        refused.turn(),
        "refused",
        TurnTerminalCause::ModelRefusal,
        refused.terminal_snapshot().frontier().snapshot(),
        Some(refused.attempt().id()),
        Some(refused.call().id()),
    )
    .await?;
    append_terminal_call_event(
        connection,
        refused.session(),
        refused.turn(),
        refused.call(),
    )
    .await?;
    outbox::append(
        connection,
        OutboxEvent::TurnTerminal {
            session: refused.session(),
            turn: refused.turn(),
            disposition: TurnTerminalOutboxDisposition::Refused {
                call: refused.call().id(),
                terminal_frontier: refused.terminal_snapshot().frontier().snapshot(),
            },
        },
    )
    .await?;
    Ok(())
}

/// Settles the injection receipt of one accepted input's command, when the
/// input was accepted by a command.
pub(crate) async fn settle_injection(
    connection: &mut PgConnection,
    session: SessionId,
    command: Option<Uuid>,
    outcome: InjectionOutcomeOutbox,
) -> Result<(), sqlx::Error> {
    let Some(command) = command else {
        return Ok(());
    };
    outbox::append(
        connection,
        OutboxEvent::InjectionSettled {
            session,
            command: DurableCommandId::from_uuid(command),
            outcome,
        },
    )
    .await
}

pub(crate) async fn persist_reclassified_pending_steering(
    connection: &mut PgConnection,
    session: SessionId,
    source_turn: TurnId,
    successors: &[ReclassifiedPendingSteeringTurn],
) -> Result<(), ModelCallRepositoryError> {
    if successors.is_empty() {
        return Ok(());
    }
    // An accepted-input source turn resolves to its configuration root through
    // the queue chain. A delegation-origin source turn has no
    // `queued_input_origin` row at all — it is its own configuration root, the
    // successor's configuration comes from
    // `turn_origin_exact_model_configuration` below, and a delegated turn
    // carries no resolved per-turn settings evidence to copy — so its successor
    // requires none. Exactly one arm below produces a row; a missing
    // accepted-input root stays a corruption.
    let model_settings_evidence_required: bool = sqlx::query_scalar(
        "WITH RECURSIVE configuration_chain AS (
            SELECT source.*
              FROM queued_input_origin AS source
             WHERE source.turn_id = $1
               AND source.session_id = $2
            UNION
            SELECT ancestor.*
              FROM configuration_chain AS current
              JOIN queued_input_origin AS ancestor
                ON ancestor.turn_id = current.source_configuration_turn_id
               AND ancestor.session_id = current.session_id
         )
         SELECT model_settings_evidence_required
           FROM configuration_chain
          WHERE source_configuration_turn_id IS NULL
         UNION ALL
         SELECT FALSE
           FROM turn_lifecycle AS lifecycle
          WHERE lifecycle.turn_id = $1
            AND lifecycle.session_id = $2
            AND lifecycle.origin_kind = 'delegation'",
    )
    .bind(turn_id_to_uuid(source_turn))
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ModelCallCorruption::Missing(
        "reclassified source configuration root",
    ))?;
    for successor in successors {
        let AcceptedInputDisposition::ReclassifiedAsTurnOrigin { turn, .. } =
            successor.accepted_input().disposition()
        else {
            return Err(ModelCallCorruption::Inconsistent(
                "reclassified accepted-input disposition",
            )
            .into());
        };
        if successor.session() != session
            || successor.source_turn() != source_turn
            || successor.binding().source_turn() != source_turn
            || *turn != successor.turn()
        {
            return Err(
                ModelCallCorruption::Inconsistent("reclassified successor correlation").into(),
            );
        }

        let command: Option<Option<Uuid>> = sqlx::query_scalar(
            "UPDATE accepted_input
                SET disposition_kind = 'reclassified_as_turn_origin',
                    origin_turn_id = $1
              WHERE accepted_input_id = $2
                AND session_id = $3
                AND acceptance_position = $4
                AND delivery_kind = 'next_safe_point'
                AND expected_active_turn_id = $5
                AND disposition_kind = 'pending_steering'
                AND origin_turn_id IS NULL
            RETURNING accepting_command_id",
        )
        .bind(turn_id_to_uuid(successor.turn()))
        .bind(successor.accepted_input().id().into_uuid())
        .bind(session_id_to_uuid(session))
        .bind(crate::mapping::input_position_to_numeric(
            successor.order().acceptance_position(),
        ))
        .bind(turn_id_to_uuid(source_turn))
        .fetch_optional(&mut *connection)
        .await?;
        let command = command.ok_or(ModelCallCorruption::Inconsistent(
            "pending-steering reclassification",
        ))?;
        settle_injection(
            connection,
            session,
            command,
            InjectionOutcomeOutbox::Delivered {
                turn: Some(successor.turn()),
            },
        )
        .await?;

        let (frozen_kind, frozen_direct, frozen_alias, frozen_alias_selected) =
            match successor.effective_configuration().model() {
                FrozenModelSelection::Direct(selection) => {
                    ("direct", Some(selection.into_uuid()), None, None)
                }
                FrozenModelSelection::FrozenAlias { alias, definition } => (
                    "frozen_alias",
                    None,
                    Some(alias.into_uuid()),
                    Some(definition.selected().into_uuid()),
                ),
            };
        let queue_rows = sqlx::query(
            "WITH exact_configuration AS (
                SELECT configuration.*
                  FROM turn_origin_exact_model_configuration($5, $3) AS configuration
             )
             INSERT INTO queued_input_origin
                (turn_id, accepted_input_id, session_id, acceptance_position,
                 priority_kind, source_configuration_turn_id, defaults_version,
                 requested_model_kind, requested_direct_model_selection_id,
                 requested_model_alias_id, frozen_model_kind,
                 frozen_direct_model_selection_id, frozen_model_alias_id,
                 frozen_alias_selected_direct_id, model_parameters,
                 known_provider_failure_retry, model_fallback,
                 dangerous_tool_auto_approval)
             SELECT
                $1, accepted.accepted_input_id, accepted.session_id,
                accepted.acceptance_position, 'ordinary', source.turn_id,
                CASE WHEN source.turn_id IS NULL THEN exact.defaults_version END,
                CASE WHEN source.turn_id IS NULL THEN exact.requested_model_kind END,
                CASE WHEN source.turn_id IS NULL
                     THEN exact.requested_direct_model_selection_id END,
                CASE WHEN source.turn_id IS NULL
                     THEN exact.requested_model_alias_id END,
                CASE WHEN source.turn_id IS NULL THEN exact.frozen_model_kind END,
                CASE WHEN source.turn_id IS NULL
                     THEN exact.frozen_direct_model_selection_id END,
                CASE WHEN source.turn_id IS NULL
                     THEN exact.frozen_model_alias_id END,
                CASE WHEN source.turn_id IS NULL
                     THEN exact.frozen_alias_selected_direct_id END,
                CASE WHEN source.turn_id IS NULL THEN 'provider_defaults' END,
                CASE WHEN source.turn_id IS NULL THEN 'disabled' END,
                CASE WHEN source.turn_id IS NULL THEN 'disabled' END,
                CASE WHEN source.turn_id IS NULL THEN $10 END
               FROM accepted_input AS accepted
               JOIN turn_lifecycle AS lifecycle
                 ON lifecycle.turn_id = $5
                AND lifecycle.session_id = accepted.session_id
               CROSS JOIN exact_configuration AS exact
               LEFT JOIN queued_input_origin AS source
                 ON source.turn_id = $5
                AND source.session_id = accepted.session_id
              WHERE accepted.accepted_input_id = $2
                AND accepted.session_id = $3
                AND accepted.acceptance_position = $4
                AND accepted.disposition_kind = 'reclassified_as_turn_origin'
                AND accepted.origin_turn_id = $1
                AND accepted.expected_active_turn_id = $5
                AND lifecycle.acceptance_position < accepted.acceptance_position
                AND (source.turn_id IS NOT NULL OR lifecycle.origin_kind = 'delegation')
                AND exact.frozen_model_kind = $6
                AND exact.frozen_direct_model_selection_id IS NOT DISTINCT FROM $7
                AND exact.frozen_model_alias_id IS NOT DISTINCT FROM $8
                AND exact.frozen_alias_selected_direct_id IS NOT DISTINCT FROM $9",
        )
        .bind(turn_id_to_uuid(successor.turn()))
        .bind(successor.accepted_input().id().into_uuid())
        .bind(session_id_to_uuid(session))
        .bind(crate::mapping::input_position_to_numeric(
            successor.order().acceptance_position(),
        ))
        .bind(turn_id_to_uuid(source_turn))
        .bind(frozen_kind)
        .bind(frozen_direct)
        .bind(frozen_alias)
        .bind(frozen_alias_selected)
        .bind(dangerous_tool_auto_approval_to_str(
            successor
                .effective_configuration()
                .dangerous_tool_auto_approval(),
        ))
        .execute(&mut *connection)
        .await?
        .rows_affected();
        require_single(queue_rows, "reclassified successor queue")?;

        let lifecycle_rows = sqlx::query(
            "INSERT INTO turn_lifecycle
                (turn_id, session_id, origin_accepted_input_id,
                 acceptance_position, state_kind)
             VALUES ($1, $2, $3, $4, 'queued')",
        )
        .bind(turn_id_to_uuid(successor.turn()))
        .bind(session_id_to_uuid(session))
        .bind(successor.accepted_input().id().into_uuid())
        .bind(crate::mapping::input_position_to_numeric(
            successor.order().acceptance_position(),
        ))
        .execute(&mut *connection)
        .await?
        .rows_affected();
        require_single(lifecycle_rows, "reclassified successor lifecycle")?;

        let settings_rows = sqlx::query(
            "INSERT INTO turn_model_settings_resolved
                (accepted_input_id, turn_id, session_id, defaults_version,
                 selected_direct_model_id, per_call_model_settings,
                 resolved_model_settings, adjusted_from_selection_id, adjustments)
             SELECT $2, $1, source.session_id, source.defaults_version,
                    source.selected_direct_model_id, source.per_call_model_settings,
                    source.resolved_model_settings, source.adjusted_from_selection_id,
                    source.adjustments
               FROM turn_model_settings_resolved AS source
              WHERE source.turn_id = $4
                AND source.session_id = $3",
        )
        .bind(turn_id_to_uuid(successor.turn()))
        .bind(successor.accepted_input().id().into_uuid())
        .bind(session_id_to_uuid(session))
        .bind(turn_id_to_uuid(source_turn))
        .execute(&mut *connection)
        .await?
        .rows_affected();
        if settings_rows > 1 {
            return Err(ModelCallRepositoryError::Corruption(
                ModelCallCorruption::Inconsistent("reclassified successor model settings"),
            ));
        }
        if settings_rows == 0 && model_settings_evidence_required {
            return Err(ModelCallCorruption::Missing("reclassified source model settings").into());
        }
        if settings_rows == 1 {
            outbox::append(
                connection,
                OutboxEvent::TurnModelSettingsResolved {
                    session,
                    accepted_input: successor.accepted_input().id(),
                },
            )
            .await?;
        }

        outbox::append(
            connection,
            OutboxEvent::InputAccepted {
                session,
                accepted_input: successor.accepted_input().id(),
                turn: successor.turn(),
                acceptance_position: successor.order().acceptance_position(),
            },
        )
        .await?;
    }
    Ok(())
}

async fn persist_ambiguous(
    connection: &mut PgConnection,
    ambiguous: &AmbiguousModelCallTurn,
    usage: ProviderReportedTokenUsage,
) -> Result<(), ModelCallRepositoryError> {
    persist_ended_call(
        connection,
        ambiguous.session(),
        ambiguous.turn(),
        ambiguous.call(),
        usage,
    )
    .await?;
    persist_ended_attempt(
        connection,
        ambiguous.session(),
        ambiguous.turn(),
        ambiguous.attempt(),
    )
    .await?;
    let rows = sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_model_call_recovery',
                recovery_model_call_id = $1
          WHERE turn_id = $2
            AND session_id = $3
            AND state_kind = 'active'
            AND active_phase_kind = 'running'
            AND current_attempt_id = $4",
    )
    .bind(ambiguous.call().id().into_uuid())
    .bind(turn_id_to_uuid(ambiguous.turn()))
    .bind(session_id_to_uuid(ambiguous.session()))
    .bind(ambiguous.attempt().id().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "ambiguous recovery lifecycle")?;
    append_terminal_call_event(
        connection,
        ambiguous.session(),
        ambiguous.turn(),
        ambiguous.call(),
    )
    .await?;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EncodedTokenUsage {
    input_tokens: Option<Decimal>,
    output_tokens: Option<Decimal>,
    cache_creation_input_tokens: Option<Decimal>,
    cache_read_input_tokens: Option<Decimal>,
}

#[derive(Debug)]
struct StoredModelCallObservation {
    session: Uuid,
    turn: Uuid,
    attempt: Uuid,
    target: Uuid,
    frontier: Uuid,
    state: String,
    disposition: Option<String>,
    provider_failure_cause: Option<String>,
    usage: EncodedTokenUsage,
}

fn decode_stored_model_call_observation(
    row: &PgRow,
) -> Result<StoredModelCallObservation, sqlx::Error> {
    Ok(StoredModelCallObservation {
        session: row.try_get("session_id")?,
        turn: row.try_get("turn_id")?,
        attempt: row.try_get("turn_attempt_id")?,
        target: row.try_get("resolved_provider_model_identity_id")?,
        frontier: row.try_get("context_frontier_id")?,
        state: row.try_get("state_kind")?,
        disposition: row.try_get("terminal_disposition_kind")?,
        provider_failure_cause: row.try_get("terminal_provider_failure_cause")?,
        usage: EncodedTokenUsage {
            input_tokens: row.try_get("usage_input_tokens")?,
            output_tokens: row.try_get("usage_output_tokens")?,
            cache_creation_input_tokens: row.try_get("usage_cache_creation_input_tokens")?,
            cache_read_input_tokens: row.try_get("usage_cache_read_input_tokens")?,
        },
    })
}

async fn persist_ended_call(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call: &signalbox_domain::EndedModelCall,
    usage: ProviderReportedTokenUsage,
) -> Result<(), ModelCallRepositoryError> {
    persist_ended_call_with_provider_failure_cause(
        connection, session, turn, call, usage, None, None,
    )
    .await
}

async fn persist_ended_call_with_provider_failure_cause(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call: &signalbox_domain::EndedModelCall,
    usage: ProviderReportedTokenUsage,
    provider_failure_cause: Option<ProviderModelCallFailureCause>,
    attachment_failure: Option<AttachmentPreparationFailure>,
) -> Result<(), ModelCallRepositoryError> {
    let usage = encode_token_usage(usage);
    let attachment_failure = attachment_failure
        .map(encode_attachment_preparation_failure)
        .transpose()?;
    let rows = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'terminal',
                terminal_disposition_kind = $1,
                usage_input_tokens = $2,
                usage_output_tokens = $3,
                usage_cache_creation_input_tokens = $4,
                usage_cache_read_input_tokens = $5,
                terminal_provider_failure_cause = $6,
                terminal_attachment_preparation_failure_cause = $7,
                terminal_attachment_preparation_failure_maximum_bytes = $8
          WHERE model_call_id = $9
            AND turn_id = $10
            AND session_id = $11
            AND turn_attempt_id = $12
            AND state_kind <> 'terminal'
            AND terminal_disposition_kind IS NULL",
    )
    .bind(encode_disposition(call.disposition()))
    .bind(usage.input_tokens)
    .bind(usage.output_tokens)
    .bind(usage.cache_creation_input_tokens)
    .bind(usage.cache_read_input_tokens)
    .bind(provider_failure_cause.map(encode_provider_failure_cause))
    .bind(attachment_failure.map(|(cause, _)| cause))
    .bind(attachment_failure.and_then(|(_, maximum_bytes)| maximum_bytes))
    .bind(call.id().into_uuid())
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .bind(call.attempt().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "terminal model call")
}

fn encode_token_usage(usage: ProviderReportedTokenUsage) -> EncodedTokenUsage {
    EncodedTokenUsage {
        input_tokens: usage.input_tokens().map(Decimal::from),
        output_tokens: usage.output_tokens().map(Decimal::from),
        cache_creation_input_tokens: usage.cache_creation_input_tokens().map(Decimal::from),
        cache_read_input_tokens: usage.cache_read_input_tokens().map(Decimal::from),
    }
}

async fn persist_ended_attempt(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    attempt: &signalbox_domain::EndedTurnAttempt,
) -> Result<(), ModelCallRepositoryError> {
    let (variant, disposition, interrupt_command, interrupt_predecessor) =
        encode_attempt_end(attempt.end())?;
    let rows = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = $1,
                end_disposition = $2,
                interrupt_command_id = COALESCE(interrupt_command_id, $3),
                interrupt_predecessor_turn_id =
                    COALESCE(interrupt_predecessor_turn_id, $4)
          WHERE turn_attempt_id = $5
            AND turn_id = $6
            AND session_id = $7
            AND (
                (
                    state_kind IN ('prepared', 'running', 'stop_requested')
                    AND end_variant IS NULL
                    AND end_disposition IS NULL
                )
            )
            AND (
                (
                    $3::uuid IS NULL
                    AND interrupt_command_id IS NULL
                    AND interrupt_predecessor_turn_id IS NULL
                )
                OR (
                    $3::uuid IS NOT NULL
                    AND (
                        interrupt_command_id IS NULL
                        OR interrupt_command_id = $3
                    )
                    AND (
                        interrupt_predecessor_turn_id IS NULL
                        OR interrupt_predecessor_turn_id = $4
                    )
                )
            )",
    )
    .bind(variant)
    .bind(disposition)
    .bind(interrupt_command)
    .bind(interrupt_predecessor)
    .bind(attempt.id().into_uuid())
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "terminal model-call attempt")
}

type EncodedAttemptEnd = (&'static str, &'static str, Option<Uuid>, Option<Uuid>);

fn encode_attempt_end(
    end: &signalbox_domain::AttemptEnd,
) -> Result<EncodedAttemptEnd, ModelCallRepositoryError> {
    match end {
        signalbox_domain::AttemptEnd::WithoutStop { disposition } => {
            let disposition = match disposition {
                signalbox_domain::UnstoppedAttemptDisposition::TurnCompleted => "turn_completed",
                signalbox_domain::UnstoppedAttemptDisposition::TurnRefused => "turn_refused",
                signalbox_domain::UnstoppedAttemptDisposition::YieldedToDurableWait => {
                    "yielded_to_durable_wait"
                }
                signalbox_domain::UnstoppedAttemptDisposition::KnownFailure => "known_failure",
                signalbox_domain::UnstoppedAttemptDisposition::Lost => "lost",
                signalbox_domain::UnstoppedAttemptDisposition::Ambiguous => "ambiguous",
            };
            Ok(("without_stop", disposition, None, None))
        }
        signalbox_domain::AttemptEnd::AfterCancellation { cause, disposition } => {
            let disposition = match disposition {
                signalbox_domain::CancellationStopDisposition::TurnCompleted => "turn_completed",
                signalbox_domain::CancellationStopDisposition::TurnRefused => "turn_refused",
                signalbox_domain::CancellationStopDisposition::KnownFailure => "known_failure",
                signalbox_domain::CancellationStopDisposition::Lost => "lost",
                signalbox_domain::CancellationStopDisposition::Cancelled => "cancelled",
                signalbox_domain::CancellationStopDisposition::Ambiguous => "ambiguous",
            };
            Ok((
                "after_cancellation",
                disposition,
                Some(durable_command_id_to_uuid(cause.command())),
                Some(turn_id_to_uuid(cause.predecessor())),
            ))
        }
        signalbox_domain::AttemptEnd::AfterFatalMismatch { .. } => {
            Err(ModelCallRepositoryError::InvalidTransition(
                "initial model execution cannot persist fatal-mismatch attempt history",
            ))
        }
    }
}

pub(crate) struct SnapshotAppend<I> {
    pub(crate) owning_session: SessionId,
    pub(crate) frontier: signalbox_domain::ContextFrontierId,
    pub(crate) prefix: Option<signalbox_domain::ContextFrontierId>,
    pub(crate) member_count: u64,
    pub(crate) prefix_member_count: u64,
    pub(crate) appended_entries: I,
}

pub(crate) enum SnapshotAppendError {
    FrontierInsert(sqlx::Error),
    MemberInsert(sqlx::Error),
    MemberPositionOverflow,
}

pub(crate) async fn insert_snapshot_append<I>(
    connection: &mut PgConnection,
    append: SnapshotAppend<I>,
) -> Result<(), SnapshotAppendError>
where
    I: IntoIterator<Item = SemanticTranscriptEntryRef>,
{
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id,
             prefix_context_frontier_id, member_count)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(session_id_to_uuid(append.owning_session))
    .bind(append.frontier.into_uuid())
    .bind(
        append
            .prefix
            .map(signalbox_domain::ContextFrontierId::into_uuid),
    )
    .bind(Decimal::from(append.member_count))
    .execute(&mut *connection)
    .await
    .map_err(SnapshotAppendError::FrontierInsert)?;
    for (index, entry) in append.appended_entries.into_iter().enumerate() {
        let index =
            u64::try_from(index).map_err(|_| SnapshotAppendError::MemberPositionOverflow)?;
        let position = append
            .prefix_member_count
            .checked_add(index)
            .and_then(|index| index.checked_add(1))
            .ok_or(SnapshotAppendError::MemberPositionOverflow)?;
        sqlx::query(
            "INSERT INTO context_frontier_delta
                (owning_session_id, context_frontier_id, member_position,
                 source_session_id, semantic_entry_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(session_id_to_uuid(append.owning_session))
        .bind(append.frontier.into_uuid())
        .bind(Decimal::from(position))
        .bind(session_id_to_uuid(entry.source_session()))
        .bind(entry.entry().into_uuid())
        .execute(&mut *connection)
        .await
        .map_err(SnapshotAppendError::MemberInsert)?;
    }
    Ok(())
}

pub(crate) async fn insert_snapshot(
    connection: &mut PgConnection,
    snapshot: &signalbox_domain::ResolvedContextFrontierSnapshot,
) -> Result<(), ModelCallRepositoryError> {
    let member_count = u64::try_from(snapshot.entry_count())
        .map_err(|_| ModelCallCorruption::Inconsistent("frontier member count"))?;
    let appended_entry_count = snapshot.appended_entries().len();
    let prefix_member_count = snapshot
        .entry_count()
        .checked_sub(appended_entry_count)
        .and_then(|count| u64::try_from(count).ok())
        .ok_or(ModelCallCorruption::Inconsistent(
            "frontier prefix member count",
        ))?;
    insert_snapshot_append(
        connection,
        SnapshotAppend {
            owning_session: snapshot.frontier().owning_session(),
            frontier: snapshot.frontier().snapshot(),
            prefix: snapshot
                .immediate_semantic_prefix()
                .map(|prefix| prefix.snapshot()),
            member_count,
            prefix_member_count,
            appended_entries: snapshot.appended_entries(),
        },
    )
    .await
    .map_err(|error| match error {
        SnapshotAppendError::FrontierInsert(error) | SnapshotAppendError::MemberInsert(error) => {
            error.into()
        }
        SnapshotAppendError::MemberPositionOverflow => {
            ModelCallCorruption::Inconsistent("frontier member position").into()
        }
    })
}

/// Classifies one pre-send prepared-call failure as a turn-terminal cause.
///
/// Attachment preparation is the more specific evidence: when it produced the
/// failure it names the cause, and the application's pre-send vocabulary names
/// it otherwise. Taking that vocabulary rather than a bare terminal cause is
/// what keeps a caller from pairing a `failed` disposition with a cause that
/// contradicts it.
const fn prepared_failure_cause(
    cause: PreparedModelCallFailureCause,
    attachment_failure: Option<AttachmentPreparationFailure>,
) -> TurnTerminalCause {
    match (attachment_failure, cause) {
        (Some(_), _) => TurnTerminalCause::AttachmentPreparationFailed,
        (None, PreparedModelCallFailureCause::CapabilityKnownFailure) => {
            TurnTerminalCause::CapabilityPreparationFailed
        }
        (None, PreparedModelCallFailureCause::ToolRoundLimitReached) => {
            TurnTerminalCause::ToolRoundLimitReached
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn terminalize_lifecycle(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    disposition: &'static str,
    cause: TurnTerminalCause,
    terminal_frontier: signalbox_domain::ContextFrontierId,
    terminal_attempt: Option<signalbox_domain::TurnAttemptId>,
    terminal_call: Option<ModelCallId>,
) -> Result<(), ModelCallRepositoryError> {
    let runner_recovery_terminal_attempt: Option<Uuid> = sqlx::query_scalar(
        "SELECT yielded_turn_attempt_id
           FROM turn_runner_recovery_interrupt_effect
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?;
    let terminal_attempt = terminal_attempt
        .map(signalbox_domain::TurnAttemptId::into_uuid)
        .or(runner_recovery_terminal_attempt);
    let rows = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                terminal_frontier_id = $1,
                active_phase_kind = NULL,
                current_attempt_id = NULL,
                recovery_model_call_id = NULL,
                active_tool_round_call_id = NULL,
                approval_tool_request_id = NULL,
                child_wait_request_id = NULL,
                recovery_tool_attempt_id = NULL,
                runner_recovery_runner_id = NULL,
                runner_recovery_placement_revision = NULL,
                runner_recovery_tool_attempt_id = NULL,
                terminal_attempt_id = $2,
                terminal_model_call_id = $3,
                terminal_tool_attempt_id = NULL,
                terminal_disposition_kind = $4,
                terminal_cause_kind = $7
          WHERE turn_id = $5
            AND session_id = $6
            AND state_kind = 'active'
            AND (
                (
                    active_phase_kind = 'running'
                    AND recovery_model_call_id IS NULL
                )
                OR (
                    $4 = 'reconciliation_required'
                    AND active_phase_kind = 'awaiting_model_call_recovery'
                    AND recovery_model_call_id = $3
                )
                OR (
                    $4 = 'cancelled'
                    AND $2::uuid IS NULL
                    AND $3::uuid IS NULL
                    AND active_phase_kind = 'awaiting_child'
                    AND child_wait_request_id IS NOT NULL
                )
                OR (
                    $4 = 'cancelled'
                    AND $3::uuid IS NULL
                    AND active_phase_kind = 'awaiting_runner_recovery'
                    AND runner_recovery_runner_id IS NOT NULL
                    AND runner_recovery_placement_revision IS NOT NULL
                    AND EXISTS (
                        SELECT 1
                          FROM turn_runner_recovery_interrupt_effect AS effect
                         WHERE effect.session_id = turn_lifecycle.session_id
                           AND effect.turn_id = turn_lifecycle.turn_id
                           AND effect.yielded_turn_attempt_id = $2
                    )
                )
            )",
    )
    .bind(terminal_frontier.into_uuid())
    .bind(terminal_attempt)
    .bind(terminal_call.map(ModelCallId::into_uuid))
    .bind(disposition)
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .bind(turn_terminal_cause_to_str(cause))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "terminal model-call lifecycle")?;
    Ok(())
}

async fn append_terminal_call_event(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call: &signalbox_domain::EndedModelCall,
) -> Result<(), ModelCallRepositoryError> {
    outbox::append(
        connection,
        OutboxEvent::ModelCallTransition {
            session,
            turn,
            call: call.id(),
            state: ModelCallOutboxState::Terminal(call.disposition()),
        },
    )
    .await?;
    Ok(())
}

fn encode_selection(
    selection: FrozenModelSelection,
) -> (&'static str, Option<Uuid>, Option<Uuid>, Option<Uuid>) {
    match selection {
        FrozenModelSelection::Direct(direct) => ("direct", Some(direct.into_uuid()), None, None),
        FrozenModelSelection::FrozenAlias { alias, definition } => (
            "frozen_alias",
            None,
            Some(alias.into_uuid()),
            Some(definition.selected().into_uuid()),
        ),
    }
}

fn encode_provider_failure_cause(cause: ProviderModelCallFailureCause) -> &'static str {
    match cause {
        ProviderModelCallFailureCause::CredentialRejected => "credential_rejected",
        ProviderModelCallFailureCause::PermissionDenied => "permission_denied",
        ProviderModelCallFailureCause::InvalidRequest => "invalid_request",
        ProviderModelCallFailureCause::TargetNotFound => "target_not_found",
        ProviderModelCallFailureCause::RequestTooLarge => "request_too_large",
        ProviderModelCallFailureCause::RateLimited => "rate_limited",
        ProviderModelCallFailureCause::QuotaExhausted => "quota_exhausted",
        ProviderModelCallFailureCause::Overloaded => "overloaded",
        ProviderModelCallFailureCause::ProviderInternal => "provider_internal",
        ProviderModelCallFailureCause::Unrecognized => "unrecognized",
    }
}

fn encode_disposition(disposition: ModelCallDisposition) -> &'static str {
    match disposition {
        ModelCallDisposition::Completed => "completed",
        ModelCallDisposition::KnownFailed => "known_failed",
        ModelCallDisposition::Refused => "refused",
        ModelCallDisposition::Cancelled => "cancelled",
        ModelCallDisposition::Ambiguous => "ambiguous",
    }
}

fn require_single(rows: u64, relationship: &'static str) -> Result<(), ModelCallRepositoryError> {
    if rows == 1 {
        Ok(())
    } else {
        Err(ModelCallCorruption::Inconsistent(relationship).into())
    }
}

async fn finish_commit<T>(
    transaction: sqlx::Transaction<'_, sqlx::Postgres>,
    result: Result<T, ModelCallRepositoryError>,
) -> Result<T, ModelCallRepositoryError> {
    match result {
        Ok(value) => {
            transaction.commit().await.map_err(|error| {
                let commit_ambiguous = commit_failure_is_ambiguous(&error);
                ModelCallRepositoryError::from_database(error, commit_ambiguous)
            })?;
            Ok(value)
        }
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
        }
    }
}

async fn finish_optional_commit<T>(
    transaction: sqlx::Transaction<'_, sqlx::Postgres>,
    result: Result<(bool, T), ModelCallRepositoryError>,
) -> Result<T, ModelCallRepositoryError> {
    match result {
        Ok((true, value)) => {
            transaction.commit().await.map_err(|error| {
                let commit_ambiguous = commit_failure_is_ambiguous(&error);
                ModelCallRepositoryError::from_database(error, commit_ambiguous)
            })?;
            Ok(value)
        }
        Ok((false, value)) => {
            transaction.rollback().await?;
            Ok(value)
        }
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
        }
    }
}

/// Measures one uncommitted preview entry the way the durable read measures
/// its committed equivalent.
///
/// Each arm mirrors the payload-kind term `latest_reported_usage` applies to a
/// committed member: accepted input sums its text parts and leaves attachment
/// stubs to their own accounting, and delegated material carries the exact
/// delivered content. Kinds a preview never mints contribute nothing.
fn preview_entry_content_bytes(
    entry: &SemanticTranscriptEntry,
    origin_contents: &[ModelCallOriginContent],
) -> u64 {
    match entry.payload() {
        SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
        | SemanticTranscriptEntryPayload::SteeringAcceptedInput { accepted_input, .. } => {
            origin_contents
                .iter()
                .find(|origin| origin.accepted_input() == *accepted_input)
                .map_or(0, |origin| accepted_input_text_bytes(origin.content()))
        }
        SemanticTranscriptEntryPayload::DelegatedTask { content, .. }
        | SemanticTranscriptEntryPayload::DelegationMessage { content, .. } => {
            utf8_byte_length(content.as_str())
        }
        SemanticTranscriptEntryPayload::DelegationResult { outcome, .. } => outcome
            .content()
            .map_or(0, |content| utf8_byte_length(content.as_str())),
        SemanticTranscriptEntryPayload::Imported { .. }
        | SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
        | SemanticTranscriptEntryPayload::ContextSummary { .. }
        | SemanticTranscriptEntryPayload::TurnFailed { .. }
        | SemanticTranscriptEntryPayload::AssistantText { .. }
        | SemanticTranscriptEntryPayload::ProviderCompaction { .. }
        | SemanticTranscriptEntryPayload::AssistantToolUse { .. }
        | SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
        | SemanticTranscriptEntryPayload::ToolDenied { .. }
        | SemanticTranscriptEntryPayload::ToolClosed { .. }
        | SemanticTranscriptEntryPayload::TurnCompleted { .. }
        | SemanticTranscriptEntryPayload::TurnCancelled { .. } => 0,
    }
}

/// Sums the text parts of one accepted input, as `octet_length` does durably.
fn accepted_input_text_bytes(content: &UserContent) -> u64 {
    content
        .parts()
        .iter()
        .fold(0_u64, |total, part| match part {
            signalbox_domain::UserContentPart::Text { value } => {
                total.saturating_add(utf8_byte_length(value.as_str()))
            }
            signalbox_domain::UserContentPart::Attachment { .. } => total,
        })
}

fn utf8_byte_length(value: &str) -> u64 {
    u64::try_from(value.len()).unwrap_or(u64::MAX)
}

fn map_projected_membership_error(
    error: crate::context_compaction::ContextCompactionRepositoryError,
) -> ModelCallRepositoryError {
    use crate::context_compaction::ContextCompactionRepositoryError as ProjectionError;
    match error {
        ProjectionError::Database(error) => error.into(),
        ProjectionError::CommitAmbiguous(error) => {
            ModelCallRepositoryError::from_database(error, true)
        }
        ProjectionError::IdentityCollision | ProjectionError::Corruption(_) => {
            ModelCallCorruption::Inconsistent("projected prospective frontier membership").into()
        }
    }
}

fn map_scheduling_error(error: SubmitInputRepositoryError) -> ModelCallRepositoryError {
    match error {
        SubmitInputRepositoryError::Database(error) => error.into(),
        SubmitInputRepositoryError::CommitAmbiguous(error) => {
            ModelCallRepositoryError::from_database(error, true)
        }
        SubmitInputRepositoryError::Corruption(error) => {
            ModelCallCorruption::Scheduling(error).into()
        }
        SubmitInputRepositoryError::DifferentCommandKind { .. } => {
            ModelCallCorruption::Inconsistent("origin command kind").into()
        }
        SubmitInputRepositoryError::AcceptedInputIdentityCollision { .. } => {
            ModelCallCorruption::Inconsistent("origin accepted-input identity").into()
        }
        SubmitInputRepositoryError::UnsupportedModelSetting(_) => {
            ModelCallCorruption::Inconsistent("origin model settings").into()
        }
        SubmitInputRepositoryError::ModelExecution(_) => {
            ModelCallCorruption::Inconsistent("origin command application").into()
        }
    }
}

fn identity_collision(error: &sqlx::Error) -> Option<ModelCallIdentityCollision> {
    match error
        .as_database_error()
        .and_then(|database| database.constraint())
    {
        Some("model_call_pkey" | "model_call_identity_pkey") => {
            Some(ModelCallIdentityCollision::ModelCall)
        }
        Some("semantic_transcript_entry_pk" | "semantic_transcript_entry_id_global") => {
            Some(ModelCallIdentityCollision::SemanticEntry)
        }
        Some("context_frontier_pk" | "context_frontier_id_global") => {
            Some(ModelCallIdentityCollision::TerminalFrontier)
        }
        Some(
            "accepted_input_origin_turn_id_key"
            | "queued_input_origin_pkey"
            | "turn_lifecycle_pkey",
        ) => Some(ModelCallIdentityCollision::ReclassifiedTurn),
        _ => None,
    }
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, ModelCallRepositoryError>
where
    for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or_else(|| ModelCallCorruption::Missing(field).into())
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, collections::BTreeSet, error::Error, fmt, io, time::Duration};

    use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
    use signalbox_domain::{ModelCallId, ProviderModelCallFailureCause, TurnId};
    use sqlx::{
        error::{DatabaseError, ErrorKind},
        types::Uuid,
    };

    use super::{
        MAX_AVAILABILITY_BACKOFF, ModelCallCorruption, ModelCallIdentityCollision,
        ModelCallRepositoryError, StoredTerminalFrontierMember, availability_retry_backoff,
        cancellation_poll_interval, commit_failure_is_ambiguous,
        completed_terminal_frontier_matches, delegation_terminal_relation_decode_error,
        failed_terminal_frontier_matches, is_same_credential_retry_cause,
        record_reclassified_turn_candidate,
    };

    #[test]
    fn same_credential_retry_causes_are_closed() {
        for cause in [
            ProviderModelCallFailureCause::RateLimited,
            ProviderModelCallFailureCause::Overloaded,
            ProviderModelCallFailureCause::ProviderInternal,
        ] {
            assert!(is_same_credential_retry_cause(cause));
        }
        for cause in [
            ProviderModelCallFailureCause::CredentialRejected,
            ProviderModelCallFailureCause::PermissionDenied,
            ProviderModelCallFailureCause::InvalidRequest,
            ProviderModelCallFailureCause::TargetNotFound,
            ProviderModelCallFailureCause::RequestTooLarge,
            ProviderModelCallFailureCause::QuotaExhausted,
            ProviderModelCallFailureCause::Unrecognized,
        ] {
            assert!(!is_same_credential_retry_cause(cause));
        }
    }

    #[test]
    fn rate_limit_backoff_is_jittered_inside_the_exponential_window() {
        let delay = availability_retry_backoff(
            ProviderModelCallFailureCause::RateLimited,
            None,
            3,
            ModelCallId::from_uuid(Uuid::from_u128(17)),
        );
        assert!(delay >= Duration::from_secs(2));
        assert!(delay < Duration::from_secs(6));
    }

    #[test]
    fn provider_retry_after_is_a_minimum_until_the_cap() {
        let delay = availability_retry_backoff(
            ProviderModelCallFailureCause::Overloaded,
            Some(Duration::from_secs(47)),
            1,
            ModelCallId::from_uuid(Uuid::from_u128(18)),
        );
        assert_eq!(delay, Duration::from_secs(47));
    }

    #[test]
    fn provider_retry_after_is_capped_and_quota_rotation_is_immediate() {
        let capped = availability_retry_backoff(
            ProviderModelCallFailureCause::RateLimited,
            Some(Duration::from_secs(600)),
            1,
            ModelCallId::from_uuid(Uuid::from_u128(19)),
        );
        assert_eq!(capped, MAX_AVAILABILITY_BACKOFF);
        let quota = availability_retry_backoff(
            ProviderModelCallFailureCause::QuotaExhausted,
            Some(Duration::from_secs(15)),
            1,
            ModelCallId::from_uuid(Uuid::from_u128(20)),
        );
        assert_eq!(quota, Duration::ZERO);
    }

    #[test]
    fn delegated_terminal_relation_decode_failure_is_corruption() {
        let error = sqlx::Error::ColumnDecode {
            index: String::from("parent_session_id"),
            source: Box::new(io::Error::new(
                io::ErrorKind::InvalidData,
                "fixture UUID decode failure",
            )),
        };

        assert!(matches!(
            delegation_terminal_relation_decode_error(error),
            ModelCallRepositoryError::Corruption(ModelCallCorruption::Inconsistent(
                "delegated terminal relationship identity"
            ))
        ));
    }

    #[tokio::test]
    async fn cancellation_polling_delays_missed_ticks() {
        assert_eq!(
            cancellation_poll_interval().missed_tick_behavior(),
            tokio::time::MissedTickBehavior::Delay
        );
    }

    /// docs/spec/model-call-execution.md: a source-turn successor candidate is
    /// a retryable minted-ID collision, not a caller transition defect.
    #[test]
    fn generated_successor_source_candidate_is_a_retryable_collision() {
        let source = TurnId::from_uuid(Uuid::from_u128(1));
        let mut proposed = BTreeSet::new();

        assert!(matches!(
            record_reclassified_turn_candidate(source, source, &mut proposed),
            Err(ModelCallRepositoryError::IdentityCollision(
                ModelCallIdentityCollision::ReclassifiedTurn
            ))
        ));
    }

    /// docs/spec/model-call-execution.md: a duplicate successor candidate is a
    /// retryable minted-ID collision, not a caller transition defect.
    #[test]
    fn generated_successor_duplicate_is_a_retryable_collision() {
        let source = TurnId::from_uuid(Uuid::from_u128(1));
        let successor = TurnId::from_uuid(Uuid::from_u128(2));
        let mut proposed = BTreeSet::new();

        record_reclassified_turn_candidate(source, successor, &mut proposed)
            .expect("the first source-safe successor is accepted");
        assert!(matches!(
            record_reclassified_turn_candidate(source, successor, &mut proposed),
            Err(ModelCallRepositoryError::IdentityCollision(
                ModelCallIdentityCollision::ReclassifiedTurn
            ))
        ));
    }
    #[derive(Debug)]
    struct ServerCommitFailure {
        code: &'static str,
    }

    impl fmt::Display for ServerCommitFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("server reported commit failure")
        }
    }

    impl Error for ServerCommitFailure {}

    impl DatabaseError for ServerCommitFailure {
        fn message(&self) -> &str {
            "server reported commit failure"
        }

        fn as_error(&self) -> &(dyn Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn Error + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }

        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.code))
        }
    }

    #[test]
    fn lost_commit_response_is_commit_ambiguous() {
        let error = sqlx::Error::Io(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "commit response was lost",
        ));
        let commit_ambiguous = commit_failure_is_ambiguous(&error);

        assert!(commit_ambiguous);
        assert_eq!(
            ModelCallRepositoryError::from_database(error, commit_ambiguous)
                .operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true
            }
        );
    }

    #[test]
    fn server_rejected_commit_is_not_ambiguous() {
        let error = sqlx::Error::Database(Box::new(ServerCommitFailure { code: "23514" }));
        let commit_ambiguous = commit_failure_is_ambiguous(&error);

        assert!(!commit_ambiguous);
        assert_eq!(
            ModelCallRepositoryError::from_database(error, commit_ambiguous)
                .operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false
            }
        );
    }

    #[test]
    fn server_reported_unknown_commit_outcomes_are_ambiguous() {
        let transaction_resolution_unknown =
            sqlx::Error::Database(Box::new(ServerCommitFailure { code: "08007" }));
        assert!(commit_failure_is_ambiguous(&transaction_resolution_unknown));

        let statement_completion_unknown =
            sqlx::Error::Database(Box::new(ServerCommitFailure { code: "40003" }));
        assert!(commit_failure_is_ambiguous(&statement_completion_unknown));
    }

    /// docs/spec/model-call-execution.md: a retained completed observation is
    /// present only when the terminal frontier is the exact source prefix,
    /// assistant sequence, and final `TurnCompleted` marker.
    #[test]
    fn completed_reread_requires_exact_terminal_frontier_shape() {
        let session = Uuid::from_u128(1);
        let turn = Uuid::from_u128(2);
        let call = Uuid::from_u128(3);
        let source = vec![(Uuid::from_u128(4), Uuid::from_u128(5))];
        let assistant = vec![signalbox_domain::AssistantResponsePart::Text(
            signalbox_domain::AssistantText::try_new(String::from("exact reply"))
                .expect("fixture text is admitted"),
        )];
        let prefix = StoredTerminalFrontierMember {
            source_session: source[0].0,
            entry: source[0].1,
            payload_kind: String::from("origin_accepted_input"),
            assistant_text: None,
            producing_call: None,
            completed_turn: None,
            failed_turn: None,
            cancelled_turn: None,
        };
        let assistant_member = StoredTerminalFrontierMember {
            source_session: session,
            entry: Uuid::from_u128(6),
            payload_kind: String::from("assistant_text"),
            assistant_text: Some(String::from("exact reply")),
            producing_call: Some(call),
            completed_turn: None,
            failed_turn: None,
            cancelled_turn: None,
        };
        let completion = StoredTerminalFrontierMember {
            source_session: session,
            entry: Uuid::from_u128(7),
            payload_kind: String::from("turn_completed"),
            assistant_text: None,
            producing_call: None,
            completed_turn: Some(turn),
            failed_turn: None,
            cancelled_turn: None,
        };
        let exact = vec![prefix.clone(), assistant_member.clone(), completion.clone()];
        assert!(completed_terminal_frontier_matches(
            &source, &exact, session, turn, call, &assistant,
        ));

        assert!(!completed_terminal_frontier_matches(
            &source,
            &[prefix.clone(), assistant_member.clone()],
            session,
            turn,
            call,
            &assistant,
        ));
        let mut extra = exact.clone();
        extra.insert(1, prefix.clone());
        assert!(!completed_terminal_frontier_matches(
            &source, &extra, session, turn, call, &assistant,
        ));
        let mut wrong_marker = completion;
        wrong_marker.completed_turn = Some(Uuid::from_u128(8));
        assert!(!completed_terminal_frontier_matches(
            &source,
            &[prefix, assistant_member, wrong_marker],
            session,
            turn,
            call,
            &assistant,
        ));
    }

    /// docs/spec/model-call-execution.md: a retained failed observation is
    /// present only when its terminal frontier is the exact source prefix
    /// plus one matching failure marker.
    #[test]
    fn failed_reread_requires_exact_terminal_frontier_shape() {
        let session = Uuid::from_u128(1);
        let turn = Uuid::from_u128(2);
        let source = vec![(Uuid::from_u128(3), Uuid::from_u128(4))];
        let prefix = StoredTerminalFrontierMember {
            source_session: source[0].0,
            entry: source[0].1,
            payload_kind: String::from("origin_accepted_input"),
            assistant_text: None,
            producing_call: None,
            completed_turn: None,
            failed_turn: None,
            cancelled_turn: None,
        };
        let failure = StoredTerminalFrontierMember {
            source_session: session,
            entry: Uuid::from_u128(5),
            payload_kind: String::from("turn_failed"),
            assistant_text: None,
            producing_call: None,
            completed_turn: None,
            failed_turn: Some(turn),
            cancelled_turn: None,
        };
        assert!(failed_terminal_frontier_matches(
            &source,
            &[prefix.clone(), failure.clone()],
            session,
            turn,
        ));

        let mut wrong_failure = failure;
        wrong_failure.failed_turn = Some(Uuid::from_u128(6));
        assert!(!failed_terminal_frontier_matches(
            &source,
            &[prefix.clone(), wrong_failure],
            session,
            turn,
        ));
        assert!(!failed_terminal_frontier_matches(
            &source,
            &[prefix],
            session,
            turn,
        ));
    }
}
