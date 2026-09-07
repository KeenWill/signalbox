//! PostgreSQL transactions surrounding the first text-only model call.
//!
//! The three transaction roles in docs/spec/model-call-execution.md stay
//! explicit here: a durable `Prepared` checkpoint, a separate
//! send-authorization commit, and a fresh post-effect observation commit. No
//! method holds a database transaction across provider work.

mod continuation;
mod credential_pool;
mod delegated_result;
mod delegation_lock;
mod live_turn;
mod load;
mod persist_disposition;
mod persist_terminal;
mod persist_tool_round;
mod prepared;
mod reread;

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
use persist_terminal::{
    persist_authorization, persist_failed_with_delegated_child_result,
    persist_terminal_outcome_with_usage,
};
pub(crate) use prepared::insert_prepared_call;

use persist_tool_round::{
    availability_retry_backoff, count_turn_credential_attempts,
    insert_credential_pool_terminal_exhaustion, is_same_credential_retry_cause,
    persist_availability_successor, persist_credential_pool_exhaustion,
};

pub(crate) use persist_disposition::SnapshotAppend;
pub(crate) use persist_disposition::SnapshotAppendError;
pub(crate) use persist_disposition::insert_snapshot;
pub(crate) use persist_disposition::insert_snapshot_append;
pub(crate) use persist_disposition::persist_reclassified_pending_steering;
use persist_disposition::{decode_stored_model_call_observation, encode_token_usage};

use prepared::{
    load_call_credential_reference, load_call_user_overrides, load_frozen_epoch_system_prompt,
    load_provider_reasoning_provenance, load_tool_conversation_entries,
    resolve_runner_placement_entries,
};

pub(crate) use continuation::fail_tool_crash_in_transaction;
pub(crate) use continuation::load_tool_continuation_execution;
pub(crate) use continuation::prepare_tool_continuation_call;
pub(crate) use continuation::resolve_session_credential;

use live_turn::{
    load_tool_denial_correlations, load_tool_inadmissible_correlations,
    load_tool_result_correlations, require_live_execution,
};
pub(crate) use reread::attach_interrupt_reclassification_candidates;
pub(crate) use reread::attach_interrupt_reclassification_candidates_for_activated;
pub(crate) use reread::attach_interrupt_reclassification_candidates_for_active;
pub(crate) use reread::attach_recovery_interrupt_reclassification_candidates;
pub(crate) use reread::attach_recovery_interrupt_reclassification_candidates_for_activated;

pub(crate) use load::authenticate_model_call_instruction_manifest;
use load::{
    decode_model_call, load_attachment_blob_facts, load_origin_contents, require_exact_call,
};

use reread::{
    attach_pending_reclassification_candidates, failed_turn_closure_matches, load_frontier_members,
    pending_reclassification_candidates, prepared_cancellation_closure_matches,
    prepared_matches_authorized, prepared_matches_stopped, record_reclassified_turn_candidate,
    select_terminal_identity_candidates, terminal_observation_closure_matches,
};

use delegated_result::{
    ExpectedDelegatedChildResult, delegated_observation_result_matches,
    delegated_terminal_result_matches,
};

use delegation_lock::{lock_model_call_terminal_frontier, locked_delegation_logical_terminal};

use credential_pool::{
    DurablePoolExclusions, SelectedRuntimePoolCredential, committed_availability_successor_backoff,
    consume_pool_member_actions, decode_prepared_usage_limit, load_availability_successor_backoff,
    load_call_pool_policy, load_durable_pool_exclusions, lock_credential_pool_action_heads,
    persist_credential_pool_member_action, prepared_serving_configuration_is_compatible,
    retain_call_capacity_policy_observation, select_runtime_pool_credential, serving_pool_target,
};

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    num::{NonZeroU32, NonZeroUsize},
    sync::Arc,
};

