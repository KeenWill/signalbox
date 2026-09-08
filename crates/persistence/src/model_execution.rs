//! PostgreSQL transactions surrounding the first text-only model call.
//!
//! The three transaction roles in docs/spec/model-call-execution.md stay
//! explicit here: a durable `Prepared` checkpoint, a separate
//! send-authorization commit, and a fresh post-effect observation commit. No
//! method holds a database transaction across provider work.

mod continuation;
mod credential_pool;
#[path = "credential_pool_records.rs"]
mod credential_pool_records;
mod delegated_result;
mod delegation_lock;
mod live_turn;
mod load;
mod persist_disposition;
mod persist_terminal;
mod persist_tool_round;
mod prepared;
mod repository;
mod repository_prepare;
mod repository_reread;
mod reread;
mod transaction_impls;

pub(crate) use credential_pool::acquire_model_call_outbox_order_guard;
pub(crate) use credential_pool::prepared_serving_evidence;
pub(crate) use delegation_lock::lock_delegated_child_endpoint_sessions;
pub(crate) use delegation_lock::lock_delegated_turn_terminal_frontier;
pub(crate) use live_turn::load_call_snapshot;
pub(crate) use live_turn::load_delegated_model_call_recovery;
pub(crate) use live_turn::load_delegated_runner_recovery_for_interrupt;
pub(crate) use live_turn::lock_session;
pub(crate) use live_turn::require_live_execution_for_restart;
pub(crate) use persist_terminal::persist_automatic_reconciliation;
pub(crate) use persist_terminal::persist_stop_requested;
pub(crate) use persist_terminal::persist_terminal_outcome;
pub(crate) use persist_terminal::persist_tool_reconciliation_required;

pub(crate) use prepared::insert_prepared_call;

pub(crate) use persist_disposition::SnapshotAppend;
pub(crate) use persist_disposition::SnapshotAppendError;
pub(crate) use persist_disposition::insert_snapshot;
pub(crate) use persist_disposition::insert_snapshot_append;
pub(crate) use persist_disposition::persist_reclassified_pending_steering;

pub(crate) use continuation::fail_tool_crash_in_transaction;
pub(crate) use continuation::load_tool_continuation_execution;
pub(crate) use continuation::prepare_tool_continuation_call;
pub(crate) use continuation::resolve_session_credential;

pub(crate) use reread::attach_interrupt_reclassification_candidates;
pub(crate) use reread::attach_interrupt_reclassification_candidates_for_activated;
pub(crate) use reread::attach_interrupt_reclassification_candidates_for_active;
pub(crate) use reread::attach_recovery_interrupt_reclassification_candidates;
pub(crate) use reread::attach_recovery_interrupt_reclassification_candidates_for_activated;

pub(crate) use load::authenticate_model_call_instruction_manifest;

use std::{
    collections::{HashMap, HashSet},
    num::{NonZeroU32, NonZeroUsize},
    sync::Arc,
};

use signalbox_application::ProviderReasoningProvenance;
use signalbox_application::{
    AttachmentPreparationFailure, ClassifyOperatorFailure, ModelCallCredentialReference,
    OperatorFailureClass, PreparedModelCallFailureCause, ResolvedToolConversationEntry,
};
use signalbox_domain::{
    ContextFrontierId, FastMode, FrozenModelSelection, ModelCallDisposition,
    ModelCallExecutionReconstitutionFailure, ModelCallId, ModelCallOriginContent,
    ModelTargetCatalog, PreparedModelCallRequest, ProviderModelCallFailureCause,
    ProviderReportedTokenUsage, ResolvedProviderTarget, SemanticTranscriptEntry,
    SemanticTranscriptEntryPayload, SemanticTranscriptEntryRef, SessionId, TurnId,
    TurnTerminalCause, UserContent,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    commit_failure_is_ambiguous,
    mapping::{session_id_to_uuid, turn_id_to_uuid, turn_terminal_cause_to_str},
    outbox::{self, ModelCallOutboxState, OutboxEvent},
    session::SessionCorruption,
    submit_input::{SubmitInputCorruption, SubmitInputRepositoryError},
};