use rust_decimal::Decimal;
use signalbox_application::ProviderReasoningProvenance;
use signalbox_application::{
    AttachmentPreparationFailure, AuthorizeModelCallOutcome, AuthorizeModelCallTransaction,
    AvailabilitySuccessorOutcome, ClassifyOperatorFailure, CommitModelCallObservationTransaction,
    CredentialPoolExhaustedOutcome, FailPreparedModelCallTransaction, ModelCallAuthorizationReread,
    ModelCallCredentialReference, ModelCallObservationCommitOutcome,
    ModelCallTerminalIdentityCandidates, OperatorFailureClass, PrepareModelCallOutcome,
    PrepareModelCallTransaction, PreparedModelCallFailureCause, ResolvedToolConversationEntry,
    RetainedModelCallObservationStatus, RetainedPreparedFailureStatus,
};
use signalbox_domain::{
    AcceptedInputId, ContextFrontierId, CorrelatedModelCallTerminalObservation,
    FailedModelCallTurn, FailedModelCallTurnIdentities, FastMode, FrozenModelSelection,
    ModelCallDisposition, ModelCallExecutionReconstitutionFailure,
    ModelCallExecutionReconstitutionInput, ModelCallId, ModelCallOriginContent,
    ModelCallPreparationFailure, ModelCallReconstitutionState, ModelCallTerminalIdentities,
    ModelCallTerminalOutcome, ModelTargetCatalog, PendingSteeringReclassificationIdentity,
    PreparedModelCallRequest, ProviderModelCallFailureCause, ProviderModelIdentity,
    ProviderReportedTokenUsage, ResolvedProviderTarget, SemanticTranscriptEntry,
    SemanticTranscriptEntryPayload, SemanticTranscriptEntryRef, SessionId, TurnId,
    TurnTerminalCause, UserContent,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    commit_failure_is_ambiguous,
    mapping::{session_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid, turn_terminal_cause_to_str},
    outbox::{self, ModelCallOutboxState, OutboxEvent},
    session::{SessionCorruption, SessionRepositoryError, load_session_from_connection},
    submit_input::{SubmitInputCorruption, SubmitInputRepositoryError, load_scheduling_projection},
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

    /// Reads the newest ordinary or dedicated-compaction call with reported input
    /// usage for one exact target and effective fast mode.
    ///
    /// A later failed call with no usage does not erase the last provider-confirmed
    /// context size. Callers may use this only as a lower bound: later transcript
    /// entries can make the next request larger, never smaller absent compaction.
    ///
    /// The prospective input names the model-visible entries the next request
    /// would carry. Membership is compared against the reported call's own
    /// frontier, so the allowance covers exactly the content appended after the
    /// provider counted its input. `replays_provider_compaction` states whether
    /// the effective target includes opaque provider-compaction members in that
    /// request projection and therefore whether final-iteration retained counts
    /// describe the next request.
    pub async fn latest_reported_usage<'a>(
        &self,
        session: SessionId,
        target: ResolvedProviderTarget,
        fast_mode: FastMode,
        replays_provider_compaction: bool,
        prospective: impl Into<ProspectiveModelInput<'a>>,
    ) -> Result<Option<ReportedModelCallUsage>, ModelCallRepositoryError> {
        let effective_target =
            serving_pool_target(self.credential_families.as_ref(), target, fast_mode);
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
                       true AS input_is_retained,
                       model_call.retained_input_tokens,
                       model_call.retained_output_tokens,
                       (model_call.terminal_disposition_kind = 'completed') AS output_is_retained,
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
                   AND model_call.effective_provider_model_identity_id = $6
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
                       NULL::numeric AS retained_input_tokens,
                       NULL::numeric AS retained_output_tokens,
                       true AS output_is_retained,
                       latest.usage_input_tokens,
                       latest.usage_output_tokens,
                       latest.usage_cache_creation_input_tokens,
                       latest.usage_cache_read_input_tokens,
                       latest.summary_entry_id AS reported_summary_entry_id,
                       false AS has_provider_compaction,
                       NULL::numeric AS proven_unreported_content_bytes
                  FROM latest_compaction AS latest
                 WHERE latest.resolved_provider_model_identity_id = $6
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
                  FROM UNNEST($2::uuid[], $3::uuid[])
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
                    retained_input_tokens, retained_output_tokens,
                    has_provider_compaction,
                    output_is_retained,
                    usage_input_tokens, usage_output_tokens,
                    usage_cache_creation_input_tokens,
                    usage_cache_read_input_tokens,
                    COALESCE(latest_call.proven_unreported_content_bytes, 0)
                    -- Entries an uncommitted preview minted have no durable row
                    -- to score; the preview measured their content itself.
                    + $4::numeric
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
                                         'provider_reasoning',
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
                                    WHEN 'provider_reasoning' THEN
                                        COALESCE(octet_length(entry.assistant_text_value), 0)
                                    WHEN 'provider_compaction' THEN
                                        CASE WHEN $5::boolean THEN
                                            COALESCE(octet_length(entry.assistant_text_value), 0)
                                        ELSE 0 END
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
                                              'provider_reasoning',
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
        .bind(&member_sessions)
        .bind(&member_entries)
        .bind(Decimal::from(uncommitted_content_bytes))
        .bind(replays_provider_compaction)
        .bind(effective_target.identity().into_uuid())
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
        let mut retained_input_tokens = decode("retained_input_tokens")?;
        let mut retained_output_tokens = decode("retained_output_tokens")?;
        let has_provider_compaction = row.try_get::<bool, _>("has_provider_compaction")?;
        if has_provider_compaction
            && (retained_input_tokens.is_none() || retained_output_tokens.is_none())
        {
            return Err(ModelCallCorruption::Missing(
                "provider-compaction retained iteration token counts",
            )
            .into());
        }
        if has_provider_compaction && !replays_provider_compaction {
            // Without the durable block, the next request replays the preserved
            // pre-compaction history. Final-iteration retained counts describe
            // a projection that request will not carry; fall back to the
            // conservative aggregate usage evidence instead.
            retained_input_tokens = None;
            retained_output_tokens = None;
        }
        Ok(Some(ReportedModelCallUsage {
            usage: ProviderReportedTokenUsage::unreported()
                .with_input_tokens(decode("usage_input_tokens")?)
                .with_output_tokens(decode("usage_output_tokens")?)
                .with_cache_creation_input_tokens(decode("usage_cache_creation_input_tokens")?)
                .with_cache_read_input_tokens(decode("usage_cache_read_input_tokens")?),
            input_includes_cache_tokens: row.try_get("usage_input_includes_cache_tokens")?,
            input_is_retained: row.try_get("input_is_retained")?,
            retained_input_tokens,
            retained_output_tokens,
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
        let tool_inadmissible_correlations =
            load_tool_inadmissible_correlations(&mut transaction, &frontier_entries).await?;
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
        .with_tool_inadmissible_correlations(tool_inadmissible_correlations)
        .reconstitute()
        .map_err(|error| {
            let (_, failure) = error.into_parts();
            ModelCallCorruption::Execution(failure)
        })?;
        let prepared = execution
            .clone()
            .prepare_initial_call(call)
            .map_err(|_| ModelCallRepositoryError::InvalidTransition("preview initial call"))?;
        let mut request = execution
            .preview_initial_call(call)
            .map_err(|_| ModelCallRepositoryError::InvalidTransition("preview initial call"))?;
        let system_prompt = load_frozen_epoch_system_prompt(
            &mut transaction,
            session_id,
            preview.turn().configuration().session_defaults_version(),
        )
        .await?;
        resolve_runner_placement_entries(transaction.as_mut(), &mut request).await?;
        let tool_entries = load_tool_conversation_entries(&mut transaction, &request).await?;
        let reasoning_provenance =
            load_provider_reasoning_provenance(&mut transaction, &request).await?;
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
        let serving_evidence = prepared_serving_evidence(
            self.credential_families.as_ref(),
            &self.continuation_usage_limits,
            request.call().target(),
            fast_mode,
        );
        let selected = select_runtime_pool_credential(
            &mut transaction,
            session_id,
            execution.turn(),
            execution.current_attempt().id(),
            serving_evidence,
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
            reasoning_provenance,
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
    ) -> Result<CountedActivationCheckpointOutcome, ModelCallRepositoryError> {
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
        let serving_evidence = prepared_serving_evidence(
            self.credential_families.as_ref(),
            &self.continuation_usage_limits,
            prepared.call().target(),
            fast_mode,
        );
        let selected = select_runtime_pool_credential(
            connection,
            prepared.session(),
            prepared.turn(),
            prepared.attempt(),
            serving_evidence,
            credential_reference,
            &self.credential_pools,
        )
        .await?;
        outbox::lock_sequence_allocator(connection).await?;
        let Some(credential_reference) = selected.reference.as_ref() else {
            // The activated turn remains call-free. The ordinary counted path
            // hands it to preparation; a definitive attachment path already
            // has identities and closes the typed exhaustion in this transaction.
            let policy = selected
                .policy
                .ok_or(ModelCallRepositoryError::InvalidTransition(
                    "credential-pool exhaustion is missing its frozen policy",
                ))?;
            return Ok(CountedActivationCheckpointOutcome::PoolExhausted(policy));
        };
        insert_prepared_call(
            connection,
            prepared,
            credential_reference,
            selected.policy.as_ref(),
            self.cache_inclusive_input_targets
                .contains(&prepared.call().target()),
            serving_evidence,
        )
        .await?;
        consume_pool_member_actions(
            connection,
            prepared.turn(),
            &selected.pending_consumed_actions,
        )
        .await?;
        Ok(CountedActivationCheckpointOutcome::Prepared)
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
        let checkpoint = self
            .checkpoint_counted_activation_in_transaction(
                connection,
                activated,
                prospective,
                outbox_order_guard,
            )
            .await?;
        if let CountedActivationCheckpointOutcome::PoolExhausted(policy) = checkpoint {
            let execution =
                require_live_execution(connection, activated.session(), &self.targets).await?;
            let exhausted = execution
                .fail_credential_pool_exhausted(policy.name().to_owned(), identities)
                .map_err(|_| {
                    ModelCallRepositoryError::InvalidTransition(
                        "credential-pool exhaustion could not close counted activation",
                    )
                })?;
            persist_credential_pool_exhaustion(connection, &exhausted).await?;
            return Ok(exhausted.into_failed());
        }
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
                        let mut request = execution.resume_prepared_call().map_err(|_| {
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
                        resolve_runner_placement_entries(transaction.as_mut(), &mut request)
                            .await?;
                        let tool_entries =
                            load_tool_conversation_entries(&mut transaction, &request).await?;
                        let reasoning_provenance =
                            load_provider_reasoning_provenance(&mut transaction, &request).await?;
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
                                reasoning_provenance,
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
                let serving_evidence = prepared_serving_evidence(
                    self.credential_families.as_ref(),
                    &self.continuation_usage_limits,
                    resolved.target(),
                    fast_mode,
                );
                let selected = Some(
                    select_runtime_pool_credential(
                        &mut transaction,
                        session,
                        execution.turn(),
                        execution.current_attempt().id(),
                        serving_evidence,
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
            let serving_evidence = prepared_serving_evidence(
                self.credential_families.as_ref(),
                &self.continuation_usage_limits,
                prepared.call().target(),
                fast_mode,
            );
            insert_prepared_call(
                &mut transaction,
                &prepared,
                credential_reference,
                selected.policy.as_ref(),
                self.cache_inclusive_input_targets
                    .contains(&prepared.call().target()),
                serving_evidence,
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
            let Some(current) = execution.current_call() else {
                return Ok((false, AuthorizeModelCallOutcome::NoSend));
            };
            if current.id() != call
                || current.state() != signalbox_domain::CurrentModelCallState::Prepared
            {
                return Ok((false, AuthorizeModelCallOutcome::NoSend));
            }
            let fast_mode = execution
                .configuration()
                .effective()
                .model_settings()
                .effective()
                .fast_mode();
            let current_serving_evidence = prepared_serving_evidence(
                self.credential_families.as_ref(),
                &self.continuation_usage_limits,
                current.target(),
                fast_mode,
            );
            let current_effective_target = current_serving_evidence.effective_target;
            let stored_serving_evidence = sqlx::query(
                "SELECT effective_provider_model_identity_id,
                        prepared_credential_model_family,
                        prepared_max_output_tokens,
                        prepared_context_window_tokens,
                        prepared_provider_compaction_replay
                   FROM model_call
                  WHERE model_call_id = $1",
            )
            .bind(call.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
            let stored_effective_target =
                ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                    stored_serving_evidence.try_get("effective_provider_model_identity_id")?,
                ));
            let stored_credential_model_family = stored_serving_evidence
                .try_get::<Option<String>, _>("prepared_credential_model_family")?;
            let stored_limit =
                decode_prepared_usage_limit(&stored_serving_evidence, stored_effective_target)?;
            if !prepared_serving_configuration_is_compatible(
                stored_effective_target,
                stored_credential_model_family.as_deref(),
                stored_limit,
                current_serving_evidence,
            ) {
                return Ok((false, AuthorizeModelCallOutcome::NoSend));
            }
            let authorized = execution.authorize_send().map_err(|_| {
                ModelCallCorruption::Inconsistent("checked Prepared call could not authorize send")
            })?;
            persist_authorization(&mut transaction, &authorized, current_effective_target).await?;
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
            if let Some(snapshot) = observation.rate_limits() {
                retain_call_capacity_policy_observation(&mut transaction, &observation, snapshot)
                    .await?;
            }
            let retained_input_tokens = observation.observation().retained_input_tokens();
            let retained_output_tokens = observation.observation().retained_output_tokens();
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
                        retained_input_tokens,
                        retained_output_tokens,
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
                // A successor reissues the request, so availability failures
                // need the adapter's proof that the failed request was never
                // accepted. Credential rejection is the one exception: the
                // authentication refusal itself authorizes rotation, but never
                // a retry on the rejected credential.
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
                    && (observation.non_acceptance_proven()
                        || cause == ProviderModelCallFailureCause::CredentialRejected)
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
                // The failed credential itself must still be admitted for a
                // retry. Otherwise only the pinned action may authorize a
                // rotation; every other action follows the terminal path.
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
                        encode_provider_failure_cause(cause),
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
                    retained_input_tokens,
                    retained_output_tokens,
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
                retained_input_tokens,
                retained_output_tokens,
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
                        usage_cache_read_input_tokens,
                        retained_input_tokens, retained_output_tokens
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
                        && stored.usage == encode_token_usage(observation.usage())
                        && stored.retained_input_tokens
                            == observation
                                .observation()
                                .retained_input_tokens()
                                .map(Decimal::from)
                        && stored.retained_output_tokens
                            == observation
                                .observation()
                                .retained_output_tokens()
                                .map(Decimal::from) =>
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
mod tests;