/// Immutable usage boundary for one resolved continuation mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolContinuationUsageLimit {
    target: ResolvedProviderTarget,
    fast_mode: FastMode,
    max_output_tokens: u64,
    context_window_tokens: u64,
    replays_provider_compaction: bool,
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
            replays_provider_compaction: false,
        }
    }

    /// Marks that this resolved target replays durable provider compaction.
    #[must_use]
    pub const fn with_provider_compaction_replay(mut self) -> Self {
        self.replays_provider_compaction = true;
        self
    }

    pub(crate) const fn max_output_tokens(self) -> u64 {
        self.max_output_tokens
    }

    pub(crate) const fn context_window_tokens(self) -> u64 {
        self.context_window_tokens
    }

    const fn replays_provider_compaction(self) -> bool {
        self.replays_provider_compaction
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
    reasoning_provenance: Box<[ProviderReasoningProvenance]>,
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
    /// so the preview carries its own projected membership and a conservative
    /// byte allowance for the entries no durable row can score.
    Preview {
        /// Model-visible members in projected order.
        projected_members: &'a [SemanticTranscriptEntryRef],
        /// UTF-8 text bytes and attachment-stub allowances for preview members.
        uncommitted_content_bytes: u64,
    },
}

impl From<ContextFrontierId> for ProspectiveModelInput<'_> {
    fn from(frontier: ContextFrontierId) -> Self {
        Self::Committed(frontier)
    }
}

#[derive(signalbox_derive::Accessors)]
/// Latest terminal-call usage usable as a conservative next-call lower bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReportedModelCallUsage {
    /// Returns the exact provider-reported fields retained for the call.
    #[get(copy)]
    usage: ProviderReportedTokenUsage,
    /// Whether the stored input field already includes the cache axes.
    #[get(copy)]
    input_includes_cache_tokens: bool,
    /// Whether the reported input is still model-visible for the next call.
    ///
    /// An ordinary call's input is the transcript prefix its successor resends.
    /// A dedicated compaction call's input is the source text its summary
    /// replaced, so none of it survives into the next request; that call's
    /// retained material is its summary output plus the content the compaction
    /// did not summarize, which the projected-content allowance counts.
    #[get(copy)]
    input_is_retained: bool,
    /// Provider-reported final-iteration input retained after in-response
    /// compaction, including cache axes and separate from billed usage.
    #[get(copy)]
    retained_input_tokens: Option<u64>,
    /// Provider-reported final-iteration output retained after in-response
    /// compaction, separate from billed usage.
    #[get(copy)]
    retained_output_tokens: Option<u64>,
    /// Whether reported output became assistant transcript for the next call.
    #[get(copy)]
    output_is_retained: bool,
    /// Returns a conservative byte allowance for model-visible transcript
    /// material appended after the reported call's input.
    #[get(copy)]
    projected_unreported_content_bytes: u64,
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
            &self.reasoning_provenance,
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
#[derive(Clone, Copy, Debug, Eq, PartialEq, signalbox_derive::OperatorError)]
pub enum ModelCallIdentityCollision {
    /// The proposed model-call identity already exists.
    #[error("model-call identity already exists")]
    ModelCall,
    /// A proposed semantic-entry identity already exists.
    #[error("semantic-entry identity already exists")]
    SemanticEntry,
    /// The proposed terminal-frontier identity already exists.
    #[error("context-frontier identity already exists")]
    TerminalFrontier,
    /// A proposed reclassified successor-turn identity already exists.
    #[error("reclassified successor-turn identity already exists")]
    ReclassifiedTurn,
}

#[derive(signalbox_derive::OperatorError)]
/// A durable shape that cannot reconstruct the execution aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallCorruption {
    #[error("missing model-call execution {field_0}")]
    /// One required durable record or field is absent.
    Missing(&'static str),
    #[error("inconsistent model-call execution {field_0}")]
    /// Stored records disagree about an exact relationship.
    Inconsistent(&'static str),
    #[error("unsupported model-call execution {field}: {value}")]
    /// A closed durable discriminator is unsupported.
    Unsupported {
        /// The field whose spelling is unsupported.
        field: &'static str,
        /// The exact durable spelling.
        value: String,
    },
    #[error("model-call current Session is invalid: {field_0}")]
    /// The current session projection is invalid.
    CurrentSession(SessionCorruption),
    #[error("model-call scheduling projection is invalid: {field_0}")]
    /// Complete scheduling records are invalid.
    Scheduling(SubmitInputCorruption),
    #[error("model-call execution reconstitution failed: {field_0:?}")]
    /// Complete live facts fail domain reconstitution.
    Execution(ModelCallExecutionReconstitutionFailure),
}

#[derive(signalbox_derive::OperatorError)]
/// Database, integrity, identity, or caller failure at the execution boundary.
#[derive(Debug)]
pub enum ModelCallRepositoryError {
    #[error("model-call database failure: {source}")]
    /// PostgreSQL could not complete the operation.
    Database {
        #[source]
        /// The underlying SQLx failure.
        source: sqlx::Error,
        /// Whether failure occurred while awaiting commit.
        commit_ambiguous: bool,
    },
    #[error(transparent)]
    /// Committed rows cannot form the accepted aggregate.
    Corruption(#[source] ModelCallCorruption),
    #[error(transparent)]
    /// A fresh identity collided durably.
    IdentityCollision(#[source] ModelCallIdentityCollision),
    #[error("no live model-call execution exists")]
    /// The application invoked an execution transition without a live turn.
    NoLiveExecution,
    #[error("model-call transition rejected: {field_0}")]
    /// A checked transition rejected an application-supplied operation.
    InvalidTransition(&'static str),
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

#[derive(signalbox_derive::Accessors)]
/// Runtime pool member in immutable policy order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialPoolRuntimeMember {
    /// Borrows the deployment-owned profile reference.
    #[get(str)]
    credential_reference: Arc<str>,
    priority: NonZeroU32,
    headroom_reserve_percent: Option<u8>,
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
            headroom_reserve_percent: None,
        }
    }

    /// Sets the membership's override of the pool headroom reserve.
    pub fn with_headroom_reserve(mut self, percent: Option<u8>) -> Self {
        self.headroom_reserve_percent = percent;
        self
    }

    /// Returns the membership priority.
    pub const fn priority(&self) -> NonZeroU32 {
        self.priority
    }
}

/// Selection among admissible members with equal priority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialPoolRuntimeTieBreak {
    /// Prefer the configured member order.
    FirstListed,
    /// Prefer the greatest known remaining capacity, then configured order.
    LeastUsed,
}

impl CredentialPoolRuntimeTieBreak {
    const fn as_str(self) -> &'static str {
        match self {
            Self::FirstListed => "first_listed",
            Self::LeastUsed => "least_used",
        }
    }

    fn parse(value: &str) -> Result<Self, ModelCallRepositoryError> {
        match value {
            "first_listed" => Ok(Self::FirstListed),
            "least_used" => Ok(Self::LeastUsed),
            _ => Err(ModelCallCorruption::Unsupported {
                field: "credential pool tie break",
                value: value.to_owned(),
            }
            .into()),
        }
    }
}

#[derive(signalbox_derive::Accessors)]
/// Immutable credential-pool policy supplied by admitted daemon configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialPoolRuntimePolicy {
    /// Borrows the exact pool name.
    #[get(str)]
    name: Arc<str>,
    members: Arc<[CredentialPoolRuntimeMember]>,
    on_pool_exhausted: CredentialPoolRuntimeExhaustion,
    quota_exhausted: CredentialPoolRuntimeAction,
    rate_limited: CredentialPoolRuntimeAction,
    overloaded: CredentialPoolRuntimeAction,
    credential_rejected: CredentialPoolRuntimeAction,
    tie_break: CredentialPoolRuntimeTieBreak,
    headroom_reserve_percent: Option<u8>,
    headroom_low: CredentialPoolRuntimeAction,
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
            tie_break: CredentialPoolRuntimeTieBreak::FirstListed,
            headroom_reserve_percent: None,
            headroom_low: CredentialPoolRuntimeAction::Stay,
        }
    }

    /// Attaches the admitted capacity selection and observation policy.
    pub fn with_capacity_policy(
        mut self,
        tie_break: CredentialPoolRuntimeTieBreak,
        headroom_reserve_percent: Option<u8>,
        headroom_low: CredentialPoolRuntimeAction,
    ) -> Self {
        self.tie_break = tie_break;
        self.headroom_reserve_percent = headroom_reserve_percent;
        self.headroom_low = headroom_low;
        self
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

#[derive(signalbox_derive::Accessors)]
/// PostgreSQL adapter for the initial model-call execution transactions.
#[derive(Clone, Debug)]
pub struct PostgresModelCallRepository {
    /// Borrows the shared pool for composition-owned adjacent transactions.
    #[get]
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

pub(crate) enum CountedActivationCheckpointOutcome {
    Prepared,
    PoolExhausted(CredentialPoolRuntimePolicy),
}

const MODEL_CALL_OUTBOX_ORDER_GUARD: &str = "model_call_outbox_order_guard:v1";

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
/// committed member: accepted input includes text and attachment stubs,
/// and delegated material carries the exact
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
                .map_or(0, |origin| accepted_input_content_bytes(origin.content()))
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
        | SemanticTranscriptEntryPayload::ProviderReasoning { .. }
        | SemanticTranscriptEntryPayload::AssistantToolUse { .. }
        | SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
        | SemanticTranscriptEntryPayload::ToolDenied { .. }
        | SemanticTranscriptEntryPayload::ToolInadmissible { .. }
        | SemanticTranscriptEntryPayload::ToolClosed { .. }
        | SemanticTranscriptEntryPayload::TurnCompleted { .. }
        | SemanticTranscriptEntryPayload::RunnerPlacementChanged { .. }
        | SemanticTranscriptEntryPayload::TurnCancelled { .. } => 0,
    }
}

/// Reserves text bytes and the bounded rendered stub for every attachment.
fn accepted_input_content_bytes(content: &UserContent) -> u64 {
    content
        .parts()
        .iter()
        .fold(0_u64, |total, part| match part {
            signalbox_domain::UserContentPart::Text { value } => {
                total.saturating_add(utf8_byte_length(value.as_str()))
            }
            signalbox_domain::UserContentPart::Attachment { .. } => total.saturating_add(
                u64::try_from(signalbox_application::MAX_RENDERED_ATTACHMENT_STUB_BYTES)
                    .unwrap_or(u64::MAX),
            ),
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
mod tests;

#[cfg(test)]
mod prospective_content_tests {
    use super::*;

    #[test]
    fn prospective_attachment_reserves_its_stub_alongside_utf8_text() {
        let content = UserContent::try_parts(vec![
            signalbox_domain::UserContentPart::Text {
                value: signalbox_domain::NonEmptyUnicodeText::try_new("界".to_owned()).unwrap(),
            },
            signalbox_domain::UserContentPart::Attachment {
                digest: signalbox_domain::BlobDigest::digest(b"fixture"),
                kind: signalbox_domain::AttachmentKind::File,
                media_type: signalbox_domain::DeclaredMediaType::try_new("text/plain".to_owned())
                    .unwrap(),
                display_filename: None,
            },
        ])
        .unwrap();
        assert_eq!(accepted_input_content_bytes(&content), 2307);
    }
}
