//! Read-only PostgreSQL projections for the local process protocol.
//!
//! These values are persistence-owned snapshots, not process-protocol frames or
//! domain aggregates. Reads use one read-only repeatable-read transaction so
//! the hub can map a complete, stable projection explicitly.

use std::{collections::VecDeque, error::Error, fmt};

use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_domain::{
    AcceptedInputId, ContextFrontierId, CredentialProfileName, DelegationMessageId,
    DirectModelSelection, FrozenAliasDefinition, FrozenModelSelection, ImportedConversationId,
    ImportedSourceAttestation, ImportedTranscriptContent, ImportedTranscriptEntryId, ModelAlias,
    ModelCallId, ModelSelectionRequest, ProviderModelIdentity, ResolvedProviderTarget,
    RunnerCapabilityClass, RunnerGeneration, RunnerId, RunnerSandboxProfile, RunnerSelector,
    RunnerWorkingDirectory, SemanticTranscriptEntryId, SemanticTranscriptEntryRef, SessionId,
    SessionReadScopeDecision, SessionReadScopeRefusal, ToolApprovalDecider, ToolApprovalDecision,
    ToolApprovalResolutionReconstitutionInput, ToolAttemptId, ToolDecisionRationale,
    ToolDenialReason, ToolRequestId, TurnAttemptId, TurnId, TurnModelSettingsResolved, UserContent,
    VersionedSessionPlacement, WorkspaceRepositoryKey,
};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

use crate::{
    conversation_import_codec::decode_content,
    mapping::{
        ToolApprovalDecisionSourceStorageKind, ToolAttemptDispositionStorageKind,
        defaults_version_from_numeric, durable_command_id_from_uuid,
        model_change_adjustments_from_json, model_settings_from_json,
        model_settings_overlay_from_json, runner_sandbox_from_str, session_id_from_uuid,
        session_id_to_uuid, tool_approval_decision_source_from_str,
        tool_attempt_disposition_from_str,
    },
    outbox::{
        DispatchedDelegationOutcome, DispatchedDelegationProvenance, DispatchedDelegationReason,
        DispatchedDelegationWaitMode, decode_delegation_outcome, decode_delegation_provenance,
        decode_delegation_reason, decode_wait_mode,
    },
};

const REPEATABLE_READ_ONLY: &str = "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY";
/// Hard safety ceiling on session identities read ahead by one summary cursor;
/// it bounds page memory and the number of histories authenticated per query.
const SESSION_SUMMARY_PAGE_SIZE: i64 = 64;

/// One model-selection request in the process-facing session summary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessModelSelection {
    /// A stable direct-selection identity.
    Direct(DirectModelSelection),
    /// A stable alias identity.
    Alias(ModelAlias),
}

/// Closed current state in one process-facing runner projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessRunnerProjectionState {
    /// No runner has been pinned yet.
    Unpinned,
    /// The current placement is pinned.
    Pinned,
    /// The exact selected runner was lost before pinning.
    RunnerLostBeforePin,
    /// The pinned runner was lost.
    RunnerLost,
    /// The lost placement was explicitly abandoned.
    RunnerAbandoned,
}

/// Closed current connection health in one process-facing runner projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessRunnerConnectionHealth {
    /// The runner connection is currently healthy.
    Connected,
    /// The connection is inside its missed-heartbeat recovery window.
    Suspect,
    /// The connection closed through orderly shutdown.
    Shutdown,
    /// The connection reached terminal loss.
    Lost,
}

/// Complete current runner placement from one repeatable-read snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessRunnerProjection {
    selector: RunnerSelector,
    runner: Option<RunnerId>,
    placement_revision: RunnerGeneration,
    sandbox: RunnerSandboxProfile,
    credential_profile: Option<CredentialProfileName>,
    repository: Option<WorkspaceRepositoryKey>,
    working_directory: Option<RunnerWorkingDirectory>,
    connection_health: Option<ProcessRunnerConnectionHealth>,
    state: ProcessRunnerProjectionState,
}

impl ProcessRunnerProjection {
    /// Borrows the immutable requested selector.
    pub const fn selector(&self) -> &RunnerSelector {
        &self.selector
    }

    /// Returns the current or lost exact runner when the state names one.
    pub const fn runner(&self) -> Option<RunnerId> {
        self.runner
    }

    /// Returns the positive current placement revision.
    pub const fn placement_revision(&self) -> RunnerGeneration {
        self.placement_revision
    }

    /// Returns the explicitly selected sandbox profile.
    pub const fn sandbox(&self) -> RunnerSandboxProfile {
        self.sandbox
    }

    /// Borrows the independently nullable requested credential profile.
    pub const fn credential_profile(&self) -> Option<&CredentialProfileName> {
        self.credential_profile.as_ref()
    }

    /// Borrows the independently nullable requested repository key.
    pub const fn repository(&self) -> Option<&WorkspaceRepositoryKey> {
        self.repository.as_ref()
    }

    /// Borrows the independently nullable exact requested directory.
    pub const fn working_directory(&self) -> Option<&RunnerWorkingDirectory> {
        self.working_directory.as_ref()
    }

    /// Returns current connection health exactly while the placement is pinned.
    pub const fn connection_health(&self) -> Option<ProcessRunnerConnectionHealth> {
        self.connection_health
    }

    /// Returns the exact current placement state.
    pub const fn state(&self) -> ProcessRunnerProjectionState {
        self.state
    }
}

/// One current session summary read from a shared transaction snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSessionSummary {
    session: SessionId,
    defaults_version: u64,
    model_selection: ProcessModelSelection,
    placement: signalbox_domain::VersionedSessionPlacement,
    runner: Option<ProcessRunnerProjection>,
}

impl ProcessSessionSummary {
    /// Returns the summarized session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the current positive defaults version.
    pub const fn defaults_version(&self) -> u64 {
        self.defaults_version
    }

    /// Returns the current model-selection request.
    pub const fn model_selection(&self) -> ProcessModelSelection {
        self.model_selection
    }

    /// Borrows the current immutable placement epoch.
    pub const fn placement(&self) -> &signalbox_domain::VersionedSessionPlacement {
        &self.placement
    }

    /// Borrows the complete current runner projection when runner placement was requested.
    pub const fn runner(&self) -> Option<&ProcessRunnerProjection> {
        self.runner.as_ref()
    }
}

/// One complete immutable session-defaults epoch read for the process
/// boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSessionDefaults {
    session: SessionId,
    version: signalbox_domain::SessionConfigurationDefaultsVersion,
    defaults: signalbox_domain::SessionConfigurationDefaults,
}

impl ProcessSessionDefaults {
    /// Returns the selected session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the read immutable epoch's version.
    pub const fn version(&self) -> signalbox_domain::SessionConfigurationDefaultsVersion {
        self.version
    }

    /// Borrows the complete defaults value on that epoch.
    pub const fn defaults(&self) -> &signalbox_domain::SessionConfigurationDefaults {
        &self.defaults
    }
}

/// Typed outcome of one session-defaults epoch read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessSessionDefaultsRead {
    /// The selected epoch with its complete defaults value.
    Read(ProcessSessionDefaults),
    /// The selected session does not exist in the read snapshot.
    SessionNotFound,
    /// The session exists but the named epoch was never installed.
    VersionNotFound,
}

/// Typed outcome of the path-scoped native transcript-open boundary.
#[derive(Debug)]
pub enum ProcessScopedTranscriptRead {
    /// The target exists and its transcript cursor is open in the checked snapshot.
    Opened(Box<ProcessTranscriptReader>),
    /// The selected target session does not exist in the checked snapshot.
    TargetNotFound,
    /// The requesting placement's parent directory does not contain the target.
    Refused(SessionReadScopeRefusal),
}

fn decode_session_defaults_value(
    row: &PgRow,
) -> Result<signalbox_domain::SessionConfigurationDefaults, ProcessReadError> {
    let kind: String = row
        .try_get::<Option<String>, _>("model_selection_kind")?
        .ok_or(ProcessReadCorruption::Missing("model_selection_kind"))?;
    let direct: Option<Uuid> = row.try_get("direct_model_selection_id")?;
    let alias: Option<Uuid> = row.try_get("model_alias_id")?;
    let model = match (kind.as_str(), direct, alias) {
        ("direct", Some(value), None) => {
            signalbox_domain::ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(value))
        }
        ("alias", None, Some(value)) => {
            signalbox_domain::ModelSelectionRequest::Alias(ModelAlias::from_uuid(value))
        }
        ("direct" | "alias", _, _) => {
            return Err(ProcessReadCorruption::Inconsistent("model selection").into());
        }
        _ => {
            return Err(ProcessReadCorruption::Unsupported {
                field: "model_selection_kind",
                value: kind,
            }
            .into());
        }
    };
    let tool_approval: String = row
        .try_get::<Option<String>, _>("dangerous_tool_auto_approval")?
        .ok_or(ProcessReadCorruption::Missing(
            "dangerous_tool_auto_approval",
        ))?;
    let dangerous_tool_auto_approval = crate::mapping::dangerous_tool_auto_approval_from_str(
        &tool_approval,
    )
    .ok_or(ProcessReadCorruption::Unsupported {
        field: "dangerous_tool_auto_approval",
        value: tool_approval,
    })?;
    let system_prompt = row
        .try_get::<Option<String>, _>("system_prompt")?
        .map(|value| {
            signalbox_domain::SessionSystemPrompt::try_new(value)
                .map_err(|_| ProcessReadCorruption::Inconsistent("system prompt admission"))
        })
        .transpose()?;
    let model_settings = row
        .try_get::<Option<serde_json::Value>, _>("model_settings")?
        .ok_or(ProcessReadCorruption::Missing("model_settings"))?;
    let model_settings = model_settings_from_json(model_settings)
        .map_err(|_| ProcessReadCorruption::Inconsistent("model settings"))?;
    signalbox_domain::SessionConfigurationDefaults::complete_with_model_settings(
        model,
        dangerous_tool_auto_approval,
        system_prompt,
        model_settings,
    )
    .ok_or_else(|| {
        ProcessReadCorruption::Inconsistent("model settings validation selection").into()
    })
}

/// One repeatable-read session-summary cursor with bounded read-ahead.
///
/// Call [`Self::next_summary`] until it returns `None`. That terminal call
/// commits the read-only transaction and makes [`Self::summary_count`]
/// available. Each page batches placement authentication for up to 64 sessions;
/// dropping a reader early rolls its transaction back.
#[derive(Debug)]
pub struct ProcessSessionSummaryReader {
    transaction: Option<Transaction<'static, Postgres>>,
    next_session_after: Option<Uuid>,
    pending: VecDeque<PendingSessionSummary>,
    summary_count: u64,
    committed_summary_count: Option<u64>,
}

#[derive(Debug)]
struct PendingSessionSummary {
    session: SessionId,
    defaults_version: u64,
    model_selection: ProcessModelSelection,
    placement: VersionedSessionPlacement,
}

impl PendingSessionSummary {
    fn with_runner(self, runner: Option<ProcessRunnerProjection>) -> ProcessSessionSummary {
        ProcessSessionSummary {
            session: self.session,
            defaults_version: self.defaults_version,
            model_selection: self.model_selection,
            placement: self.placement,
            runner,
        }
    }
}

impl ProcessSessionSummaryReader {
    /// Returns the committed count only after [`Self::next_summary`] returned
    /// `None`.
    pub const fn summary_count(&self) -> Option<u64> {
        self.committed_summary_count
    }

    /// Yields one summary in session-identity order without retaining prior
    /// decoded rows.
    pub async fn next_summary(
        &mut self,
    ) -> Result<Option<ProcessSessionSummary>, ProcessReadError> {
        if self.committed_summary_count.is_some() {
            return Ok(None);
        }

        if self.pending.is_empty() {
            let next_session_after = self.next_session_after;
            let (pending, next_session_after) =
                load_session_summary_page(self.transaction_mut()?, next_session_after).await?;
            self.pending = pending;
            self.next_session_after = next_session_after;
        }

        if let Some(pending) = self.pending.front() {
            let session = pending.session;
            let runner = load_process_runner_projection(self.transaction_mut()?, session).await?;
            let summary = self
                .pending
                .pop_front()
                .ok_or(ProcessReadCorruption::Missing("pending session summary"))?
                .with_runner(runner);
            self.summary_count =
                self.summary_count
                    .checked_add(1)
                    .ok_or(ProcessReadCorruption::InvalidOrdinal(
                        "session summary count",
                    ))?;
            return Ok(Some(summary));
        }

        let transaction = self
            .transaction
            .take()
            .ok_or(ProcessReadCorruption::Missing("process read transaction"))?;
        transaction.commit().await?;
        self.committed_summary_count = Some(self.summary_count);
        Ok(None)
    }

    fn transaction_mut(&mut self) -> Result<&mut Transaction<'static, Postgres>, ProcessReadError> {
        self.transaction
            .as_mut()
            .ok_or_else(|| ProcessReadCorruption::Missing("process read transaction").into())
    }
}

async fn load_session_summary_page(
    transaction: &mut Transaction<'static, Postgres>,
    next_session_after: Option<Uuid>,
) -> Result<(VecDeque<PendingSessionSummary>, Option<Uuid>), ProcessReadError> {
    let rows = sqlx::query(
        "SELECT
            session_row.session_id,
            current_defaults.current_version AS defaults_version,
            selected_defaults.model_selection_kind,
            selected_defaults.direct_model_selection_id,
            selected_defaults.model_alias_id
           FROM session AS session_row
           LEFT JOIN session_current_defaults AS current_defaults
             ON current_defaults.session_id = session_row.session_id
           LEFT JOIN session_defaults_version AS selected_defaults
             ON selected_defaults.session_id = current_defaults.session_id
            AND selected_defaults.version = current_defaults.current_version
          WHERE ($1::uuid IS NULL OR session_row.session_id > $1)
          ORDER BY session_row.session_id
          LIMIT $2",
    )
    .bind(next_session_after)
    .bind(SESSION_SUMMARY_PAGE_SIZE)
    .fetch_all(&mut **transaction)
    .await?;
    if rows.is_empty() {
        return Ok((VecDeque::new(), next_session_after));
    }

    let sessions = rows
        .iter()
        .map(|row| required::<Uuid>(row, "session_id").map(session_id_from_uuid))
        .collect::<Result<Vec<_>, _>>()?;
    let mut placements = crate::session_placement::load_current_batch(transaction, &sessions)
        .await
        .map_err(map_session_placement_read_error)?;
    let mut pending = VecDeque::with_capacity(rows.len());
    for row in rows {
        let session_uuid = required(&row, "session_id")?;
        let placement = placements
            .remove(&session_uuid)
            .ok_or(ProcessReadCorruption::Missing("session placement"))?;
        pending.push_back(decode_pending_session_summary(&row, placement)?);
    }
    if !placements.is_empty() {
        return Err(ProcessReadCorruption::Inconsistent("session placement batch").into());
    }
    let next_session_after = pending
        .back()
        .map(|summary| session_id_to_uuid(summary.session));
    Ok((pending, next_session_after))
}

/// Durable state of the current model call attached to an active turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessCurrentModelCallState {
    /// Provider work has not been authorized.
    Prepared,
    /// Provider work was authorized and may have happened.
    InFlight,
    /// Cancellation was durably requested for issued provider work.
    CancellationRequested,
}

/// Current model call attached to the active turn attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessCurrentModelCall {
    call: ModelCallId,
    state: ProcessCurrentModelCallState,
}

impl ProcessCurrentModelCall {
    /// Returns the current model-call identity.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Returns the exact durable call state.
    pub const fn state(&self) -> ProcessCurrentModelCallState {
        self.state
    }
}

/// Terminal model-call dispositions admitted by a failed turn projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessFailedModelCallDisposition {
    /// The provider interaction definitively failed.
    KnownFailed,
    /// The provider call was cancelled without terminalizing the turn as
    /// cancelled.
    Cancelled,
}

/// Persistence-owned closed classification of a definitive provider error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessProviderModelCallFailureCause {
    /// The provider rejected the request credential.
    CredentialRejected,
    /// The credential lacked permission.
    PermissionDenied,
    /// The provider judged the request invalid.
    InvalidRequest,
    /// The requested model or resource was not found.
    TargetNotFound,
    /// The request exceeded a provider size limit.
    RequestTooLarge,
    /// The provider applied a transient rate limit.
    RateLimited,
    /// The account's available quota was exhausted.
    QuotaExhausted,
    /// The provider reported overload.
    Overloaded,
    /// The provider reported an internal error.
    ProviderInternal,
    /// The adapter did not recognize the definitive provider error.
    Unrecognized,
}

/// Persistence-owned closed classification of an unsent attachment-preparation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessAttachmentPreparationFailureCause {
    /// Distinct rendered attachment bytes exceeded the deployment ceiling.
    TooLarge,
    /// No recorded replica contained the required attachment.
    Missing,
    /// Recorded replicas failed length or digest verification.
    Corrupt,
}

/// Optional terminal model-call evidence for a failed turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessFailedTerminalModelCall {
    call: ModelCallId,
    disposition: ProcessFailedModelCallDisposition,
    provider_failure_cause: Option<ProcessProviderModelCallFailureCause>,
    attachment_preparation_failure_cause: Option<ProcessAttachmentPreparationFailureCause>,
}

impl ProcessFailedTerminalModelCall {
    /// Returns the terminal model-call identity.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Returns the exact terminal model-call disposition.
    pub const fn disposition(&self) -> ProcessFailedModelCallDisposition {
        self.disposition
    }

    /// Returns the closed provider classification when this call retained one.
    pub const fn provider_failure_cause(&self) -> Option<ProcessProviderModelCallFailureCause> {
        self.provider_failure_cause
    }

    /// Returns the closed local attachment-preparation cause when retained.
    pub const fn attachment_preparation_failure_cause(
        &self,
    ) -> Option<ProcessAttachmentPreparationFailureCause> {
        self.attachment_preparation_failure_cause
    }
}

/// Whether a session can owe a user reconciliation decision right now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessModelCallRecoveryPrecondition {
    /// No such session exists in this snapshot.
    SessionAbsent,
    /// The session exists but no active turn is parked on a model call.
    NoParkedTurn,
    /// The session's active turn is parked on this exact ambiguous call.
    Parked {
        /// The active turn holding the slot until reconciliation.
        turn: TurnId,
    },
}

/// Authoritative lifecycle state for one projected turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessTurnState {
    /// Accepted work has not activated.
    Queued {
        /// Accepted input that created the queued turn.
        accepted_input: AcceptedInputId,
        /// Exact accepted ordered user content.
        content: UserContent,
    },
    /// Delegated work has not activated.
    QueuedDelegated {
        /// Tool request that spawned the delegated session.
        spawning_request: ToolRequestId,
        /// Parent session that issued the spawn request.
        parent_session: SessionId,
        /// Parent turn that issued the spawn request.
        parent_turn: TurnId,
        /// Exact delegated task text.
        content: String,
    },
    /// A contiguous range of delivered delegation content is waiting to wake
    /// an otherwise idle recipient.
    QueuedDelegationWake {
        /// First recipient-wide delivery sequence included by the wake.
        first_delivery_sequence: u64,
        /// Last recipient-wide delivery sequence included by the wake.
        through_delivery_sequence: u64,
    },
    /// Parent policy logically terminalized the delegated root while any
    /// retained physical execution evidence remains inert.
    DelegationTerminated {
        /// Tool request that spawned the terminalized child.
        spawning_request: ToolRequestId,
        /// Typed stopped or cancelled outcome.
        outcome: DispatchedDelegationOutcome,
        /// Exact parent terminal reason.
        reason: DispatchedDelegationReason,
        /// Exact parent-command provenance.
        provenance: DispatchedDelegationProvenance,
    },
    /// The current attempt is running.
    ActiveRunning {
        /// Current live attempt.
        current_attempt: TurnAttemptId,
        /// Current provider call, when one has been prepared or authorized.
        current_model_call: Option<ProcessCurrentModelCall>,
    },
    /// The ended attempt is parked on an ambiguous model call.
    ActiveAwaitingModelCallRecovery {
        /// Ended attempt whose call is ambiguous.
        ended_attempt: TurnAttemptId,
        /// Ambiguous call awaiting recovery.
        recovery_call: ModelCallId,
        /// Durable automatic attempts already claimed.
        automatic_reconciliation_attempts: u32,
        /// True only after the automatic attempt budget is exhausted.
        operator_action_required: bool,
    },
    /// The yielded tool batch is parked on a user decision.
    ActiveAwaitingToolApproval {
        /// Earliest undecided tool request.
        request: ToolRequestId,
    },
    /// The yielded foreground await is parked on one exact delegated child.
    ActiveAwaitingChild {
        /// Tool request that issued the foreground await.
        awaiting_request: ToolRequestId,
        /// Spawn request naming the relationship.
        spawning_request: ToolRequestId,
        /// Exact child whose terminal result releases this turn.
        child: SessionId,
    },
    /// The yielded tool batch is parked on an ambiguous external effect.
    ActiveAwaitingToolRecovery {
        /// Ended turn attempt that issued the tool effect.
        ended_attempt: TurnAttemptId,
        /// Ambiguous tool attempt awaiting recovery.
        recovery_attempt: ToolAttemptId,
        /// Durable automatic attempts already claimed.
        automatic_reconciliation_attempts: u32,
        /// True only after the automatic attempt budget is exhausted.
        operator_action_required: bool,
    },
    /// The turn is parked on replacement of one exact lost runner placement.
    ActiveAwaitingRunnerRecovery {
        /// Runner whose durable loss owns this wait.
        runner: RunnerId,
        /// Positive placement revision against which loss was projected.
        placement_revision: RunnerGeneration,
        /// Physical tool attempt interrupted by loss, when one exists.
        interrupted_tool_attempt: Option<ToolAttemptId>,
    },
    /// The turn terminalized as failed.
    Failed {
        /// Exact terminal semantic frontier.
        terminal_frontier: ContextFrontierId,
        /// Terminal physical attempt, absent only for an evidence-free
        /// recovery failure.
        terminal_attempt: Option<TurnAttemptId>,
        /// Terminal call evidence, absent when no call existed.
        terminal_model_call: Option<ProcessFailedTerminalModelCall>,
    },
    /// The turn terminalized as completed.
    Completed {
        /// Exact terminal semantic frontier.
        terminal_frontier: ContextFrontierId,
        /// Outcome-authoritative attempt.
        terminal_attempt: TurnAttemptId,
        /// Outcome-authoritative model call.
        terminal_call: ModelCallId,
    },
    /// The turn terminalized as refused.
    Refused {
        /// Exact terminal semantic frontier.
        terminal_frontier: ContextFrontierId,
        /// Outcome-authoritative attempt.
        terminal_attempt: TurnAttemptId,
        /// Outcome-authoritative model call.
        terminal_call: ModelCallId,
    },
    /// The turn terminalized after confirmed cancellation.
    Cancelled {
        /// Exact terminal semantic frontier.
        terminal_frontier: ContextFrontierId,
        /// Outcome-authoritative attempt.
        terminal_attempt: TurnAttemptId,
        /// Terminal call, absent when cancellation preceded preparation.
        terminal_call: Option<ModelCallId>,
    },
    /// The turn terminalized requiring external reconciliation.
    ReconciliationRequired {
        /// Exact terminal semantic frontier.
        terminal_frontier: ContextFrontierId,
        /// Outcome-authoritative attempt.
        terminal_attempt: TurnAttemptId,
        /// Exact ambiguous terminal operation.
        operation: ProcessReconciliationOperation,
    },
}

/// Exact ambiguous operation exposed by a process transcript projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessReconciliationOperation {
    /// Ambiguous provider call.
    ModelCall(ModelCallId),
    /// Ambiguous tool attempt.
    ToolAttempt(ToolAttemptId),
}

/// One turn in acceptance order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessTranscriptTurn {
    turn: TurnId,
    acceptance_position: u64,
    state: ProcessTurnState,
    model_settings: Option<TurnModelSettingsResolved>,
}

/// Exact token fields for one terminal model call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessModelCallTokenUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

/// Closed provenance of one terminal model call's usage fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessModelCallUsageProvenance {
    /// Counts reported by the provider or adapter stream.
    Reported,
    /// Counts produced by an explicit estimator.
    Estimated,
}

impl ProcessModelCallUsageProvenance {
    fn from_storage(value: &str) -> Option<Self> {
        match value {
            "reported" => Some(Self::Reported),
            "estimated" => Some(Self::Estimated),
            _ => None,
        }
    }
}

/// Closed meaning of a provider-reported model-call input-token count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessModelCallInputTokenSemantics {
    /// The input count excludes the separately reported cache axes.
    CacheExclusive,
    /// The input count includes the separately reported cache axes.
    CacheInclusive,
}

impl ProcessModelCallInputTokenSemantics {
    const fn from_storage(value: Option<bool>) -> Option<Self> {
        match value {
            Some(true) => Some(Self::CacheInclusive),
            Some(false) => Some(Self::CacheExclusive),
            None => None,
        }
    }
}

impl ProcessModelCallTokenUsage {
    /// Returns the input-token count when present.
    pub const fn input_tokens(self) -> Option<u64> {
        self.input_tokens
    }

    /// Returns the output-token count when present.
    pub const fn output_tokens(self) -> Option<u64> {
        self.output_tokens
    }

    /// Returns the cache-creation input-token count when present.
    pub const fn cache_creation_input_tokens(self) -> Option<u64> {
        self.cache_creation_input_tokens
    }

    /// Returns the cache-read input-token count when present.
    pub const fn cache_read_input_tokens(self) -> Option<u64> {
        self.cache_read_input_tokens
    }
}

/// One terminal model call's typed token evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessTranscriptModelCallUsage {
    turn: TurnId,
    call: ModelCallId,
    target: ResolvedProviderTarget,
    credential_profile: String,
    input_token_semantics: Option<ProcessModelCallInputTokenSemantics>,
    provenance: ProcessModelCallUsageProvenance,
    usage: ProcessModelCallTokenUsage,
}

impl ProcessTranscriptModelCallUsage {
    /// Returns the owning turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the terminal model-call identity.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Returns the immutable provider target whose configured rates apply.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.target
    }

    /// Returns the event-sourced credential profile pinned into this call.
    pub fn credential_profile(&self) -> &str {
        &self.credential_profile
    }

    /// Returns the pinned meaning of this call's reported input-token count.
    ///
    /// Absence identifies a call prepared before that semantic pin existed.
    pub const fn input_token_semantics(&self) -> Option<ProcessModelCallInputTokenSemantics> {
        self.input_token_semantics
    }

    /// Returns the closed provenance of this call's token fields.
    pub const fn provenance(&self) -> ProcessModelCallUsageProvenance {
        self.provenance
    }

    /// Returns the exact independently optional provider fields.
    pub const fn usage(&self) -> ProcessModelCallTokenUsage {
        self.usage
    }
}

impl ProcessTranscriptTurn {
    /// Returns the immutable turn identity.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the immutable positive acceptance position.
    pub const fn acceptance_position(&self) -> u64 {
        self.acceptance_position
    }

    /// Returns the authoritative lifecycle state.
    pub const fn state(&self) -> &ProcessTurnState {
        &self.state
    }

    /// Returns complete frozen settings evidence when the turn was committed
    /// after settings persistence became available.
    pub const fn model_settings(&self) -> Option<&TurnModelSettingsResolved> {
        self.model_settings.as_ref()
    }
}

/// Session ancestry relevant to process-protocol compatibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessSessionAncestry {
    /// User-initiated native session.
    UserInitiated,
    /// Session seeded from one immutable imported frontier.
    ImportedConversation,
}

/// Exact source-speaker attestation in the conservative process projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessImportedSourceSpeaker {
    /// The source omitted the speaker field.
    NotAttested,
    /// The source explicitly supplied no speaker.
    AttestedAbsent,
    /// The source attested user authorship.
    User,
    /// The source attested assistant authorship.
    Assistant,
}

/// Conservative imported content kind exposed by the process read boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessImportedContentKind {
    /// One source event.
    SourceEvent,
    /// One source-defined message block.
    SourceMessageBlock,
    /// Text whose value is unattested or explicitly absent.
    Text,
    /// One tool call.
    ToolCall,
    /// One tool result.
    ToolResult,
    /// One thinking block.
    Thinking,
    /// One redacted-thinking block.
    RedactedThinking,
    /// One document block.
    Document,
    /// One typed message-content absence.
    MessageContentAbsent,
}

/// Typed outcome of an executed tool-result transcript entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessToolExecutionResultDisposition {
    /// The executor returned admitted result content.
    Completed,
    /// The executor returned definitive typed failure evidence.
    KnownFailed,
}

/// One ordered member of the latest authoritative semantic frontier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessTranscriptEntry {
    /// Exact delegated task that opened one child session.
    DelegatedTask {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Tool request that spawned the child.
        spawning_request: ToolRequestId,
        /// Parent session that issued the spawn request.
        parent_session: SessionId,
        /// Parent turn that issued the spawn request.
        parent_turn: TurnId,
        /// Exact delegated task text.
        content: String,
    },
    /// Exact bidirectional delegation message delivered to this frontier.
    DelegationMessage {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Relationship identity.
        spawning_request: ToolRequestId,
        /// Immutable message identity.
        message: DelegationMessageId,
        /// Sending session.
        sender: SessionId,
        /// Receiving session.
        recipient: SessionId,
        /// Relationship-local message ordinal.
        ordinal: u64,
        /// Recipient-wide delivery sequence.
        delivery_sequence: u64,
        /// Exact delivered content.
        content: String,
    },
    /// Exact child result delivered through one registered wait.
    DelegationResult {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Await request receiving this result.
        awaiting_request: ToolRequestId,
        /// Relationship identity.
        spawning_request: ToolRequestId,
        /// Terminal child session.
        child: SessionId,
        /// Foreground or background delivery mode.
        mode: DispatchedDelegationWaitMode,
        /// Recipient-wide position for background delivery only.
        delivery_sequence: Option<u64>,
        /// Typed terminal result outcome.
        outcome: DispatchedDelegationOutcome,
        /// Delivered content for a successful result only.
        content: Option<String>,
        /// Typed lifecycle reason.
        reason: DispatchedDelegationReason,
        /// Exact child-turn or parent-command proof.
        provenance: DispatchedDelegationProvenance,
    },
    /// Injected boundary declaring the model identity newly in force.
    ModelIdentityChanged {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Turn whose start first observes the identity.
        turn: TurnId,
        /// Immutable defaults epoch bound by that turn.
        defaults_version: u64,
        /// Exact direct model identity frozen for that turn.
        selected: DirectModelSelection,
    },
    /// Model-produced summary of one exact earlier semantic range.
    ContextSummary {
        /// Zero-based position in the complete frontier.
        entry_index: u64,
        /// Session that owns the immutable summary entry.
        source_session: SessionId,
        /// Semantic summary-entry identity.
        entry: SemanticTranscriptEntryId,
        /// Dedicated producing model call.
        model_call: ModelCallId,
        /// Inclusive summarized-range first entry.
        first: signalbox_domain::SemanticTranscriptEntryRef,
        /// Inclusive summarized-range final entry.
        through: signalbox_domain::SemanticTranscriptEntryRef,
        /// Exact model-produced summary text.
        content: String,
    },
    /// Exact accepted user input.
    User {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Accepted-input identity.
        accepted_input: AcceptedInputId,
        /// Origin turn.
        turn: TurnId,
        /// Exact admitted ordered user content.
        content: UserContent,
    },
    /// Exact committed assistant text.
    Assistant {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Owning turn.
        turn: TurnId,
        /// Producing model call.
        model_call: ModelCallId,
        /// Exact committed assistant text.
        content: String,
    },
    /// Assistant tool proposal.
    AssistantToolUse {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Owning turn.
        turn: TurnId,
        /// Producing model call.
        model_call: ModelCallId,
        /// Exact logical tool request.
        request: ToolRequestId,
        /// Exact stored tool name.
        name: String,
        /// Exact stored normalized or scrubbed undecodable arguments.
        arguments: String,
        /// Explicit decision provenance, absent while pending and for automatic policy.
        approval: Option<ProcessToolApproval>,
    },
    /// Executed tool result reference.
    ToolExecutionResult {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Exact logical tool request.
        request: ToolRequestId,
        /// Exact physical tool attempt.
        attempt: ToolAttemptId,
        /// Typed terminal outcome of the exact physical attempt.
        disposition: ProcessToolExecutionResultDisposition,
        /// Exact provider-visible result content.
        content: String,
    },
    /// User or policy denied one tool request.
    ToolDenied {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Exact denied request.
        request: ToolRequestId,
        /// Exact provider-visible denial content.
        content: String,
    },
    /// The turn ended before one tool request resolved ordinarily.
    ToolClosed {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Exact closed request.
        request: ToolRequestId,
        /// Exact provider-visible terminal-closure content.
        content: String,
    },
    /// Explicit failed-turn marker.
    TurnFailed {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Failed turn.
        turn: TurnId,
    },
    /// Explicit completed-turn marker.
    TurnCompleted {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Completed turn.
        turn: TurnId,
    },
    /// Explicit cancelled-turn marker.
    TurnCancelled {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Cancelled turn.
        turn: TurnId,
    },
    /// Imported text whose value was explicitly source-attested.
    ImportedText {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the projected semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Owning imported conversation.
        imported_conversation: ImportedConversationId,
        /// Exact imported entry identity.
        imported_entry: ImportedTranscriptEntryId,
        /// Exact source-speaker attestation.
        source_speaker: ProcessImportedSourceSpeaker,
        /// Exact source-attested text.
        content: String,
    },
    /// Conservative imported entry without rendered text.
    Imported {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the projected semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Owning imported conversation.
        imported_conversation: ImportedConversationId,
        /// Exact imported entry identity.
        imported_entry: ImportedTranscriptEntryId,
        /// Exact source-speaker attestation.
        source_speaker: ProcessImportedSourceSpeaker,
        /// Conservative normalized content kind.
        content_kind: ProcessImportedContentKind,
    },
}

/// One explicit approval decision projected with an assistant tool proposal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessToolApproval {
    decision: ToolApprovalDecision,
    decider: ToolApprovalDecider,
    rationale: Option<ToolDecisionRationale>,
}

impl ProcessToolApproval {
    /// Borrows the exact recorded decision.
    pub const fn decision(&self) -> &ToolApprovalDecision {
        &self.decision
    }

    /// Returns the exact user or delegate provenance.
    pub const fn decider(&self) -> ToolApprovalDecider {
        self.decider
    }

    /// Borrows the delegate rationale, absent for a user decision.
    pub const fn rationale(&self) -> Option<&ToolDecisionRationale> {
        self.rationale.as_ref()
    }
}

/// One complete transcript and cursor observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessTranscriptSnapshot {
    session: SessionId,
    cursor: u64,
    runner: Option<ProcessRunnerProjection>,
    turns: Vec<ProcessTranscriptTurn>,
    model_call_usage: Vec<ProcessTranscriptModelCallUsage>,
    entries: Vec<ProcessTranscriptEntry>,
}

impl ProcessTranscriptSnapshot {
    /// Returns the selected session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the global last committed outbox sequence from this snapshot.
    pub const fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Borrows the current runner placement, absent for a daemon-only session.
    pub const fn runner(&self) -> Option<&ProcessRunnerProjection> {
        self.runner.as_ref()
    }

    /// Borrows turns in immutable acceptance order.
    pub fn turns(&self) -> &[ProcessTranscriptTurn] {
        &self.turns
    }

    /// Borrows terminal model-call usage in turn and call identity order.
    pub fn model_call_usage(&self) -> &[ProcessTranscriptModelCallUsage] {
        &self.model_call_usage
    }

    /// Borrows the latest semantic frontier in member order.
    pub fn entries(&self) -> &[ProcessTranscriptEntry] {
        &self.entries
    }
}

/// One bounded-memory item yielded from a repeatable-read transcript snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessTranscriptItem {
    /// One turn in acceptance order.
    Turn(ProcessTranscriptTurn),
    /// One terminal model call's typed token evidence.
    ModelCallUsage(ProcessTranscriptModelCallUsage),
    /// One semantic entry in frontier order.
    Entry(ProcessTranscriptEntry),
}

/// Counts and cursor observed after a transcript reader reaches its committed
/// end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessTranscriptSummary {
    session: SessionId,
    cursor: u64,
    turn_count: u64,
    model_call_count: u64,
    entry_count: u64,
}

impl ProcessTranscriptSummary {
    /// Returns the selected session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the global outbox cursor from the repeatable-read snapshot.
    pub const fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Returns the exact number of yielded turns.
    pub const fn turn_count(&self) -> u64 {
        self.turn_count
    }

    /// Returns the exact number of yielded terminal model calls.
    pub const fn model_call_count(&self) -> u64 {
        self.model_call_count
    }

    /// Returns the exact number of yielded semantic entries.
    pub const fn entry_count(&self) -> u64 {
        self.entry_count
    }
}

/// One repeatable-read transcript cursor that owns at most one decoded row.
///
/// Call [`Self::next_item`] until it returns `None`. That terminal call commits
/// the read-only transaction and makes [`Self::summary`] available. Dropping a
/// reader early rolls its transaction back.
#[derive(Debug)]
pub struct ProcessTranscriptReader {
    transaction: Option<Transaction<'static, Postgres>>,
    session: SessionId,
    cursor: u64,
    runner: Option<ProcessRunnerProjection>,
    lineage_tip: Option<TurnId>,
    latest_frontier: Option<ContextFrontierId>,
    expected_turn_count: u64,
    turn_count: u64,
    next_turn_after: Option<u64>,
    turns_complete: bool,
    expected_model_call_count: u64,
    model_call_count: u64,
    next_model_call_after: Option<(u64, ModelCallId)>,
    model_calls_complete: bool,
    entry_count: Option<u64>,
    next_entry_index: u64,
    summary: Option<ProcessTranscriptSummary>,
    automatic_reconciliation_attempt_budget: Option<Option<u32>>,
}

impl ProcessTranscriptReader {
    /// Returns the selected session while the reader is active.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Borrows the current runner placement from this same repeatable-read snapshot.
    pub const fn runner(&self) -> Option<&ProcessRunnerProjection> {
        self.runner.as_ref()
    }

    /// Returns the snapshot's global outbox cursor.
    pub const fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Returns the committed summary only after [`Self::next_item`] returned
    /// `None`.
    pub const fn summary(&self) -> Option<ProcessTranscriptSummary> {
        self.summary
    }

    /// Yields one turn, model-call usage record, or entry without retaining
    /// prior decoded rows.
    pub async fn next_item(&mut self) -> Result<Option<ProcessTranscriptItem>, ProcessReadError> {
        if self.summary.is_some() {
            return Ok(None);
        }

        if !self.turns_complete {
            let session = self.session;
            let next_turn_after = self.next_turn_after;
            let row = load_next_transcript_turn(self.transaction_mut()?, session, next_turn_after)
                .await?;
            if let Some(row) = row {
                let decoded =
                    decode_transcript_turn(&row, self.automatic_reconciliation_attempt_budget)?;
                match (decoded.start_lineage, decoded.latest_frontier) {
                    (None, None) => {}
                    (Some(_), Some(frontier)) => {
                        if Some(decoded.turn.turn()) == self.lineage_tip
                            && self.latest_frontier.replace(frontier).is_some()
                        {
                            return Err(ProcessReadCorruption::Inconsistent(
                                "turn execution lineage",
                            )
                            .into());
                        }
                    }
                    _ => {
                        return Err(ProcessReadCorruption::Inconsistent(
                            "started turn frontier shape",
                        )
                        .into());
                    }
                }
                self.next_turn_after = Some(decoded.turn.acceptance_position());
                self.turn_count =
                    self.turn_count
                        .checked_add(1)
                        .ok_or(ProcessReadCorruption::InvalidOrdinal(
                            "transcript turn count",
                        ))?;
                return Ok(Some(ProcessTranscriptItem::Turn(decoded.turn)));
            }
            if self.turn_count != self.expected_turn_count {
                return Err(ProcessReadCorruption::Inconsistent("turn acceptance ordering").into());
            }
            if self.lineage_tip.is_some() && self.latest_frontier.is_none() {
                return Err(ProcessReadCorruption::Inconsistent("turn execution lineage").into());
            }
            self.turns_complete = true;
        }

        if !self.model_calls_complete {
            let session = self.session;
            let next_model_call_after = self.next_model_call_after;
            let row =
                load_next_model_call_usage(self.transaction_mut()?, session, next_model_call_after)
                    .await?;
            if let Some(row) = row {
                let (acceptance_position, usage) = decode_model_call_usage(&row)?;
                self.next_model_call_after = Some((acceptance_position, usage.call()));
                self.model_call_count = self.model_call_count.checked_add(1).ok_or(
                    ProcessReadCorruption::InvalidOrdinal("transcript model-call count"),
                )?;
                return Ok(Some(ProcessTranscriptItem::ModelCallUsage(usage)));
            }
            if self.model_call_count != self.expected_model_call_count {
                return Err(
                    ProcessReadCorruption::Inconsistent("terminal model-call ordering").into(),
                );
            }
            self.model_calls_complete = true;
        }

        if self.entry_count.is_none() {
            let session = self.session;
            let current_frontier = self.latest_frontier;
            let latest_frontier = advance_through_latest_compaction(
                self.transaction_mut()?,
                session,
                current_frontier,
            )
            .await?;
            self.latest_frontier = latest_frontier;
            self.entry_count = Some(match latest_frontier {
                Some(frontier) => {
                    open_transcript_entry_cursor(self.transaction_mut()?, session, frontier).await?
                }
                None => 0,
            });
        }

        let entry_count = self
            .entry_count
            .ok_or(ProcessReadCorruption::Missing("transcript entry count"))?;
        if self.latest_frontier.is_some() {
            let entry_index = self.next_entry_index;
            if let Some(entry) =
                fetch_next_transcript_entry(self.transaction_mut()?, entry_index, entry_count)
                    .await?
            {
                if entry_index >= entry_count {
                    return Err(ProcessReadCorruption::Inconsistent(
                        "context frontier declared membership",
                    )
                    .into());
                }
                self.next_entry_index = self.next_entry_index.checked_add(1).ok_or(
                    ProcessReadCorruption::InvalidOrdinal("transcript entry index"),
                )?;
                return Ok(Some(ProcessTranscriptItem::Entry(entry)));
            }
        }
        if self.next_entry_index != entry_count {
            return Err(ProcessReadCorruption::Inconsistent(
                "context frontier declared membership",
            )
            .into());
        }

        let transaction = self
            .transaction
            .take()
            .ok_or(ProcessReadCorruption::Missing("process read transaction"))?;
        transaction.commit().await?;
        self.summary = Some(ProcessTranscriptSummary {
            session: self.session,
            cursor: self.cursor,
            turn_count: self.turn_count,
            model_call_count: self.model_call_count,
            entry_count,
        });
        Ok(None)
    }

    fn transaction_mut(&mut self) -> Result<&mut Transaction<'static, Postgres>, ProcessReadError> {
        self.transaction
            .as_mut()
            .ok_or_else(|| ProcessReadCorruption::Missing("process read transaction").into())
    }
}

async fn advance_through_latest_compaction(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
    current: Option<ContextFrontierId>,
) -> Result<Option<ContextFrontierId>, ProcessReadError> {
    let latest: Option<Uuid> = sqlx::query_scalar(
        "SELECT candidate.result_frontier_id
           FROM context_compaction AS candidate
          WHERE candidate.session_id = $1
            AND NOT EXISTS (
                SELECT 1
                  FROM context_compaction AS successor
                 WHERE successor.session_id = candidate.session_id
                   AND successor.predecessor_compaction_id =
                           candidate.context_compaction_id
            )",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(latest) = latest.map(ContextFrontierId::from_uuid) else {
        return Ok(current);
    };
    let Some(current) = current else {
        return Ok(Some(latest));
    };
    if current == latest {
        return Ok(Some(current));
    }
    let row: (bool, bool) = sqlx::query_as(
        "SELECT
            NOT EXISTS (
                SELECT 1
                  FROM resolve_context_frontier_members($1, $2) AS earlier
                  LEFT JOIN resolve_context_frontier_members($1, $3) AS later
                    ON later.member_position = earlier.member_position
                   AND later.source_session_id = earlier.source_session_id
                   AND later.semantic_entry_id = earlier.semantic_entry_id
                 WHERE later.member_position IS NULL
            ),
            NOT EXISTS (
                SELECT 1
                  FROM resolve_context_frontier_members($1, $3) AS earlier
                  LEFT JOIN resolve_context_frontier_members($1, $2) AS later
                    ON later.member_position = earlier.member_position
                   AND later.source_session_id = earlier.source_session_id
                   AND later.semantic_entry_id = earlier.semantic_entry_id
                 WHERE later.member_position IS NULL
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(current.into_uuid())
    .bind(latest.into_uuid())
    .fetch_one(&mut **transaction)
    .await?;
    match row {
        (true, false) => Ok(Some(latest)),
        (false, true) => Ok(Some(current)),
        _ => {
            Err(ProcessReadCorruption::Inconsistent("turn and compaction frontier lineage").into())
        }
    }
}

/// A committed read shape that cannot form the closed process projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessReadCorruption {
    /// One required row or field was absent.
    Missing(&'static str),
    /// A closed storage discriminator had no admitted mapping.
    Unsupported {
        /// Storage field containing the discriminator.
        field: &'static str,
        /// Unsupported durable spelling.
        value: String,
    },
    /// Related durable fields disagreed.
    Inconsistent(&'static str),
    /// A stored ordinal was not an admitted unsigned integer.
    InvalidOrdinal(&'static str),
}

impl fmt::Display for ProcessReadCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "process read is missing {field}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "process read has unsupported {field}: {value}")
            }
            Self::Inconsistent(relationship) => {
                write!(formatter, "process read has inconsistent {relationship}")
            }
            Self::InvalidOrdinal(field) => {
                write!(formatter, "process read has invalid {field}")
            }
        }
    }
}

impl Error for ProcessReadCorruption {}

/// PostgreSQL failure or fail-closed projection corruption.
#[derive(Debug)]
pub enum ProcessReadError {
    /// PostgreSQL could not complete the repeatable-read transaction.
    Database(sqlx::Error),
    /// Committed rows could not form the closed projection.
    Corruption(ProcessReadCorruption),
}

impl fmt::Display for ProcessReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(_) => formatter.write_str("process read database operation failed"),
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for ProcessReadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for ProcessReadError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<ProcessReadCorruption> for ProcessReadError {
    fn from(error: ProcessReadCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// PostgreSQL-backed process read boundary.
#[derive(Clone, Debug)]
pub struct ProcessReadRepository {
    pool: PgPool,
    automatic_reconciliation_attempt_budget: Option<Option<u32>>,
}

impl ProcessReadRepository {
    /// Uses the supplied pool for independent repeatable-read snapshots.
    pub const fn new(pool: PgPool) -> Self {
        Self {
            pool,
            automatic_reconciliation_attempt_budget: None,
        }
    }

    /// Applies the deployment's optional automatic reconciliation budget.
    pub const fn with_automatic_reconciliation_attempt_budget(
        mut self,
        budget: Option<u32>,
    ) -> Self {
        self.automatic_reconciliation_attempt_budget = Some(budget);
        self
    }

    /// Reads one complete current or named immutable session-defaults epoch.
    ///
    /// A `None` version selects the epoch named by the session's current
    /// pointer; a named version selects exactly that immutable epoch. The
    /// read is one statement-consistent SELECT. For an existing session, a
    /// missing current pointer or missing pointed-at epoch fails closed as
    /// corruption; only a named version that was never installed is the typed
    /// absent-version outcome.
    pub async fn read_session_defaults(
        &self,
        session: SessionId,
        version: Option<signalbox_domain::SessionConfigurationDefaultsVersion>,
    ) -> Result<ProcessSessionDefaultsRead, ProcessReadError> {
        let named = version.map(|value| Decimal::from(value.as_u64()));
        let row = sqlx::query(
            "SELECT
                session_row.session_id,
                current_defaults.current_version,
                current_epoch.version AS current_epoch_version,
                selected_defaults.version AS selected_version,
                selected_defaults.model_selection_kind,
                selected_defaults.direct_model_selection_id,
                selected_defaults.model_alias_id,
                selected_defaults.dangerous_tool_auto_approval,
                selected_defaults.system_prompt,
                selected_defaults.model_settings
               FROM session AS session_row
               LEFT JOIN session_current_defaults AS current_defaults
                 ON current_defaults.session_id = session_row.session_id
               LEFT JOIN session_defaults_version AS current_epoch
                 ON current_epoch.session_id = session_row.session_id
                AND current_epoch.version = current_defaults.current_version
               LEFT JOIN session_defaults_version AS selected_defaults
                 ON selected_defaults.session_id = session_row.session_id
                AND selected_defaults.version =
                        COALESCE($2, current_defaults.current_version)
              WHERE session_row.session_id = $1",
        )
        .bind(session_id_to_uuid(session))
        .bind(named)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(ProcessSessionDefaultsRead::SessionNotFound);
        };
        // An existing session must carry a current pointer that resolves to
        // an installed epoch even when a named historical epoch is selected:
        // a missing pointer or a dangling pointer is corruption, not a
        // servable read ().
        let current_version: Option<Decimal> = row.try_get("current_version")?;
        if current_version.is_none() {
            return Err(ProcessReadCorruption::Missing("current defaults pointer").into());
        }
        let current_epoch_version: Option<Decimal> = row.try_get("current_epoch_version")?;
        if current_epoch_version.is_none() {
            return Err(ProcessReadCorruption::Missing("current defaults epoch").into());
        }
        let selected_version: Option<Decimal> = row.try_get("selected_version")?;
        let Some(selected_version) = selected_version else {
            return if named.is_some() {
                Ok(ProcessSessionDefaultsRead::VersionNotFound)
            } else {
                Err(ProcessReadCorruption::Missing("current defaults epoch").into())
            };
        };
        let selected_version = signalbox_domain::SessionConfigurationDefaultsVersion::try_from_u64(
            u64::try_from(selected_version)
                .map_err(|_| ProcessReadCorruption::InvalidOrdinal("selected_version"))?,
        )
        .ok_or(ProcessReadCorruption::InvalidOrdinal("selected_version"))?;
        let defaults = decode_session_defaults_value(&row)?;
        Ok(ProcessSessionDefaultsRead::Read(ProcessSessionDefaults {
            session,
            version: selected_version,
            defaults,
        }))
    }

    /// Collects every current session summary in session-identity order.
    ///
    /// Production process serving uses [`Self::open_session_summaries`] to
    /// avoid retaining the complete catalog in request memory.
    pub async fn list_sessions(&self) -> Result<Vec<ProcessSessionSummary>, ProcessReadError> {
        let mut reader = self.open_session_summaries().await?;
        let mut summaries = Vec::new();
        while let Some(summary) = reader.next_summary().await? {
            summaries.push(summary);
        }
        Ok(summaries)
    }

    /// Opens one repeatable-read session-summary cursor.
    ///
    /// The cursor yields at most one decoded summary at a time.
    pub async fn open_session_summaries(
        &self,
    ) -> Result<ProcessSessionSummaryReader, ProcessReadError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(REPEATABLE_READ_ONLY)
            .execute(&mut *transaction)
            .await?;
        Ok(ProcessSessionSummaryReader {
            transaction: Some(transaction),
            next_session_after: None,
            pending: VecDeque::new(),
            summary_count: 0,
            committed_summary_count: None,
        })
    }

    /// Reads the selected session's immutable ancestry, or `None` when absent.
    ///
    /// This narrow read lets a process adapter reject a representation that
    /// cannot carry imported ancestry before constructing or mutating it.
    pub async fn session_ancestry(
        &self,
        requested_session: SessionId,
    ) -> Result<Option<ProcessSessionAncestry>, ProcessReadError> {
        let row = sqlx::query(
            "SELECT ancestry_kind
               FROM session
              WHERE session_id = $1",
        )
        .bind(session_id_to_uuid(requested_session))
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| decode_process_session_ancestry(&row))
            .transpose()
    }

    /// Returns whether the selected session has durable tool-only history.
    ///
    /// This narrow read reports whether tool-only transcript evidence exists.
    pub async fn session_has_tool_history(
        &self,
        requested_session: SessionId,
    ) -> Result<bool, ProcessReadError> {
        sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1
                   FROM tool_request
                  WHERE session_id = $1
             )",
        )
        .bind(session_id_to_uuid(requested_session))
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// Returns the session owning the named logical tool request, or `None`
    /// when no request has that identity.
    ///
    /// This narrow read lets a process adapter refuse a decision whose named
    /// session does not own the named request before a durable command is
    /// recorded; the canonical decision command remains the authority for
    /// every recorded outcome.
    pub async fn tool_request_session(
        &self,
        request: ToolRequestId,
    ) -> Result<Option<SessionId>, ProcessReadError> {
        let row = sqlx::query_scalar::<_, Uuid>(
            "SELECT session_id
               FROM tool_request
              WHERE request_id = $1",
        )
        .bind(request.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(SessionId::from_uuid))
    }

    /// Reads whether the session exists and, when it does, whether its active
    /// turn is parked on the model-call recovery wait.
    ///
    /// This narrow read lets a process adapter refuse a reconciliation request
    /// whose named turn owes no user decision, before recording a durable
    /// command. It is a precondition, never authority: the authoritative
    /// transaction revalidates the exact expected active turn under the
    /// session lock, and an ended attempt never returns to a live phase, so an
    /// admitted wait can only stay parked or terminalize before that
    /// transaction runs. An absent session is reported separately so the
    /// adapter can leave that case to the authoritative transaction's own
    /// typed rejection instead of collapsing it into a missing wait.
    pub async fn model_call_recovery_precondition(
        &self,
        requested_session: SessionId,
    ) -> Result<ProcessModelCallRecoveryPrecondition, ProcessReadError> {
        let row: Option<(bool, Option<Uuid>)> = sqlx::query_as(
            "SELECT TRUE,
                    (SELECT turn_id
                       FROM turn_lifecycle
                      WHERE session_id = session.session_id
                        AND state_kind = 'active'
                        AND NOT delegation_runtime_terminal
                        AND active_phase_kind = 'awaiting_model_call_recovery')
               FROM session
              WHERE session_id = $1",
        )
        .bind(session_id_to_uuid(requested_session))
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            None => ProcessModelCallRecoveryPrecondition::SessionAbsent,
            Some((_, None)) => ProcessModelCallRecoveryPrecondition::NoParkedTurn,
            Some((_, Some(turn))) => ProcessModelCallRecoveryPrecondition::Parked {
                turn: TurnId::from_uuid(turn),
            },
        })
    }

    /// Reads only the exact source-qualified semantic entries selected for a
    /// compaction range, preserving their one-based physical positions.
    pub async fn read_selected_transcript_entries(
        &self,
        positions: &[u64],
        references: &[SemanticTranscriptEntryRef],
    ) -> Result<Box<[ProcessTranscriptEntry]>, ProcessReadError> {
        if positions.is_empty() || positions.len() != references.len() {
            return Err(
                ProcessReadCorruption::Inconsistent("selected transcript range shape").into(),
            );
        }
        let stored_positions = positions
            .iter()
            .copied()
            .map(Decimal::from)
            .collect::<Vec<_>>();
        let source_sessions = references
            .iter()
            .map(|reference| session_id_to_uuid(reference.source_session()))
            .collect::<Vec<_>>();
        let entry_ids = references
            .iter()
            .map(|reference| reference.entry().into_uuid())
            .collect::<Vec<_>>();
        let rows = sqlx::query(
            "SELECT
                selected.member_position,
                selected.source_session_id,
                selected.semantic_entry_id,
                entry.payload_kind,
                entry.origin_accepted_input_id,
                entry.steering_source_turn_id,
                entry.failed_turn_id,
                entry.assistant_text_value,
                entry.producing_model_call_id,
                entry.assistant_tool_request_id,
                entry.tool_result_request_id,
                entry.tool_result_attempt_id,
                entry.completed_turn_id,
                entry.cancelled_turn_id,
                entry.imported_conversation_id,
                entry.imported_transcript_entry_id,
                entry.model_identity_turn_id,
                entry.model_identity_defaults_version,
                entry.model_identity_direct_selection_id,
                entry.context_summary_value,
                entry.context_summary_producing_call_id,
                entry.context_summary_first_source_session_id,
                entry.context_summary_first_entry_id,
                entry.context_summary_through_source_session_id,
                entry.context_summary_through_entry_id,
                entry.delegated_task_spawning_tool_request_id,
                entry.delegation_message_id,
                entry.delegation_result_awaiting_tool_request_id,
                entry.delegation_result_spawning_tool_request_id,
                delegated_task.task_content AS delegated_task_content,
                task_relation.parent_session_id AS delegated_task_parent_session_id,
                task_relation.parent_turn_id AS delegated_task_parent_turn_id,
                delegated_message.spawning_tool_request_id AS delegation_message_spawning_request_id,
                delegated_message.event_ordinal AS delegation_message_ordinal,
                delegated_message.content_text AS delegation_message_content,
                message_delivery.recipient_session_id AS delegation_message_recipient_session_id,
                message_delivery.delivery_sequence AS delegation_message_delivery_sequence,
                CASE delegated_message.direction
                    WHEN 'parent_to_child' THEN message_relation.parent_session_id
                    WHEN 'child_to_parent' THEN message_relation.child_session_id
                END AS delegation_message_sender_session_id,
                delegated_wait.child_session_id AS delegation_result_child_session_id,
                delegated_wait.wait_mode AS delegation_result_wait_mode,
                result_delivery.delivery_sequence AS delegation_result_delivery_sequence,
                delegated_result.outcome_kind AS delegation_result_outcome_kind,
                delegated_result.content_text AS delegation_result_content,
                result_event.reason_kind AS delegation_result_reason_kind,
                result_event.provenance_kind,
                result_event.provenance_session_id,
                result_event.provenance_turn_id,
                result_event.provenance_goal_generation,
                result_event.provenance_command_id,
                imported.source_speaker_kind AS imported_source_speaker_kind,
                imported.content_encoding AS imported_content_encoding,
                CASE WHEN accepted.accepted_input_id IS NULL THEN NULL
                     ELSE accepted_input_content_parts_json(
                        accepted.accepted_input_id)
                END AS origin_content,
                accepted.origin_turn_id,
                call.turn_id AS assistant_turn_id,
                result_attempt.request_id AS result_attempt_request_id,
                transcript_request.tool_name AS transcript_tool_name,
                transcript_request.arguments_text AS transcript_tool_arguments,
                result_attempt.terminal_disposition_kind AS result_disposition,
                result_attempt.result_text AS result_text,
                result_attempt.error_kind AS result_error_kind,
                result_attempt.error_detail AS result_error_detail,
                transcript_approval.decision_kind AS transcript_decision_kind,
                transcript_approval.decision_source AS transcript_decision_source,
                transcript_approval.denial_reason AS transcript_denial_reason,
                transcript_approval.user_command_id AS transcript_user_command_id,
                transcript_approval.delegate_model_selection_id AS transcript_delegate_model_selection_id,
                transcript_approval.delegate_model_call_id AS transcript_delegate_model_call_id,
                transcript_approval.rationale AS transcript_decision_rationale,
                transcript_approval.override_denied_request_id
                    AS transcript_override_denied_request_id,
                transcript_override.command_id AS transcript_override_command_id
               FROM UNNEST($1::numeric[], $2::uuid[], $3::uuid[])
                    WITH ORDINALITY AS selected(
                        member_position,
                        source_session_id,
                        semantic_entry_id,
                        selected_ordinal
                    )
               JOIN semantic_transcript_entry AS entry
                 ON entry.source_session_id = selected.source_session_id
                AND entry.semantic_entry_id = selected.semantic_entry_id
               LEFT JOIN accepted_input AS accepted
                 ON accepted.session_id = entry.source_session_id
                AND accepted.accepted_input_id = entry.origin_accepted_input_id
               LEFT JOIN model_call AS call
                 ON call.session_id = entry.source_session_id
                AND call.model_call_id = entry.producing_model_call_id
               LEFT JOIN tool_attempt AS result_attempt
                 ON result_attempt.session_id = entry.source_session_id
                AND result_attempt.attempt_id = entry.tool_result_attempt_id
               LEFT JOIN tool_request AS transcript_request
                 ON transcript_request.session_id = entry.source_session_id
                AND transcript_request.request_id = COALESCE(
                    entry.assistant_tool_request_id,
                    entry.tool_result_request_id,
                    result_attempt.request_id
                )
               LEFT JOIN tool_approval_decision AS transcript_approval
                 ON transcript_approval.request_id = transcript_request.request_id
               LEFT JOIN tool_approval_user_override AS transcript_override
                 ON transcript_override.denied_request_id =
                    transcript_approval.override_denied_request_id
               LEFT JOIN imported_transcript_entry AS imported
                 ON imported.imported_conversation_id =
                        entry.imported_conversation_id
                AND imported.imported_transcript_entry_id =
                        entry.imported_transcript_entry_id
               LEFT JOIN session_delegation_initial_task AS delegated_task
                 ON delegated_task.spawning_tool_request_id =
                        entry.delegated_task_spawning_tool_request_id
                AND delegated_task.child_session_id = entry.source_session_id
                AND delegated_task.semantic_entry_id = entry.semantic_entry_id
               LEFT JOIN session_delegation AS task_relation
                 ON task_relation.spawning_tool_request_id =
                        delegated_task.spawning_tool_request_id
               LEFT JOIN session_message_delivery AS message_delivery
                 ON message_delivery.message_id = entry.delegation_message_id
                AND message_delivery.recipient_session_id = entry.source_session_id
               LEFT JOIN session_message AS delegated_message
                 ON delegated_message.message_id = message_delivery.message_id
                AND delegated_message.spawning_tool_request_id =
                        message_delivery.spawning_tool_request_id
               LEFT JOIN session_delegation AS message_relation
                 ON message_relation.spawning_tool_request_id =
                        delegated_message.spawning_tool_request_id
               LEFT JOIN session_child_result_delivery AS result_delivery
                 ON result_delivery.awaiting_tool_request_id =
                        entry.delegation_result_awaiting_tool_request_id
                AND result_delivery.spawning_tool_request_id =
                        entry.delegation_result_spawning_tool_request_id
                AND result_delivery.parent_session_id = entry.source_session_id
               LEFT JOIN session_delegation_wait AS delegated_wait
                 ON delegated_wait.awaiting_tool_request_id =
                        result_delivery.awaiting_tool_request_id
                AND delegated_wait.spawning_tool_request_id =
                        result_delivery.spawning_tool_request_id
                AND delegated_wait.parent_session_id = result_delivery.parent_session_id
               LEFT JOIN session_child_result AS delegated_result
                 ON delegated_result.spawning_tool_request_id =
                        result_delivery.spawning_tool_request_id
               LEFT JOIN session_delegation_event AS result_event
                 ON result_event.spawning_tool_request_id =
                        delegated_result.spawning_tool_request_id
                AND result_event.event_ordinal = delegated_result.event_ordinal
                AND result_event.event_kind = delegated_result.event_kind
              ORDER BY selected.selected_ordinal",
        )
        .bind(&stored_positions)
        .bind(&source_sessions)
        .bind(&entry_ids)
        .fetch_all(&self.pool)
        .await?;
        if rows.len() != references.len() {
            return Err(ProcessReadCorruption::Inconsistent(
                "selected transcript range membership",
            )
            .into());
        }
        let mut entries = Vec::with_capacity(rows.len());
        for ((row, expected_position), expected_reference) in
            rows.iter().zip(positions).zip(references)
        {
            let stored_position = decode_positive(
                required(row, "member_position")?,
                "selected transcript member position",
            )?;
            let source_session = session_id_from_uuid(required(row, "source_session_id")?);
            let entry = SemanticTranscriptEntryId::from_uuid(required(row, "semantic_entry_id")?);
            if stored_position != *expected_position
                || SemanticTranscriptEntryRef::from_source(source_session, entry)
                    != *expected_reference
            {
                return Err(
                    ProcessReadCorruption::Inconsistent("selected transcript entry order").into(),
                );
            }
            let entry_index =
                stored_position
                    .checked_sub(1)
                    .ok_or(ProcessReadCorruption::InvalidOrdinal(
                        "selected transcript member position",
                    ))?;
            entries.push(decode_transcript_entry(row, entry_index)?);
        }
        Ok(entries.into_boxed_slice())
    }

    /// Reads one complete transcript snapshot, or `None` only when the session
    /// is absent from the shared transaction snapshot.
    pub async fn read_transcript(
        &self,
        requested_session: SessionId,
    ) -> Result<Option<ProcessTranscriptSnapshot>, ProcessReadError> {
        let Some(mut reader) = self.open_transcript(requested_session).await? else {
            return Ok(None);
        };
        let mut turns = Vec::new();
        let mut model_call_usage = Vec::new();
        let mut entries = Vec::new();
        while let Some(item) = reader.next_item().await? {
            match item {
                ProcessTranscriptItem::Turn(turn) => turns.push(turn),
                ProcessTranscriptItem::ModelCallUsage(usage) => model_call_usage.push(usage),
                ProcessTranscriptItem::Entry(entry) => entries.push(entry),
            }
        }
        let summary = reader
            .summary()
            .ok_or(ProcessReadCorruption::Missing("process transcript summary"))?;
        Ok(Some(ProcessTranscriptSnapshot {
            session: summary.session(),
            cursor: summary.cursor(),
            runner: reader.runner,
            turns,
            model_call_usage,
            entries,
        }))
    }

    /// Opens one repeatable-read transcript cursor, or `None` only when the
    /// session is absent from that transaction snapshot.
    ///
    /// The cursor yields at most one decoded turn, model-call usage record, or
    /// entry at a time. This is the production boundary for spooling snapshots
    /// without transcript-sized process memory.
    pub async fn open_transcript(
        &self,
        requested_session: SessionId,
    ) -> Result<Option<ProcessTranscriptReader>, ProcessReadError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(REPEATABLE_READ_ONLY)
            .execute(&mut *transaction)
            .await?;
        let session_exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM session WHERE session_id = $1)")
                .bind(session_id_to_uuid(requested_session))
                .fetch_one(&mut *transaction)
                .await?;
        if !session_exists {
            transaction.commit().await?;
            return Ok(None);
        }

        Ok(Some(
            open_transcript_in_transaction(
                transaction,
                requested_session,
                self.automatic_reconciliation_attempt_budget,
            )
            .await?,
        ))
    }

    /// Checks one requester's parent-directory scope and opens the target
    /// transcript within the same repeatable-read snapshot.
    pub async fn open_scoped_transcript(
        &self,
        requesting_session: SessionId,
        target_session: SessionId,
    ) -> Result<ProcessScopedTranscriptRead, ProcessReadError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(REPEATABLE_READ_ONLY)
            .execute(&mut *transaction)
            .await?;
        let Some(requesting_placement) =
            load_process_session_placement(&mut transaction, requesting_session).await?
        else {
            return Err(ProcessReadCorruption::Missing("requesting session placement").into());
        };
        let Some(target_placement) =
            load_process_session_placement(&mut transaction, target_session).await?
        else {
            transaction.commit().await?;
            return Ok(ProcessScopedTranscriptRead::TargetNotFound);
        };
        match requesting_placement
            .placement()
            .decide_cross_session_read(target_placement.placement())
        {
            SessionReadScopeDecision::Allowed => Ok(ProcessScopedTranscriptRead::Opened(Box::new(
                open_transcript_in_transaction(
                    transaction,
                    target_session,
                    self.automatic_reconciliation_attempt_budget,
                )
                .await?,
            ))),
            SessionReadScopeDecision::Refused(refusal) => {
                transaction.commit().await?;
                Ok(ProcessScopedTranscriptRead::Refused(refusal))
            }
        }
    }
}

async fn load_process_session_placement(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<Option<VersionedSessionPlacement>, ProcessReadError> {
    crate::session_placement::load_current(transaction, session)
        .await
        .map_err(map_session_placement_read_error)
}

fn map_session_placement_read_error(
    error: crate::session_placement::SessionPlacementRepositoryError,
) -> ProcessReadError {
    use crate::session_placement::SessionPlacementRepositoryError;

    match error {
        SessionPlacementRepositoryError::Database(error)
        | SessionPlacementRepositoryError::CommitAmbiguous(error) => {
            ProcessReadError::Database(error)
        }
        SessionPlacementRepositoryError::InvalidCommandId
        | SessionPlacementRepositoryError::Corruption(_) => {
            ProcessReadCorruption::Inconsistent("session placement").into()
        }
    }
}

async fn open_transcript_in_transaction(
    mut transaction: Transaction<'static, Postgres>,
    requested_session: SessionId,
    automatic_reconciliation_attempt_budget: Option<Option<u32>>,
) -> Result<ProcessTranscriptReader, ProcessReadError> {
    let stored_cursor: Option<Decimal> = sqlx::query_scalar(
        "SELECT last_sequence
               FROM outbox_sequence_state
              WHERE singleton",
    )
    .fetch_optional(&mut *transaction)
    .await?;
    let cursor = decode_nonnegative(
        stored_cursor.ok_or(ProcessReadCorruption::Missing("outbox sequence state"))?,
        "outbox cursor",
    )?;
    let runner = load_process_runner_projection(&mut transaction, requested_session).await?;
    let lineage_tip = load_execution_lineage_tip(&mut transaction, requested_session).await?;
    //  remains fail-closed on every transcript open: native lineage
    // supersedes the seed as the rendered frontier, not as an integrity fact.
    let imported_seed =
        load_checked_imported_seed_frontier(&mut transaction, requested_session).await?;
    let expected_turn_count =
        load_transcript_turn_count(&mut transaction, requested_session).await?;
    let expected_model_call_count =
        load_terminal_model_call_count(&mut transaction, requested_session).await?;
    Ok(ProcessTranscriptReader {
        transaction: Some(transaction),
        session: requested_session,
        cursor,
        runner,
        lineage_tip,
        latest_frontier: if lineage_tip.is_none() {
            imported_seed
        } else {
            None
        },
        expected_turn_count,
        turn_count: 0,
        next_turn_after: None,
        turns_complete: false,
        expected_model_call_count,
        model_call_count: 0,
        next_model_call_after: None,
        model_calls_complete: false,
        entry_count: None,
        next_entry_index: 0,
        summary: None,
        automatic_reconciliation_attempt_budget,
    })
}

pub(crate) async fn load_process_runner_projection(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<Option<ProcessRunnerProjection>, ProcessReadError> {
    let row = sqlx::query(
        "SELECT placement.selector_kind, placement.selector_runner_id,
                placement.selector_capability_class,
                placement.directory_selection_kind,
                placement.requested_working_directory,
                placement.requested_credential_profile_name,
                placement.workspace_requirement_kind,
                placement.requested_repository_key,
                placement.requested_sandbox_profile,
                placement.placement_revision, placement.state_kind,
                placement.pinned_runner_id, placement.lost_runner_id,
                placement.loss_source_kind,
                connection.state_kind AS connection_state_kind
           FROM runner_current_session_placement AS current_placement
           JOIN runner_session_placement_record AS placement
             ON placement.session_id = current_placement.session_id
            AND placement.event_ordinal = current_placement.event_ordinal
           LEFT JOIN LATERAL (
                SELECT state_kind
                  FROM runner_connection_event
                 WHERE enrollment_id = placement.registration_enrollment_id
                 ORDER BY connection_epoch DESC, event_ordinal DESC
                 LIMIT 1
           ) AS connection ON placement.state_kind = 'pinned'
          WHERE current_placement.session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };

    let selector_kind: String = required(&row, "selector_kind")?;
    let selector_runner: Option<Uuid> = row.try_get("selector_runner_id")?;
    let selector_capability: Option<String> = row.try_get("selector_capability_class")?;
    let selector = match (selector_kind.as_str(), selector_runner, selector_capability) {
        ("identity", Some(runner), None) => RunnerSelector::Identity(RunnerId::from_uuid(runner)),
        ("capability_class", None, Some(capability)) => RunnerSelector::CapabilityClass(
            RunnerCapabilityClass::try_new(capability)
                .map_err(|_| ProcessReadCorruption::Inconsistent("runner selector"))?,
        ),
        _ => return Err(ProcessReadCorruption::Inconsistent("runner selector").into()),
    };

    let directory_kind: String = required(&row, "directory_selection_kind")?;
    let requested_directory: Option<String> = row.try_get("requested_working_directory")?;
    let working_directory = match (directory_kind.as_str(), requested_directory) {
        ("runner_default", None) => None,
        ("exact", Some(directory)) => Some(
            RunnerWorkingDirectory::try_new(directory)
                .map_err(|_| ProcessReadCorruption::Inconsistent("runner working directory"))?,
        ),
        _ => {
            return Err(
                ProcessReadCorruption::Inconsistent("runner working directory selection").into(),
            );
        }
    };

    let workspace_kind: String = required(&row, "workspace_requirement_kind")?;
    let requested_repository: Option<String> = row.try_get("requested_repository_key")?;
    let repository = match (workspace_kind.as_str(), requested_repository) {
        ("none", None) => None,
        ("repository_worktree", Some(repository)) => Some(
            WorkspaceRepositoryKey::try_new(repository)
                .map_err(|_| ProcessReadCorruption::Inconsistent("runner repository key"))?,
        ),
        _ => return Err(ProcessReadCorruption::Inconsistent("runner workspace request").into()),
    };

    let credential_profile = row
        .try_get::<Option<String>, _>("requested_credential_profile_name")?
        .map(CredentialProfileName::try_new)
        .transpose()
        .map_err(|_| ProcessReadCorruption::Inconsistent("runner credential profile"))?;
    let sandbox_name: String = required(&row, "requested_sandbox_profile")?;
    let sandbox =
        runner_sandbox_from_str(&sandbox_name).ok_or(ProcessReadCorruption::Unsupported {
            field: "runner sandbox profile",
            value: sandbox_name,
        })?;
    let placement_revision = RunnerGeneration::try_from_u64(decode_positive(
        required(&row, "placement_revision")?,
        "runner placement revision",
    )?)
    .ok_or(ProcessReadCorruption::InvalidOrdinal(
        "runner placement revision",
    ))?;

    let state_kind: String = required(&row, "state_kind")?;
    let pinned_runner = row
        .try_get::<Option<Uuid>, _>("pinned_runner_id")?
        .map(RunnerId::from_uuid);
    let lost_runner = row
        .try_get::<Option<Uuid>, _>("lost_runner_id")?
        .map(RunnerId::from_uuid);
    let loss_source: Option<String> = row.try_get("loss_source_kind")?;
    let connection_state: Option<String> = row.try_get("connection_state_kind")?;
    let (runner, state) = match (
        state_kind.as_str(),
        pinned_runner,
        lost_runner,
        loss_source.as_deref(),
    ) {
        ("unpinned", None, None, None) => (None, ProcessRunnerProjectionState::Unpinned),
        ("pinned", Some(runner), None, None) => {
            (Some(runner), ProcessRunnerProjectionState::Pinned)
        }
        ("runner_lost_before_pin", None, Some(runner), None) => (
            Some(runner),
            ProcessRunnerProjectionState::RunnerLostBeforePin,
        ),
        ("runner_lost", Some(pinned), Some(lost), Some("connection" | "registration"))
            if pinned == lost =>
        {
            (Some(lost), ProcessRunnerProjectionState::RunnerLost)
        }
        ("runner_abandoned", None, Some(lost), None) => {
            (Some(lost), ProcessRunnerProjectionState::RunnerAbandoned)
        }
        ("runner_abandoned", Some(pinned), Some(lost), Some("connection" | "registration"))
            if pinned == lost =>
        {
            (Some(lost), ProcessRunnerProjectionState::RunnerAbandoned)
        }
        _ => return Err(ProcessReadCorruption::Inconsistent("runner placement state").into()),
    };
    let connection_health = match (state, connection_state.as_deref()) {
        (ProcessRunnerProjectionState::Pinned, Some("connected")) => {
            Some(ProcessRunnerConnectionHealth::Connected)
        }
        (ProcessRunnerProjectionState::Pinned, Some("suspect")) => {
            Some(ProcessRunnerConnectionHealth::Suspect)
        }
        (ProcessRunnerProjectionState::Pinned, Some("shutdown")) => {
            Some(ProcessRunnerConnectionHealth::Shutdown)
        }
        (ProcessRunnerProjectionState::Pinned, Some("lost")) => {
            Some(ProcessRunnerConnectionHealth::Lost)
        }
        (
            ProcessRunnerProjectionState::Unpinned
            | ProcessRunnerProjectionState::RunnerLostBeforePin
            | ProcessRunnerProjectionState::RunnerLost
            | ProcessRunnerProjectionState::RunnerAbandoned,
            None,
        ) => None,
        _ => return Err(ProcessReadCorruption::Inconsistent("runner connection health").into()),
    };

    Ok(Some(ProcessRunnerProjection {
        selector,
        runner,
        placement_revision,
        sandbox,
        credential_profile,
        repository,
        working_directory,
        connection_health,
        state,
    }))
}

fn decode_process_session_ancestry(
    row: &PgRow,
) -> Result<ProcessSessionAncestry, ProcessReadError> {
    let ancestry: String = required(row, "ancestry_kind")?;
    match ancestry.as_str() {
        "none" => Ok(ProcessSessionAncestry::UserInitiated),
        "imported_conversation" => Ok(ProcessSessionAncestry::ImportedConversation),
        _ => Err(ProcessReadCorruption::Unsupported {
            field: "session ancestry kind",
            value: ancestry,
        }
        .into()),
    }
}

async fn load_checked_imported_seed_frontier(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<Option<ContextFrontierId>, ProcessReadError> {
    sqlx::query("SELECT assert_imported_session_seed_complete($1)")
        .bind(session_id_to_uuid(session))
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_seed_validation_error)?;

    let row = sqlx::query(
        "SELECT
            session_row.ancestry_kind,
            seed.seed_context_frontier_id
           FROM session AS session_row
           LEFT JOIN imported_session_seed AS seed
             ON seed.session_id = session_row.session_id
          WHERE session_row.session_id = $1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_one(&mut **transaction)
    .await?;
    let ancestry = decode_process_session_ancestry(&row)?;
    let seed: Option<Uuid> = row.try_get("seed_context_frontier_id")?;
    match (ancestry, seed) {
        (ProcessSessionAncestry::UserInitiated, None) => Ok(None),
        (ProcessSessionAncestry::ImportedConversation, Some(frontier)) => {
            Ok(Some(ContextFrontierId::from_uuid(frontier)))
        }
        _ => Err(ProcessReadCorruption::Inconsistent("imported session seed shape").into()),
    }
}

fn map_seed_validation_error(error: sqlx::Error) -> ProcessReadError {
    let is_integrity_failure = error.as_database_error().is_some_and(|database| {
        matches!(
            database.code().as_deref(),
            Some("23000" | "23502" | "23503" | "23505" | "23514")
        )
    });
    if is_integrity_failure {
        ProcessReadCorruption::Inconsistent("imported session seed").into()
    } else {
        error.into()
    }
}

fn decode_pending_session_summary(
    row: &PgRow,
    placement: VersionedSessionPlacement,
) -> Result<PendingSessionSummary, ProcessReadError> {
    let session = session_id_from_uuid(required(row, "session_id")?);
    let defaults_version = decode_positive(
        required(row, "defaults_version")?,
        "current defaults version",
    )?;
    let kind: String = required(row, "model_selection_kind")?;
    let direct: Option<Uuid> = row.try_get("direct_model_selection_id")?;
    let alias: Option<Uuid> = row.try_get("model_alias_id")?;
    let model_selection = match (kind.as_str(), direct, alias) {
        ("direct", Some(selection), None) => {
            ProcessModelSelection::Direct(DirectModelSelection::from_uuid(selection))
        }
        ("alias", None, Some(alias)) => ProcessModelSelection::Alias(ModelAlias::from_uuid(alias)),
        ("direct" | "alias", _, _) => {
            return Err(ProcessReadCorruption::Inconsistent("model selection shape").into());
        }
        _ => {
            return Err(ProcessReadCorruption::Unsupported {
                field: "model selection kind",
                value: kind,
            }
            .into());
        }
    };
    Ok(PendingSessionSummary {
        session,
        defaults_version,
        model_selection,
        placement,
    })
}

struct DecodedTurn {
    turn: ProcessTranscriptTurn,
    start_lineage: Option<DecodedStartLineage>,
    latest_frontier: Option<ContextFrontierId>,
}

#[derive(Debug)]
enum DecodedTurnOrigin {
    AcceptedInput {
        accepted_input: AcceptedInputId,
        content: UserContent,
    },
    DelegatedTask {
        spawning_request: ToolRequestId,
        parent_session: SessionId,
        parent_turn: TurnId,
        content: String,
    },
    DelegationWake {
        first_delivery_sequence: u64,
        through_delivery_sequence: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecodedStartLineage {
    FirstInSession,
    After(TurnId),
}

async fn load_execution_lineage_tip(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<Option<TurnId>, ProcessReadError> {
    let row = sqlx::query(
        "WITH RECURSIVE
            started AS (
                SELECT
                    turn_id,
                    start_lineage_kind,
                    immediate_predecessor_turn_id
                  FROM turn_lifecycle
                 WHERE session_id = $1
                   AND state_kind IN ('active', 'terminal')
                   AND start_lineage_kind IS NOT NULL
            ),
            chain(turn_id) AS (
                SELECT turn_id
                  FROM started
                 WHERE start_lineage_kind = 'first_in_session'
                UNION
                SELECT child.turn_id
                  FROM started AS child
                  JOIN chain AS predecessor
                    ON child.start_lineage_kind = 'after'
                   AND child.immediate_predecessor_turn_id = predecessor.turn_id
            ),
            tips AS (
                SELECT candidate.turn_id
                  FROM started AS candidate
                 WHERE NOT EXISTS (
                    SELECT 1
                      FROM started AS successor
                     WHERE successor.start_lineage_kind = 'after'
                       AND successor.immediate_predecessor_turn_id = candidate.turn_id
                 )
            )
         SELECT
            (SELECT count(*) FROM started) AS started_count,
            (SELECT count(*) FROM started
              WHERE start_lineage_kind = 'first_in_session') AS root_count,
            (SELECT count(*) FROM chain) AS visited_count,
            (SELECT count(*) FROM tips) AS tip_count,
            EXISTS (
                SELECT 1
                  FROM started
                 WHERE start_lineage_kind = 'after'
                 GROUP BY immediate_predecessor_turn_id
                HAVING count(*) > 1
            ) AS branched,
            EXISTS (
                SELECT 1
                  FROM started AS child
                  LEFT JOIN started AS predecessor
                    ON predecessor.turn_id = child.immediate_predecessor_turn_id
                 WHERE child.start_lineage_kind = 'after'
                   AND predecessor.turn_id IS NULL
            ) AS missing_predecessor,
            (SELECT turn_id FROM tips LIMIT 1) AS tip_turn_id",
    )
    .bind(session_id_to_uuid(session))
    .fetch_one(&mut **transaction)
    .await?;
    decode_execution_lineage_tip(
        decode_database_count(&row, "started_count", "started turn count")?,
        decode_database_count(&row, "root_count", "root turn count")?,
        decode_database_count(&row, "visited_count", "visited turn count")?,
        decode_database_count(&row, "tip_count", "tip turn count")?,
        row.try_get("branched")?,
        row.try_get("missing_predecessor")?,
        row.try_get::<Option<Uuid>, _>("tip_turn_id")?
            .map(TurnId::from_uuid),
    )
}

fn decode_execution_lineage_tip(
    started_count: u64,
    root_count: u64,
    visited_count: u64,
    tip_count: u64,
    branched: bool,
    missing_predecessor: bool,
    tip: Option<TurnId>,
) -> Result<Option<TurnId>, ProcessReadError> {
    if started_count == 0 {
        return if root_count == 0
            && visited_count == 0
            && tip_count == 0
            && !branched
            && !missing_predecessor
            && tip.is_none()
        {
            Ok(None)
        } else {
            Err(ProcessReadCorruption::Inconsistent("turn execution lineage").into())
        };
    }
    if root_count != 1
        || visited_count != started_count
        || tip_count != 1
        || branched
        || missing_predecessor
    {
        return Err(ProcessReadCorruption::Inconsistent("turn execution lineage").into());
    }
    tip.map(Some)
        .ok_or_else(|| ProcessReadCorruption::Inconsistent("turn execution lineage").into())
}

async fn load_transcript_turn_count(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<u64, ProcessReadError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM turn_lifecycle AS turn
          WHERE turn.session_id = $1
            AND (
                goal_turn_is_runtime_relevant(turn.session_id, turn.turn_id)
                OR EXISTS (
                    SELECT 1
                      FROM session_delegation_logical_terminal AS logical_terminal
                     WHERE logical_terminal.child_session_id = turn.session_id
                       AND logical_terminal.child_turn_id = turn.turn_id
                )
            )",
    )
    .bind(session_id_to_uuid(session))
    .fetch_one(&mut **transaction)
    .await?;
    u64::try_from(count)
        .map_err(|_| ProcessReadCorruption::InvalidOrdinal("transcript turn count").into())
}

async fn load_terminal_model_call_count(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<u64, ProcessReadError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT
            (SELECT count(*)
               FROM model_call
              WHERE session_id = $1
                AND state_kind = 'terminal')
            +
            (SELECT count(*)
               FROM tool_approval_judge_model_call
              WHERE session_id = $1
                AND state_kind = 'terminal')",
    )
    .bind(session_id_to_uuid(session))
    .fetch_one(&mut **transaction)
    .await?;
    u64::try_from(count)
        .map_err(|_| ProcessReadCorruption::InvalidOrdinal("transcript model-call count").into())
}

async fn load_next_model_call_usage(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
    after: Option<(u64, ModelCallId)>,
) -> Result<Option<PgRow>, ProcessReadError> {
    sqlx::query(
        "WITH terminal_call AS (
            SELECT turn_id, session_id, model_call_id,
                   resolved_provider_model_identity_id, credential_reference,
                   usage_provenance_kind, usage_input_includes_cache_tokens,
                   usage_input_tokens, usage_output_tokens,
                   usage_cache_creation_input_tokens,
                   usage_cache_read_input_tokens
              FROM model_call
             WHERE session_id = $1 AND state_kind = 'terminal'
            UNION ALL
            SELECT turn_id, session_id, model_call_id,
                   resolved_provider_model_identity_id, credential_reference,
                   usage_provenance_kind, usage_input_includes_cache_tokens,
                   input_tokens, output_tokens,
                   cache_creation_input_tokens, cache_read_input_tokens
              FROM tool_approval_judge_model_call
             WHERE session_id = $1 AND state_kind = 'terminal'
         )
         SELECT
            turn.acceptance_position,
            call.turn_id,
            call.model_call_id,
            call.resolved_provider_model_identity_id,
            call.credential_reference,
            call.usage_provenance_kind,
            call.usage_input_includes_cache_tokens,
            call.usage_input_tokens,
            call.usage_output_tokens,
            call.usage_cache_creation_input_tokens,
            call.usage_cache_read_input_tokens
           FROM terminal_call AS call
           JOIN turn_lifecycle AS turn
             ON turn.turn_id = call.turn_id
            AND turn.session_id = call.session_id
          WHERE $2::numeric IS NULL
             OR turn.acceptance_position > $2
             OR (
                 turn.acceptance_position = $2
                 AND call.model_call_id > $3
             )
          ORDER BY turn.acceptance_position, call.model_call_id
          LIMIT 1",
    )
    .bind(session_id_to_uuid(session))
    .bind(after.map(|(position, _)| Decimal::from(position)))
    .bind(after.map(|(_, call)| call.into_uuid()))
    .fetch_optional(&mut **transaction)
    .await
    .map_err(Into::into)
}

fn decode_model_call_usage(
    row: &PgRow,
) -> Result<(u64, ProcessTranscriptModelCallUsage), ProcessReadError> {
    let acceptance_position = decode_positive(
        required(row, "acceptance_position")?,
        "model-call turn acceptance position",
    )?;
    let provenance_value = required::<String>(row, "usage_provenance_kind")?;
    let Some(provenance) = ProcessModelCallUsageProvenance::from_storage(provenance_value.as_str())
    else {
        return Err(ProcessReadCorruption::Unsupported {
            field: "usage_provenance_kind",
            value: provenance_value,
        }
        .into());
    };
    let input_tokens = row
        .try_get::<Option<Decimal>, _>("usage_input_tokens")?
        .map(|value| decode_nonnegative(value, "model-call input tokens"))
        .transpose()?;
    let output_tokens = row
        .try_get::<Option<Decimal>, _>("usage_output_tokens")?
        .map(|value| decode_nonnegative(value, "model-call output tokens"))
        .transpose()?;
    let cache_creation_input_tokens = row
        .try_get::<Option<Decimal>, _>("usage_cache_creation_input_tokens")?
        .map(|value| decode_nonnegative(value, "model-call cache-creation input tokens"))
        .transpose()?;
    let cache_read_input_tokens = row
        .try_get::<Option<Decimal>, _>("usage_cache_read_input_tokens")?
        .map(|value| decode_nonnegative(value, "model-call cache-read input tokens"))
        .transpose()?;
    Ok((
        acceptance_position,
        ProcessTranscriptModelCallUsage {
            turn: TurnId::from_uuid(required(row, "turn_id")?),
            call: ModelCallId::from_uuid(required(row, "model_call_id")?),
            target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(required(
                row,
                "resolved_provider_model_identity_id",
            )?)),
            credential_profile: required(row, "credential_reference")?,
            input_token_semantics: ProcessModelCallInputTokenSemantics::from_storage(
                row.try_get("usage_input_includes_cache_tokens")?,
            ),
            provenance,
            usage: ProcessModelCallTokenUsage {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
            },
        },
    ))
}

async fn load_next_transcript_turn(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
    after: Option<u64>,
) -> Result<Option<PgRow>, ProcessReadError> {
    sqlx::query(
        "SELECT
            turn.turn_id,
            turn.session_id AS turn_session_id,
            turn.acceptance_position,
            turn.origin_kind,
            turn.origin_accepted_input_id,
            turn.state_kind,
            turn.start_lineage_kind,
            turn.immediate_predecessor_turn_id,
            turn.starting_frontier_id,
            turn.terminal_frontier_id,
            turn.active_phase_kind,
            turn.child_wait_request_id,
            turn.current_attempt_id,
            turn.terminal_disposition_kind,
            turn.recovery_model_call_id,
            turn.active_tool_round_call_id,
            turn.approval_tool_request_id,
            turn.recovery_tool_attempt_id,
            turn.runner_recovery_runner_id,
            turn.runner_recovery_placement_revision,
            turn.runner_recovery_tool_attempt_id,
            turn.terminal_attempt_id,
            turn.terminal_model_call_id,
            turn.terminal_tool_attempt_id,
            terminal_call.terminal_disposition_kind
                AS terminal_model_call_disposition_kind,
            terminal_call.terminal_provider_failure_cause
                AS terminal_model_call_provider_failure_cause,
            terminal_call.terminal_attachment_preparation_failure_cause
                AS terminal_model_call_attachment_preparation_failure_cause,
            accepted.accepted_input_id,
            accepted.acceptance_position AS accepted_position,
            accepted.origin_turn_id,
            CASE WHEN accepted.accepted_input_id IS NULL THEN NULL
                 ELSE accepted_input_content_parts_json(
                    accepted.accepted_input_id)
            END AS accepted_content,
            task.spawning_tool_request_id AS delegated_spawning_tool_request_id,
            task.task_content AS delegated_task_content,
            relation.parent_session_id AS delegated_parent_session_id,
            relation.parent_turn_id AS delegated_parent_turn_id,
            wake.first_delivery_sequence AS delegated_wake_first_delivery_sequence,
            wake.through_delivery_sequence AS delegated_wake_through_delivery_sequence,
            child_wait.spawning_tool_request_id AS child_wait_spawning_request_id,
            child_wait.child_session_id AS child_wait_child_session_id,
            accepted.model_settings_override AS accepted_model_settings_override,
            settings.accepted_input_id AS settings_accepted_input_id,
            settings.turn_id AS settings_turn_id,
            settings.session_id AS settings_session_id,
            settings.defaults_version AS settings_defaults_version,
            settings.selected_direct_model_id AS settings_selected_direct_id,
            settings.per_call_model_settings AS settings_per_call_model_settings,
            settings.resolved_model_settings AS settings_resolved_model_settings,
            settings.adjusted_from_selection_id AS settings_adjusted_from_selection_id,
            settings.adjustments AS settings_adjustments,
            configuration_origin.defaults_version AS origin_defaults_version,
            configuration_origin.requested_model_kind AS origin_requested_model_kind,
            configuration_origin.requested_direct_model_selection_id
                AS origin_requested_direct_id,
            configuration_origin.requested_model_alias_id AS origin_requested_alias_id,
            configuration_origin.frozen_model_kind AS origin_frozen_model_kind,
            configuration_origin.frozen_direct_model_selection_id AS origin_frozen_direct_id,
            configuration_origin.frozen_model_alias_id AS origin_frozen_alias_id,
            configuration_origin.frozen_alias_selected_direct_id
                AS origin_frozen_alias_selected_direct_id,
            configuration_origin.model_settings_evidence_required
                AS origin_model_settings_evidence_required,
            origin_accepted.model_settings_override
                AS origin_model_settings_override,
            origin_defaults.model_settings AS origin_defaults_model_settings,
            current_call.model_call_id AS current_model_call_id,
            current_call.state_kind AS current_model_call_state_kind,
            current_call.context_frontier_id AS current_model_call_frontier_id,
            recovery_call.context_frontier_id AS recovery_model_call_frontier_id,
            automatic_reconciliation.state_kind
                AS automatic_reconciliation_state_kind,
            automatic_reconciliation.attempt_count
                AS automatic_reconciliation_attempt_count,
            automatic_reconciliation.model_call_id
                AS automatic_reconciliation_model_call_id,
            automatic_reconciliation.tool_attempt_id
                AS automatic_reconciliation_tool_attempt_id,
            active_tool_round.boundary_frontier_id AS active_tool_round_frontier_id,
            logical_terminal.spawning_tool_request_id
                AS logical_terminal_spawning_request_id,
            logical_terminal.terminal_frontier_id
                AS logical_terminal_frontier_id,
            logical_terminal_event.outcome_kind
                AS logical_terminal_outcome_kind,
            logical_terminal_event.reason_kind
                AS logical_terminal_reason_kind,
            logical_terminal_event.provenance_kind AS provenance_kind,
            logical_terminal_event.provenance_session_id AS provenance_session_id,
            logical_terminal_event.provenance_turn_id AS provenance_turn_id,
            logical_terminal_event.provenance_goal_generation
                AS provenance_goal_generation,
            logical_terminal_event.provenance_command_id AS provenance_command_id
           FROM turn_lifecycle AS turn
           LEFT JOIN accepted_input AS accepted
             ON accepted.accepted_input_id = turn.origin_accepted_input_id
            AND accepted.session_id = turn.session_id
           LEFT JOIN session_delegation_initial_task AS task
             ON task.turn_id = turn.turn_id
            AND task.child_session_id = turn.session_id
           LEFT JOIN session_delegation_wake_turn_origin AS wake
             ON wake.turn_id = turn.turn_id
            AND wake.recipient_session_id = turn.session_id
            AND wake.admission_position = turn.acceptance_position
           LEFT JOIN session_delegation AS relation
             ON relation.spawning_tool_request_id = task.spawning_tool_request_id
            AND relation.child_session_id = task.child_session_id
           LEFT JOIN session_delegation_wait AS child_wait
             ON child_wait.awaiting_tool_request_id = turn.child_wait_request_id
            AND child_wait.parent_turn_id = turn.turn_id
            AND child_wait.parent_session_id = turn.session_id
            AND child_wait.wait_mode = 'foreground'
           LEFT JOIN turn_model_settings_resolved AS settings
             ON settings.accepted_input_id = turn.origin_accepted_input_id
            AND settings.turn_id = turn.turn_id
            AND settings.session_id = turn.session_id
           LEFT JOIN LATERAL (
                WITH RECURSIVE configuration_chain AS (
                    SELECT queued.*
                      FROM queued_input_origin AS queued
                     WHERE queued.accepted_input_id = turn.origin_accepted_input_id
                       AND queued.turn_id = turn.turn_id
                       AND queued.session_id = turn.session_id
                    UNION
                    SELECT source.*
                      FROM configuration_chain AS current
                      JOIN queued_input_origin AS source
                        ON source.turn_id = current.source_configuration_turn_id
                       AND source.session_id = current.session_id
                )
                SELECT *
                  FROM configuration_chain
                 WHERE source_configuration_turn_id IS NULL
           ) AS configuration_origin ON TRUE
           LEFT JOIN accepted_input AS origin_accepted
             ON origin_accepted.accepted_input_id =
                configuration_origin.accepted_input_id
            AND origin_accepted.session_id = configuration_origin.session_id
            AND origin_accepted.origin_turn_id = configuration_origin.turn_id
           LEFT JOIN session_defaults_version AS origin_defaults
             ON origin_defaults.session_id = configuration_origin.session_id
            AND origin_defaults.version = configuration_origin.defaults_version
           LEFT JOIN model_call AS current_call
             ON current_call.turn_attempt_id = turn.current_attempt_id
            AND current_call.turn_id = turn.turn_id
            AND current_call.session_id = turn.session_id
            AND current_call.state_kind <> 'terminal'
           LEFT JOIN model_call AS recovery_call
             ON recovery_call.model_call_id = turn.recovery_model_call_id
            AND recovery_call.turn_attempt_id = turn.current_attempt_id
            AND recovery_call.turn_id = turn.turn_id
            AND recovery_call.session_id = turn.session_id
            AND recovery_call.state_kind = 'terminal'
           LEFT JOIN automatic_reconciliation AS automatic_reconciliation
             ON automatic_reconciliation.turn_id = turn.turn_id
            AND automatic_reconciliation.session_id = turn.session_id
           LEFT JOIN model_call AS terminal_call
             ON terminal_call.model_call_id = turn.terminal_model_call_id
            AND terminal_call.turn_attempt_id = turn.terminal_attempt_id
            AND terminal_call.turn_id = turn.turn_id
            AND terminal_call.session_id = turn.session_id
            AND terminal_call.state_kind = 'terminal'
           LEFT JOIN tool_round AS active_tool_round
             ON active_tool_round.producing_model_call_id =
                turn.active_tool_round_call_id
            AND active_tool_round.turn_id = turn.turn_id
            AND active_tool_round.session_id = turn.session_id
           LEFT JOIN session_delegation_logical_terminal AS logical_terminal
             ON logical_terminal.child_session_id = turn.session_id
            AND logical_terminal.child_turn_id = turn.turn_id
           LEFT JOIN session_delegation_event AS logical_terminal_event
             ON logical_terminal_event.spawning_tool_request_id =
                    logical_terminal.spawning_tool_request_id
            AND logical_terminal_event.event_kind = 'outcome_recorded'
            AND logical_terminal_event.provenance_command_id =
                    logical_terminal.root_command_id
            AND logical_terminal_event.outcome_kind = CASE
                    logical_terminal.disposition_kind
                    WHEN 'stopped' THEN 'child_stopped'
                    WHEN 'cancelled' THEN 'child_cancelled'
                END
          WHERE turn.session_id = $1
            AND (
                goal_turn_is_runtime_relevant(turn.session_id, turn.turn_id)
                OR logical_terminal.child_turn_id IS NOT NULL
            )
            AND ($2::numeric IS NULL OR turn.acceptance_position > $2)
          ORDER BY turn.acceptance_position
          LIMIT 1",
    )
    .bind(session_id_to_uuid(session))
    .bind(after.map(Decimal::from))
    .fetch_optional(&mut **transaction)
    .await
    .map_err(Into::into)
}

fn decode_provider_failure_cause(
    value: &str,
) -> Result<ProcessProviderModelCallFailureCause, ProcessReadError> {
    match value {
        "credential_rejected" => Ok(ProcessProviderModelCallFailureCause::CredentialRejected),
        "permission_denied" => Ok(ProcessProviderModelCallFailureCause::PermissionDenied),
        "invalid_request" => Ok(ProcessProviderModelCallFailureCause::InvalidRequest),
        "target_not_found" => Ok(ProcessProviderModelCallFailureCause::TargetNotFound),
        "request_too_large" => Ok(ProcessProviderModelCallFailureCause::RequestTooLarge),
        "rate_limited" => Ok(ProcessProviderModelCallFailureCause::RateLimited),
        "quota_exhausted" => Ok(ProcessProviderModelCallFailureCause::QuotaExhausted),
        "overloaded" => Ok(ProcessProviderModelCallFailureCause::Overloaded),
        "provider_internal" => Ok(ProcessProviderModelCallFailureCause::ProviderInternal),
        "unrecognized" => Ok(ProcessProviderModelCallFailureCause::Unrecognized),
        value => Err(ProcessReadCorruption::Unsupported {
            field: "model-call provider failure cause",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn decode_attachment_preparation_failure_cause(
    value: &str,
) -> Result<ProcessAttachmentPreparationFailureCause, ProcessReadError> {
    match value {
        "too_large" => Ok(ProcessAttachmentPreparationFailureCause::TooLarge),
        "missing" => Ok(ProcessAttachmentPreparationFailureCause::Missing),
        "corrupt" => Ok(ProcessAttachmentPreparationFailureCause::Corrupt),
        value => Err(ProcessReadCorruption::Unsupported {
            field: "model-call attachment-preparation failure cause",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn decode_database_count(
    row: &PgRow,
    column: &'static str,
    field: &'static str,
) -> Result<u64, ProcessReadError> {
    let count: i64 = row.try_get(column)?;
    u64::try_from(count).map_err(|_| ProcessReadCorruption::InvalidOrdinal(field).into())
}

#[allow(clippy::too_many_arguments)]
fn decode_transcript_turn_origin(
    origin_kind: String,
    origin_accepted_input: Option<Uuid>,
    accepted_input: Option<Uuid>,
    accepted_position: Option<Decimal>,
    accepted_origin: Option<Uuid>,
    accepted_content: Option<Value>,
    delegated_spawning_request: Option<Uuid>,
    delegated_parent_session: Option<Uuid>,
    delegated_parent_turn: Option<Uuid>,
    delegated_task_content: Option<String>,
    delegated_wake_first: Option<Decimal>,
    delegated_wake_through: Option<Decimal>,
    turn: TurnId,
    acceptance_position: u64,
) -> Result<DecodedTurnOrigin, ProcessReadError> {
    match (
        origin_kind.as_str(),
        origin_accepted_input,
        accepted_input,
        accepted_position,
        accepted_origin,
        accepted_content,
        delegated_spawning_request,
        delegated_parent_session,
        delegated_parent_turn,
        delegated_task_content,
        delegated_wake_first,
        delegated_wake_through,
    ) {
        (
            "accepted_input",
            Some(origin_accepted_input),
            Some(accepted_input),
            Some(accepted_position),
            Some(accepted_origin),
            Some(content),
            None,
            None,
            None,
            None,
            None,
            None,
        ) => {
            let accepted_position = decode_positive(accepted_position, "accepted input position")?;
            let content = crate::user_content::decode(content)
                .map_err(|_| ProcessReadCorruption::Inconsistent("turn accepted-input content"))?;
            if origin_accepted_input != accepted_input
                || accepted_position != acceptance_position
                || accepted_origin != turn.into_uuid()
            {
                return Err(
                    ProcessReadCorruption::Inconsistent("turn accepted-input correlation").into(),
                );
            }
            Ok(DecodedTurnOrigin::AcceptedInput {
                accepted_input: AcceptedInputId::from_uuid(accepted_input),
                content,
            })
        }
        (
            "delegation",
            None,
            None,
            None,
            None,
            None,
            Some(spawning_request),
            Some(parent_session),
            Some(parent_turn),
            Some(content),
            None,
            None,
        ) if !content.is_empty() => Ok(DecodedTurnOrigin::DelegatedTask {
            spawning_request: ToolRequestId::from_uuid(spawning_request),
            parent_session: SessionId::from_uuid(parent_session),
            parent_turn: TurnId::from_uuid(parent_turn),
            content,
        }),
        (
            "delegation",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(first),
            Some(through),
        ) => {
            let first = decode_positive(first, "delegation wake first delivery sequence")?;
            let through = decode_positive(through, "delegation wake through delivery sequence")?;
            if first > through {
                return Err(
                    ProcessReadCorruption::Inconsistent("delegation wake delivery range").into(),
                );
            }
            Ok(DecodedTurnOrigin::DelegationWake {
                first_delivery_sequence: first,
                through_delivery_sequence: through,
            })
        }
        ("accepted_input" | "delegation", ..) => {
            Err(ProcessReadCorruption::Inconsistent("turn origin correlation").into())
        }
        (value, ..) => Err(ProcessReadCorruption::Unsupported {
            field: "turn origin kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn decode_transcript_model_selection(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
) -> Result<ModelSelectionRequest, ProcessReadError> {
    match (kind.as_str(), direct, alias) {
        ("direct", Some(selection), None) => Ok(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(selection),
        )),
        ("alias", None, Some(alias)) => {
            Ok(ModelSelectionRequest::Alias(ModelAlias::from_uuid(alias)))
        }
        ("direct" | "alias", _, _) => {
            Err(ProcessReadCorruption::Inconsistent("turn requested model shape").into())
        }
        _ => Err(ProcessReadCorruption::Unsupported {
            field: "turn requested model kind",
            value: kind,
        }
        .into()),
    }
}

fn decode_transcript_frozen_model(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    alias_selected: Option<Uuid>,
) -> Result<FrozenModelSelection, ProcessReadError> {
    match (kind.as_str(), direct, alias, alias_selected) {
        ("direct", Some(selection), None, None) => Ok(FrozenModelSelection::Direct(
            DirectModelSelection::from_uuid(selection),
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
            Err(ProcessReadCorruption::Inconsistent("turn frozen model shape").into())
        }
        _ => Err(ProcessReadCorruption::Unsupported {
            field: "turn frozen model kind",
            value: kind,
        }
        .into()),
    }
}

fn requested_from_transcript_frozen(selection: &FrozenModelSelection) -> ModelSelectionRequest {
    match selection {
        FrozenModelSelection::Direct(selection) => ModelSelectionRequest::Direct(*selection),
        FrozenModelSelection::FrozenAlias { alias, .. } => ModelSelectionRequest::Alias(*alias),
    }
}

fn decode_transcript_turn_model_settings(
    row: &PgRow,
    turn: TurnId,
    accepted_input: AcceptedInputId,
) -> Result<Option<TurnModelSettingsResolved>, ProcessReadError> {
    let stored_accepted: Option<Uuid> = row.try_get("settings_accepted_input_id")?;
    let stored_turn: Option<Uuid> = row.try_get("settings_turn_id")?;
    let stored_session: Option<Uuid> = row.try_get("settings_session_id")?;
    let stored_defaults: Option<Decimal> = row.try_get("settings_defaults_version")?;
    let stored_selected: Option<Uuid> = row.try_get("settings_selected_direct_id")?;
    let stored_per_call: Option<Value> = row.try_get("settings_per_call_model_settings")?;
    let stored_settings: Option<Value> = row.try_get("settings_resolved_model_settings")?;
    let stored_adjustments: Option<Value> = row.try_get("settings_adjustments")?;
    let absent = stored_accepted.is_none()
        && stored_turn.is_none()
        && stored_session.is_none()
        && stored_defaults.is_none()
        && stored_selected.is_none()
        && stored_per_call.is_none()
        && stored_settings.is_none()
        && stored_adjustments.is_none();
    if absent {
        let evidence_required: bool = required(row, "origin_model_settings_evidence_required")?;
        return if evidence_required {
            Err(ProcessReadCorruption::Missing("turn model settings evidence").into())
        } else {
            Ok(None)
        };
    }
    let (Some(stored_accepted), Some(stored_turn), Some(stored_session), Some(stored_defaults)) = (
        stored_accepted,
        stored_turn,
        stored_session,
        stored_defaults,
    ) else {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings shape").into());
    };
    let Some(stored_selected) = stored_selected else {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings shape").into());
    };
    let Some(stored_per_call) = stored_per_call else {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings shape").into());
    };
    let Some(stored_settings) = stored_settings else {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings shape").into());
    };
    let Some(stored_adjustments) = stored_adjustments else {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings shape").into());
    };
    let turn_session: Uuid = required(row, "turn_session_id")?;
    if AcceptedInputId::from_uuid(stored_accepted) != accepted_input
        || TurnId::from_uuid(stored_turn) != turn
        || stored_session != turn_session
    {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings identity").into());
    }
    let defaults_version = defaults_version_from_numeric(stored_defaults)
        .map_err(|_| ProcessReadCorruption::Inconsistent("turn model settings version"))?;
    let origin_defaults = defaults_version_from_numeric(required(row, "origin_defaults_version")?)
        .map_err(|_| ProcessReadCorruption::Inconsistent("turn origin defaults version"))?;
    let requested = decode_transcript_model_selection(
        required(row, "origin_requested_model_kind")?,
        row.try_get("origin_requested_direct_id")?,
        row.try_get("origin_requested_alias_id")?,
    )?;
    let frozen = decode_transcript_frozen_model(
        required(row, "origin_frozen_model_kind")?,
        row.try_get("origin_frozen_direct_id")?,
        row.try_get("origin_frozen_alias_id")?,
        row.try_get("origin_frozen_alias_selected_direct_id")?,
    )?;
    let per_call = model_settings_overlay_from_json(stored_per_call)
        .map_err(|_| ProcessReadCorruption::Inconsistent("turn per-call model settings"))?;
    let origin_per_call =
        model_settings_overlay_from_json(required(row, "origin_model_settings_override")?)
            .map_err(|_| ProcessReadCorruption::Inconsistent("turn accepted model settings"))?;
    if defaults_version != origin_defaults
        || requested != requested_from_transcript_frozen(&frozen)
        || frozen.selected_direct().into_uuid() != stored_selected
        || per_call != origin_per_call
    {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings origin").into());
    }
    let event = TurnModelSettingsResolved::try_new(
        accepted_input,
        turn,
        defaults_version,
        frozen,
        per_call,
        model_settings_from_json(stored_settings)
            .map_err(|_| ProcessReadCorruption::Inconsistent("turn resolved model settings"))?,
        row.try_get::<Option<Uuid>, _>("settings_adjusted_from_selection_id")?
            .map(DirectModelSelection::from_uuid),
        model_change_adjustments_from_json(stored_adjustments)
            .map_err(|_| ProcessReadCorruption::Inconsistent("turn model setting adjustments"))?,
    )
    .ok_or(ProcessReadCorruption::Inconsistent(
        "turn model settings evidence",
    ))?;
    let origin_defaults =
        model_settings_from_json(required(row, "origin_defaults_model_settings")?)
            .map_err(|_| ProcessReadCorruption::Inconsistent("turn defaults model settings"))?;
    if !crate::model_settings_resolution::matches_defaults(&event, origin_defaults) {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings defaults").into());
    }
    Ok(Some(event))
}

fn admitted_automatic_reconciliation_attempts(
    attempts: i32,
    exhausted: bool,
    budget: Option<Option<u32>>,
) -> Result<u32, ProcessReadError> {
    let attempts = u32::try_from(attempts).map_err(|_| {
        ProcessReadCorruption::Inconsistent("automatic reconciliation attempt count")
    })?;
    let admitted = if exhausted {
        match budget {
            Some(Some(budget)) => attempts == budget,
            Some(None) => false,
            None => attempts > 0,
        }
    } else {
        budget.is_none_or(|budget| budget.is_none_or(|budget| attempts <= budget))
    };
    admitted
        .then_some(attempts)
        .ok_or(ProcessReadCorruption::Inconsistent(
            "automatic reconciliation attempt budget",
        ))
        .map_err(Into::into)
}

fn decode_transcript_turn(
    row: &PgRow,
    automatic_reconciliation_attempt_budget: Option<Option<u32>>,
) -> Result<DecodedTurn, ProcessReadError> {
    let turn = TurnId::from_uuid(required(row, "turn_id")?);
    let acceptance_position = decode_positive(
        required(row, "acceptance_position")?,
        "turn acceptance position",
    )?;
    let origin_kind: String = required(row, "origin_kind")?;
    let origin = decode_transcript_turn_origin(
        origin_kind,
        row.try_get("origin_accepted_input_id")?,
        row.try_get("accepted_input_id")?,
        row.try_get("accepted_position")?,
        row.try_get("origin_turn_id")?,
        row.try_get("accepted_content")?,
        row.try_get("delegated_spawning_tool_request_id")?,
        row.try_get("delegated_parent_session_id")?,
        row.try_get("delegated_parent_turn_id")?,
        row.try_get("delegated_task_content")?,
        row.try_get("delegated_wake_first_delivery_sequence")?,
        row.try_get("delegated_wake_through_delivery_sequence")?,
        turn,
        acceptance_position,
    )?;
    let logical_terminal = decode_logical_delegation_terminal(row)?;
    // The accepted-input correlation checks now live in
    // `decode_transcript_turn_origin`, which also admits delegation origins.
    // Model-settings evidence is keyed by the originating accepted input, and
    // the schema forbids one on a delegation-origin turn, so those decode to no
    // resolved settings instead of demanding structurally absent evidence.
    let model_settings = match &origin {
        DecodedTurnOrigin::AcceptedInput { accepted_input, .. } => {
            decode_transcript_turn_model_settings(row, turn, *accepted_input)?
        }
        DecodedTurnOrigin::DelegatedTask { .. } | DecodedTurnOrigin::DelegationWake { .. } => None,
    };
    let state_kind: String = required(row, "state_kind")?;
    let start_lineage_kind: Option<String> = row.try_get("start_lineage_kind")?;
    let immediate_predecessor: Option<Uuid> = row.try_get("immediate_predecessor_turn_id")?;
    let start_lineage = match (
        state_kind.as_str(),
        start_lineage_kind.as_deref(),
        immediate_predecessor,
    ) {
        ("queued", None, None) => None,
        ("active" | "terminal", Some("first_in_session"), None) => {
            Some(DecodedStartLineage::FirstInSession)
        }
        ("active" | "terminal", Some("after"), Some(predecessor)) => {
            Some(DecodedStartLineage::After(TurnId::from_uuid(predecessor)))
        }
        ("queued" | "active" | "terminal", Some(value), _)
            if !matches!(value, "first_in_session" | "after") =>
        {
            return Err(ProcessReadCorruption::Unsupported {
                field: "turn start lineage kind",
                value: value.to_owned(),
            }
            .into());
        }
        _ => {
            return Err(ProcessReadCorruption::Inconsistent("turn start lineage shape").into());
        }
    };
    let starting_frontier: Option<Uuid> = row.try_get("starting_frontier_id")?;
    let terminal_frontier: Option<Uuid> = row.try_get("terminal_frontier_id")?;
    let active_phase: Option<String> = row.try_get("active_phase_kind")?;
    let current_attempt: Option<Uuid> = row.try_get("current_attempt_id")?;
    let child_wait_request: Option<Uuid> = row.try_get("child_wait_request_id")?;
    let child_wait_spawning_request: Option<Uuid> =
        row.try_get("child_wait_spawning_request_id")?;
    let child_wait_child: Option<Uuid> = row.try_get("child_wait_child_session_id")?;
    let terminal_disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
    let recovery_call: Option<Uuid> = row.try_get("recovery_model_call_id")?;
    let active_tool_round_call: Option<Uuid> = row.try_get("active_tool_round_call_id")?;
    let approval_tool_request: Option<Uuid> = row.try_get("approval_tool_request_id")?;
    let recovery_tool_attempt: Option<Uuid> = row.try_get("recovery_tool_attempt_id")?;
    let runner_recovery_runner: Option<Uuid> = row.try_get("runner_recovery_runner_id")?;
    let runner_recovery_revision: Option<Decimal> =
        row.try_get("runner_recovery_placement_revision")?;
    let runner_recovery_tool_attempt: Option<Uuid> =
        row.try_get("runner_recovery_tool_attempt_id")?;
    let terminal_attempt: Option<Uuid> = row.try_get("terminal_attempt_id")?;
    let terminal_call: Option<Uuid> = row.try_get("terminal_model_call_id")?;
    let terminal_tool_attempt: Option<Uuid> = row.try_get("terminal_tool_attempt_id")?;
    let terminal_call_disposition: Option<String> =
        row.try_get("terminal_model_call_disposition_kind")?;
    let terminal_call_provider_failure_cause: Option<String> =
        row.try_get("terminal_model_call_provider_failure_cause")?;
    let terminal_call_attachment_preparation_failure_cause: Option<String> =
        row.try_get("terminal_model_call_attachment_preparation_failure_cause")?;
    if active_phase.as_deref() != Some("awaiting_runner_recovery")
        && (runner_recovery_runner.is_some()
            || runner_recovery_revision.is_some()
            || runner_recovery_tool_attempt.is_some())
    {
        return Err(
            ProcessReadCorruption::Inconsistent("runner recovery lifecycle payload").into(),
        );
    }
    if terminal_call_provider_failure_cause.is_some()
        && terminal_call_disposition.as_deref() != Some("known_failed")
    {
        return Err(ProcessReadCorruption::Inconsistent(
            "provider failure cause without known-failed model call",
        )
        .into());
    }
    if terminal_call_attachment_preparation_failure_cause.is_some()
        && (terminal_call_disposition.as_deref() != Some("known_failed")
            || terminal_call_provider_failure_cause.is_some())
    {
        return Err(ProcessReadCorruption::Inconsistent(
            "attachment-preparation failure cause without local known-failed model call",
        )
        .into());
    }
    let current_model_call: Option<Uuid> = row.try_get("current_model_call_id")?;
    let current_model_call_state: Option<String> = row.try_get("current_model_call_state_kind")?;
    let current_model_call_frontier: Option<Uuid> =
        row.try_get("current_model_call_frontier_id")?;
    let recovery_model_call_frontier: Option<Uuid> =
        row.try_get("recovery_model_call_frontier_id")?;
    let automatic_reconciliation_state: Option<String> =
        row.try_get("automatic_reconciliation_state_kind")?;
    let automatic_reconciliation_attempts: Option<i32> =
        row.try_get("automatic_reconciliation_attempt_count")?;
    let automatic_reconciliation_model_call: Option<Uuid> =
        row.try_get("automatic_reconciliation_model_call_id")?;
    let automatic_reconciliation_tool_attempt: Option<Uuid> =
        row.try_get("automatic_reconciliation_tool_attempt_id")?;
    let active_tool_round_frontier: Option<Uuid> = row.try_get("active_tool_round_frontier_id")?;

    if !matches!(state_kind.as_str(), "queued" | "active" | "terminal") {
        return Err(ProcessReadCorruption::Unsupported {
            field: "turn state kind",
            value: state_kind,
        }
        .into());
    }
    if let Some(value) = active_phase.as_deref()
        && !matches!(
            value,
            "running"
                | "awaiting_model_call_recovery"
                | "awaiting_tool_approval"
                | "awaiting_child"
                | "awaiting_tool_recovery"
                | "awaiting_runner_recovery"
        )
    {
        return Err(ProcessReadCorruption::Unsupported {
            field: "turn active phase",
            value: value.to_owned(),
        }
        .into());
    }
    let automatic_reconciliation_present = automatic_reconciliation_state.is_some()
        || automatic_reconciliation_attempts.is_some()
        || automatic_reconciliation_model_call.is_some()
        || automatic_reconciliation_tool_attempt.is_some();
    let terminal_reconciliation = state_kind == "terminal"
        && terminal_disposition.as_deref() == Some("reconciliation_required");
    if automatic_reconciliation_present {
        if !matches!(
            active_phase.as_deref(),
            Some("awaiting_model_call_recovery" | "awaiting_tool_recovery")
        ) && !terminal_reconciliation
        {
            return Err(ProcessReadCorruption::Inconsistent(
                "automatic model-call reconciliation outside recovery wait",
            )
            .into());
        }
        if terminal_reconciliation {
            let valid_terminal_automatic = matches!(
                (
                    automatic_reconciliation_state.as_deref(),
                    automatic_reconciliation_attempts,
                    automatic_reconciliation_model_call,
                    automatic_reconciliation_tool_attempt,
                ),
                (
                    Some("reconciled" | "superseded"),
                    Some(attempts),
                    model_call,
                    tool_attempt,
                ) if admitted_automatic_reconciliation_attempts(
                        attempts,
                        false,
                        automatic_reconciliation_attempt_budget,
                    ).is_ok()
                    && model_call == terminal_call
                    && tool_attempt == terminal_tool_attempt
                    && model_call.is_some() != tool_attempt.is_some()
            );
            if !valid_terminal_automatic {
                return Err(ProcessReadCorruption::Inconsistent(
                    "terminal automatic model-call reconciliation state",
                )
                .into());
            }
        }
    }
    if let Some(value) = terminal_disposition.as_deref()
        && !matches!(
            value,
            "failed" | "completed" | "refused" | "cancelled" | "reconciliation_required"
        )
    {
        return Err(ProcessReadCorruption::Unsupported {
            field: "turn terminal disposition",
            value: value.to_owned(),
        }
        .into());
    }
    let (current_model_call, current_model_call_frontier) = match (
        current_model_call,
        current_model_call_state.as_deref(),
        current_model_call_frontier,
    ) {
        (None, None, None) => (None, None),
        (Some(call), Some("prepared"), Some(frontier)) => (
            Some(ProcessCurrentModelCall {
                call: ModelCallId::from_uuid(call),
                state: ProcessCurrentModelCallState::Prepared,
            }),
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (Some(call), Some("in_flight"), Some(frontier)) => (
            Some(ProcessCurrentModelCall {
                call: ModelCallId::from_uuid(call),
                state: ProcessCurrentModelCallState::InFlight,
            }),
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (Some(call), Some("cancellation_requested"), Some(frontier)) => (
            Some(ProcessCurrentModelCall {
                call: ModelCallId::from_uuid(call),
                state: ProcessCurrentModelCallState::CancellationRequested,
            }),
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (Some(_), Some(value), _)
            if !matches!(value, "prepared" | "in_flight" | "cancellation_requested") =>
        {
            return Err(ProcessReadCorruption::Unsupported {
                field: "current model call state",
                value: value.to_owned(),
            }
            .into());
        }
        _ => {
            return Err(ProcessReadCorruption::Inconsistent("current model call shape").into());
        }
    };
    let recovery_model_call_frontier =
        recovery_model_call_frontier.map(ContextFrontierId::from_uuid);

    if matches!(active_phase.as_deref(), Some("awaiting_runner_recovery")) {
        let (Some(starting_frontier), Some(runner), Some(revision)) = (
            starting_frontier,
            runner_recovery_runner,
            runner_recovery_revision,
        ) else {
            return Err(ProcessReadCorruption::Inconsistent("runner recovery wait shape").into());
        };
        if state_kind != "active"
            || terminal_frontier.is_some()
            || current_attempt.is_some()
            || terminal_disposition.is_some()
            || approval_tool_request.is_some()
            || recovery_call.is_some()
            || recovery_tool_attempt.is_some()
            || child_wait_request.is_some()
            || terminal_attempt.is_some()
            || terminal_call.is_some()
            || terminal_tool_attempt.is_some()
            || current_model_call.is_some()
            || current_model_call_frontier.is_some()
            || recovery_model_call_frontier.is_some()
            || active_tool_round_call.is_some() != active_tool_round_frontier.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("runner recovery wait shape").into());
        }
        let latest_frontier = active_tool_round_frontier.unwrap_or(starting_frontier);
        return project_logical_delegation_terminal(
            DecodedTurn {
                turn: ProcessTranscriptTurn {
                    turn,
                    acceptance_position,
                    model_settings,
                    state: ProcessTurnState::ActiveAwaitingRunnerRecovery {
                        runner: RunnerId::from_uuid(runner),
                        placement_revision: decode_runner_generation(
                            revision,
                            "runner recovery placement revision",
                        )?,
                        interrupted_tool_attempt: runner_recovery_tool_attempt
                            .map(ToolAttemptId::from_uuid),
                    },
                },
                start_lineage,
                latest_frontier: Some(ContextFrontierId::from_uuid(latest_frontier)),
            },
            logical_terminal,
        );
    }

    if matches!(active_phase.as_deref(), Some("awaiting_child")) {
        let (
            Some(starting_frontier),
            Some(awaiting_request),
            Some(spawning_request),
            Some(child),
            Some(_producing_call),
            Some(tool_frontier),
        ) = (
            starting_frontier,
            child_wait_request,
            child_wait_spawning_request,
            child_wait_child,
            active_tool_round_call,
            active_tool_round_frontier,
        )
        else {
            return Err(ProcessReadCorruption::Inconsistent("child wait shape").into());
        };
        if state_kind != "active"
            || terminal_frontier.is_some()
            || current_attempt.is_some()
            || terminal_disposition.is_some()
            || approval_tool_request.is_some()
            || recovery_call.is_some()
            || recovery_tool_attempt.is_some()
            || terminal_attempt.is_some()
            || terminal_call.is_some()
            || terminal_tool_attempt.is_some()
            || current_model_call.is_some()
            || current_model_call_frontier.is_some()
            || recovery_model_call_frontier.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("child wait shape").into());
        }
        let latest_frontier = ContextFrontierId::from_uuid(tool_frontier);
        if latest_frontier == ContextFrontierId::from_uuid(starting_frontier) {
            return Err(ProcessReadCorruption::Inconsistent("child wait frontier").into());
        }
        return project_logical_delegation_terminal(
            DecodedTurn {
                turn: ProcessTranscriptTurn {
                    turn,
                    acceptance_position,
                    model_settings,
                    state: ProcessTurnState::ActiveAwaitingChild {
                        awaiting_request: ToolRequestId::from_uuid(awaiting_request),
                        spawning_request: ToolRequestId::from_uuid(spawning_request),
                        child: SessionId::from_uuid(child),
                    },
                },
                start_lineage,
                latest_frontier: Some(latest_frontier),
            },
            logical_terminal,
        );
    }

    if matches!(active_phase.as_deref(), Some("awaiting_tool_approval")) {
        let (Some(starting_frontier), Some(_producing_call), Some(request), Some(tool_frontier)) = (
            starting_frontier,
            active_tool_round_call,
            approval_tool_request,
            active_tool_round_frontier,
        ) else {
            return Err(ProcessReadCorruption::Inconsistent("tool approval wait shape").into());
        };
        if state_kind != "active"
            || terminal_frontier.is_some()
            || current_attempt.is_some()
            || terminal_disposition.is_some()
            || recovery_call.is_some()
            || recovery_tool_attempt.is_some()
            || terminal_attempt.is_some()
            || terminal_call.is_some()
            || terminal_tool_attempt.is_some()
            || current_model_call.is_some()
            || current_model_call_frontier.is_some()
            || recovery_model_call_frontier.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("tool approval wait shape").into());
        }
        let latest_frontier = ContextFrontierId::from_uuid(tool_frontier);
        if latest_frontier == ContextFrontierId::from_uuid(starting_frontier) {
            return Err(ProcessReadCorruption::Inconsistent("tool approval frontier").into());
        }
        return project_logical_delegation_terminal(
            DecodedTurn {
                turn: ProcessTranscriptTurn {
                    turn,
                    acceptance_position,
                    model_settings,
                    state: ProcessTurnState::ActiveAwaitingToolApproval {
                        request: ToolRequestId::from_uuid(request),
                    },
                },
                start_lineage,
                latest_frontier: Some(latest_frontier),
            },
            logical_terminal,
        );
    }

    if matches!(active_phase.as_deref(), Some("awaiting_tool_recovery")) {
        let (
            Some(starting_frontier),
            Some(ended_attempt),
            Some(_producing_call),
            Some(recovery_attempt),
            Some(tool_frontier),
        ) = (
            starting_frontier,
            current_attempt,
            active_tool_round_call,
            recovery_tool_attempt,
            active_tool_round_frontier,
        )
        else {
            return Err(ProcessReadCorruption::Inconsistent("tool recovery wait shape").into());
        };
        if state_kind != "active"
            || terminal_frontier.is_some()
            || terminal_disposition.is_some()
            || approval_tool_request.is_some()
            || recovery_call.is_some()
            || terminal_attempt.is_some()
            || terminal_call.is_some()
            || terminal_tool_attempt.is_some()
            || current_model_call.is_some()
            || current_model_call_frontier.is_some()
            || recovery_model_call_frontier.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("tool recovery wait shape").into());
        }
        let latest_frontier = ContextFrontierId::from_uuid(tool_frontier);
        if latest_frontier == ContextFrontierId::from_uuid(starting_frontier) {
            return Err(ProcessReadCorruption::Inconsistent("tool recovery frontier").into());
        }
        if automatic_reconciliation_model_call.is_some()
            || automatic_reconciliation_tool_attempt
                .is_some_and(|stored| stored != recovery_attempt)
        {
            return Err(ProcessReadCorruption::Inconsistent(
                "automatic tool reconciliation attempt identity",
            )
            .into());
        }
        let (automatic_reconciliation_attempts, operator_action_required) = match (
            automatic_reconciliation_state.as_deref(),
            automatic_reconciliation_attempts,
            automatic_reconciliation_tool_attempt,
        ) {
            (None, None, None) => (0, false),
            (Some("scheduled" | "attempting"), Some(attempts), Some(_)) => (
                admitted_automatic_reconciliation_attempts(
                    attempts,
                    false,
                    automatic_reconciliation_attempt_budget,
                )?,
                false,
            ),
            (Some("exhausted"), Some(attempts), Some(_)) => (
                admitted_automatic_reconciliation_attempts(
                    attempts,
                    true,
                    automatic_reconciliation_attempt_budget,
                )?,
                true,
            ),
            _ => {
                return Err(ProcessReadCorruption::Inconsistent(
                    "active automatic tool reconciliation state",
                )
                .into());
            }
        };
        return project_logical_delegation_terminal(
            DecodedTurn {
                turn: ProcessTranscriptTurn {
                    turn,
                    acceptance_position,
                    model_settings,
                    state: ProcessTurnState::ActiveAwaitingToolRecovery {
                        ended_attempt: TurnAttemptId::from_uuid(ended_attempt),
                        recovery_attempt: ToolAttemptId::from_uuid(recovery_attempt),
                        automatic_reconciliation_attempts,
                        operator_action_required,
                    },
                },
                start_lineage,
                latest_frontier: Some(latest_frontier),
            },
            logical_terminal,
        );
    }

    if matches!(active_phase.as_deref(), Some("running")) && active_tool_round_call.is_some() {
        let (Some(starting_frontier), Some(attempt), Some(tool_frontier)) = (
            starting_frontier,
            current_attempt,
            active_tool_round_frontier,
        ) else {
            return Err(ProcessReadCorruption::Inconsistent("running tool round shape").into());
        };
        if state_kind != "active"
            || terminal_frontier.is_some()
            || terminal_disposition.is_some()
            || approval_tool_request.is_some()
            || recovery_call.is_some()
            || recovery_tool_attempt.is_some()
            || terminal_attempt.is_some()
            || terminal_call.is_some()
            || terminal_tool_attempt.is_some()
            || current_model_call.is_some()
            || current_model_call_frontier.is_some()
            || recovery_model_call_frontier.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("running tool round shape").into());
        }
        let latest_frontier = ContextFrontierId::from_uuid(tool_frontier);
        if latest_frontier == ContextFrontierId::from_uuid(starting_frontier) {
            return Err(ProcessReadCorruption::Inconsistent("running tool frontier").into());
        }
        return project_logical_delegation_terminal(
            DecodedTurn {
                turn: ProcessTranscriptTurn {
                    turn,
                    acceptance_position,
                    model_settings,
                    state: ProcessTurnState::ActiveRunning {
                        current_attempt: TurnAttemptId::from_uuid(attempt),
                        current_model_call: None,
                    },
                },
                start_lineage,
                latest_frontier: Some(latest_frontier),
            },
            logical_terminal,
        );
    }

    if state_kind == "terminal"
        && terminal_disposition.as_deref() == Some("reconciliation_required")
        && terminal_call.is_none()
        && terminal_tool_attempt.is_some()
    {
        let (Some(frontier), Some(attempt), Some(tool_attempt)) =
            (terminal_frontier, terminal_attempt, terminal_tool_attempt)
        else {
            return Err(ProcessReadCorruption::Inconsistent("tool reconciliation shape").into());
        };
        if active_phase.is_some()
            || current_attempt.is_some()
            || recovery_call.is_some()
            || active_tool_round_call.is_some()
            || approval_tool_request.is_some()
            || recovery_tool_attempt.is_some()
            || current_model_call.is_some()
            || current_model_call_frontier.is_some()
            || recovery_model_call_frontier.is_some()
            || active_tool_round_frontier.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("tool reconciliation shape").into());
        }
        return project_logical_delegation_terminal(
            DecodedTurn {
                turn: ProcessTranscriptTurn {
                    turn,
                    acceptance_position,
                    model_settings,
                    state: ProcessTurnState::ReconciliationRequired {
                        terminal_frontier: ContextFrontierId::from_uuid(frontier),
                        terminal_attempt: TurnAttemptId::from_uuid(attempt),
                        operation: ProcessReconciliationOperation::ToolAttempt(
                            ToolAttemptId::from_uuid(tool_attempt),
                        ),
                    },
                },
                start_lineage,
                latest_frontier: Some(ContextFrontierId::from_uuid(frontier)),
            },
            logical_terminal,
        );
    }

    if active_tool_round_call.is_some()
        || approval_tool_request.is_some()
        || recovery_tool_attempt.is_some()
        || terminal_tool_attempt.is_some()
        || active_tool_round_frontier.is_some()
    {
        return Err(ProcessReadCorruption::Inconsistent("tool lifecycle authority shape").into());
    }

    let (state, latest_frontier) = match (
        state_kind.as_str(),
        starting_frontier,
        terminal_frontier,
        active_phase.as_deref(),
        current_attempt,
        terminal_disposition.as_deref(),
        recovery_call,
        terminal_attempt,
        terminal_call,
        terminal_call_disposition.as_deref(),
        current_model_call,
    ) {
        ("queued", None, None, None, None, None, None, None, None, None, None) => {
            let state = match origin {
                DecodedTurnOrigin::AcceptedInput {
                    accepted_input,
                    content,
                } => ProcessTurnState::Queued {
                    accepted_input,
                    content,
                },
                DecodedTurnOrigin::DelegatedTask {
                    spawning_request,
                    parent_session,
                    parent_turn,
                    content,
                } => ProcessTurnState::QueuedDelegated {
                    spawning_request,
                    parent_session,
                    parent_turn,
                    content,
                },
                DecodedTurnOrigin::DelegationWake {
                    first_delivery_sequence,
                    through_delivery_sequence,
                } => ProcessTurnState::QueuedDelegationWake {
                    first_delivery_sequence,
                    through_delivery_sequence,
                },
            };
            (state, None)
        }
        (
            "active",
            Some(frontier),
            None,
            Some("running"),
            Some(attempt),
            None,
            None,
            None,
            None,
            None,
            current_model_call,
        ) => (
            ProcessTurnState::ActiveRunning {
                current_attempt: TurnAttemptId::from_uuid(attempt),
                current_model_call,
            },
            Some(
                current_model_call_frontier
                    .unwrap_or_else(|| ContextFrontierId::from_uuid(frontier)),
            ),
        ),
        (
            "active",
            Some(_),
            None,
            Some("awaiting_model_call_recovery"),
            Some(attempt),
            None,
            Some(call),
            None,
            None,
            None,
            None,
        ) => {
            if automatic_reconciliation_tool_attempt.is_some()
                || automatic_reconciliation_model_call.is_some_and(|stored| stored != call)
            {
                return Err(ProcessReadCorruption::Inconsistent(
                    "automatic model-call reconciliation call identity",
                )
                .into());
            }
            let call_frontier = recovery_model_call_frontier.ok_or(
                ProcessReadCorruption::Inconsistent("recovery model call frontier"),
            )?;
            let (automatic_reconciliation_attempts, operator_action_required) = match (
                automatic_reconciliation_state.as_deref(),
                automatic_reconciliation_attempts,
                automatic_reconciliation_model_call,
            ) {
                (None, None, None) => (0, false),
                (Some("scheduled" | "attempting"), Some(attempts), Some(_)) => (
                    admitted_automatic_reconciliation_attempts(
                        attempts,
                        false,
                        automatic_reconciliation_attempt_budget,
                    )?,
                    false,
                ),
                (Some("exhausted"), Some(attempts), Some(_)) => (
                    admitted_automatic_reconciliation_attempts(
                        attempts,
                        true,
                        automatic_reconciliation_attempt_budget,
                    )?,
                    true,
                ),
                _ => {
                    return Err(ProcessReadCorruption::Inconsistent(
                        "active automatic model-call reconciliation state",
                    )
                    .into());
                }
            };
            (
                ProcessTurnState::ActiveAwaitingModelCallRecovery {
                    ended_attempt: TurnAttemptId::from_uuid(attempt),
                    recovery_call: ModelCallId::from_uuid(call),
                    automatic_reconciliation_attempts,
                    operator_action_required,
                },
                Some(call_frontier),
            )
        }
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("failed"),
            None,
            None,
            None,
            None,
            None,
        ) => (
            ProcessTurnState::Failed {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: None,
                terminal_model_call: None,
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("failed"),
            None,
            Some(attempt),
            None,
            None,
            None,
        ) => (
            ProcessTurnState::Failed {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: Some(TurnAttemptId::from_uuid(attempt)),
                terminal_model_call: None,
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("failed"),
            None,
            Some(attempt),
            Some(call),
            Some(disposition @ ("known_failed" | "cancelled")),
            None,
        ) => (
            ProcessTurnState::Failed {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: Some(TurnAttemptId::from_uuid(attempt)),
                terminal_model_call: Some(ProcessFailedTerminalModelCall {
                    call: ModelCallId::from_uuid(call),
                    disposition: match disposition {
                        "known_failed" => ProcessFailedModelCallDisposition::KnownFailed,
                        "cancelled" => ProcessFailedModelCallDisposition::Cancelled,
                        _ => {
                            return Err(ProcessReadCorruption::Inconsistent(
                                "failed terminal model call disposition",
                            )
                            .into());
                        }
                    },
                    provider_failure_cause: terminal_call_provider_failure_cause
                        .as_deref()
                        .map(decode_provider_failure_cause)
                        .transpose()?,
                    attachment_preparation_failure_cause:
                        terminal_call_attachment_preparation_failure_cause
                            .as_deref()
                            .map(decode_attachment_preparation_failure_cause)
                            .transpose()?,
                }),
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("completed"),
            None,
            Some(attempt),
            Some(call),
            Some("completed"),
            None,
        ) => (
            ProcessTurnState::Completed {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: TurnAttemptId::from_uuid(attempt),
                terminal_call: ModelCallId::from_uuid(call),
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("refused"),
            None,
            Some(attempt),
            Some(call),
            Some("refused"),
            None,
        ) => (
            ProcessTurnState::Refused {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: TurnAttemptId::from_uuid(attempt),
                terminal_call: ModelCallId::from_uuid(call),
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("cancelled"),
            None,
            Some(attempt),
            None,
            None,
            None,
        ) => (
            ProcessTurnState::Cancelled {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: TurnAttemptId::from_uuid(attempt),
                terminal_call: None,
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("cancelled"),
            None,
            Some(attempt),
            Some(call),
            Some("cancelled"),
            None,
        ) => (
            ProcessTurnState::Cancelled {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: TurnAttemptId::from_uuid(attempt),
                terminal_call: Some(ModelCallId::from_uuid(call)),
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("reconciliation_required"),
            None,
            Some(attempt),
            Some(call),
            Some("ambiguous"),
            None,
        ) => (
            ProcessTurnState::ReconciliationRequired {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: TurnAttemptId::from_uuid(attempt),
                operation: ProcessReconciliationOperation::ModelCall(ModelCallId::from_uuid(call)),
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        _ => {
            return Err(ProcessReadCorruption::Inconsistent("turn lifecycle state shape").into());
        }
    };

    project_logical_delegation_terminal(
        DecodedTurn {
            turn: ProcessTranscriptTurn {
                turn,
                acceptance_position,
                model_settings,
                state,
            },
            start_lineage,
            latest_frontier,
        },
        logical_terminal,
    )
}

#[derive(Clone, Copy)]
struct LogicalDelegationTerminalProjection {
    spawning_request: ToolRequestId,
    terminal_frontier: ContextFrontierId,
    outcome: DispatchedDelegationOutcome,
    reason: DispatchedDelegationReason,
    provenance: DispatchedDelegationProvenance,
}

fn decode_logical_delegation_terminal(
    row: &PgRow,
) -> Result<Option<LogicalDelegationTerminalProjection>, ProcessReadError> {
    let spawning_request: Option<Uuid> = row.try_get("logical_terminal_spawning_request_id")?;
    let terminal_frontier: Option<Uuid> = row.try_get("logical_terminal_frontier_id")?;
    let outcome: Option<String> = row.try_get("logical_terminal_outcome_kind")?;
    let reason: Option<String> = row.try_get("logical_terminal_reason_kind")?;
    match (spawning_request, outcome.as_deref(), reason.as_deref()) {
        (None, None, None) => Ok(None),
        (Some(spawning_request), Some(outcome), Some(reason)) => {
            let terminal_frontier = terminal_frontier.ok_or(
                ProcessReadCorruption::Inconsistent("logical delegation terminal frontier"),
            )?;
            let outcome = decode_delegation_outcome(outcome).map_err(|_| {
                ProcessReadCorruption::Inconsistent("logical delegation terminal outcome")
            })?;
            let reason = decode_delegation_reason(reason).map_err(|_| {
                ProcessReadCorruption::Inconsistent("logical delegation terminal reason")
            })?;
            let provenance = decode_delegation_provenance(row).map_err(|_| {
                ProcessReadCorruption::Inconsistent("logical delegation terminal provenance")
            })?;
            if !matches!(
                (outcome, reason),
                (
                    DispatchedDelegationOutcome::ChildStopped,
                    DispatchedDelegationReason::ParentStoppedWithDescendants
                ) | (
                    DispatchedDelegationOutcome::ChildStopped,
                    DispatchedDelegationReason::ParentCancelledWithDescendants
                ) | (
                    DispatchedDelegationOutcome::ChildCancelled,
                    DispatchedDelegationReason::ParentStoppedWithDescendants
                ) | (
                    DispatchedDelegationOutcome::ChildCancelled,
                    DispatchedDelegationReason::ParentCancelledWithDescendants
                )
            ) || !matches!(
                provenance,
                DispatchedDelegationProvenance::ParentTurnCommand { .. }
                    | DispatchedDelegationProvenance::ParentGoalCommand { .. }
                    | DispatchedDelegationProvenance::ParentLifecycleCommand { .. }
            ) {
                return Err(ProcessReadCorruption::Inconsistent(
                    "logical delegation terminal shape",
                )
                .into());
            }
            Ok(Some(LogicalDelegationTerminalProjection {
                spawning_request: ToolRequestId::from_uuid(spawning_request),
                terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
                outcome,
                reason,
                provenance,
            }))
        }
        _ => Err(
            ProcessReadCorruption::Inconsistent("logical delegation terminal correlation").into(),
        ),
    }
}

fn project_logical_delegation_terminal(
    mut decoded: DecodedTurn,
    logical_terminal: Option<LogicalDelegationTerminalProjection>,
) -> Result<DecodedTurn, ProcessReadError> {
    if let Some(logical_terminal) = logical_terminal {
        decoded.turn.state = ProcessTurnState::DelegationTerminated {
            spawning_request: logical_terminal.spawning_request,
            outcome: logical_terminal.outcome,
            reason: logical_terminal.reason,
            provenance: logical_terminal.provenance,
        };
        // The physical decode observed a mid-execution boundary, but the
        // cascade froze this turn at the logical terminal's frontier and every
        // successor chains from it. Rendering the physical frontier would show
        // execution evidence no successor model call ever saw, and would vanish
        // from the same transcript as soon as a successor activates. A turn
        // terminalized while still queued started no execution lineage at all,
        // so it keeps the absent frontier its start lineage pairs with.
        if decoded.start_lineage.is_some() {
            decoded.latest_frontier = Some(logical_terminal.terminal_frontier);
        }
    }
    Ok(decoded)
}

async fn open_transcript_entry_cursor(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
    frontier: ContextFrontierId,
) -> Result<u64, ProcessReadError> {
    let stored_member_count: Option<Decimal> = sqlx::query_scalar(
        "SELECT member_count
           FROM context_frontier
          WHERE owning_session_id = $1
            AND context_frontier_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    let member_count = decode_nonnegative(
        stored_member_count.ok_or(ProcessReadCorruption::Missing("context frontier"))?,
        "context frontier member count",
    )?;
    // The transaction-scoped cursor retains this query's execution state, so
    // every later FETCH advances the same single recursive chain resolution.
    sqlx::query(
        "DECLARE signalbox_process_transcript_entries NO SCROLL CURSOR FOR
         SELECT
            member.actual_member_count,
            member.member_position,
            member.source_session_id,
            member.semantic_entry_id,
            entry.payload_kind,
            entry.origin_accepted_input_id,
            entry.steering_source_turn_id,
            entry.failed_turn_id,
            entry.assistant_text_value,
            entry.producing_model_call_id,
            entry.assistant_tool_request_id,
            entry.tool_result_request_id,
            entry.tool_result_attempt_id,
            entry.completed_turn_id,
            entry.cancelled_turn_id,
            entry.imported_conversation_id,
            entry.imported_transcript_entry_id,
            entry.model_identity_turn_id,
            entry.model_identity_defaults_version,
            entry.model_identity_direct_selection_id,
            entry.context_summary_value,
            entry.context_summary_producing_call_id,
            entry.context_summary_first_source_session_id,
            entry.context_summary_first_entry_id,
            entry.context_summary_through_source_session_id,
            entry.context_summary_through_entry_id,
            entry.delegated_task_spawning_tool_request_id,
            entry.delegation_message_id,
            entry.delegation_result_awaiting_tool_request_id,
            entry.delegation_result_spawning_tool_request_id,
            delegated_task.task_content AS delegated_task_content,
            task_relation.parent_session_id AS delegated_task_parent_session_id,
            task_relation.parent_turn_id AS delegated_task_parent_turn_id,
            delegated_message.spawning_tool_request_id AS delegation_message_spawning_request_id,
            delegated_message.event_ordinal AS delegation_message_ordinal,
            delegated_message.content_text AS delegation_message_content,
            message_delivery.recipient_session_id AS delegation_message_recipient_session_id,
            message_delivery.delivery_sequence AS delegation_message_delivery_sequence,
            CASE delegated_message.direction
                WHEN 'parent_to_child' THEN message_relation.parent_session_id
                WHEN 'child_to_parent' THEN message_relation.child_session_id
            END AS delegation_message_sender_session_id,
            delegated_wait.child_session_id AS delegation_result_child_session_id,
            delegated_wait.wait_mode AS delegation_result_wait_mode,
            result_delivery.delivery_sequence AS delegation_result_delivery_sequence,
            delegated_result.outcome_kind AS delegation_result_outcome_kind,
            delegated_result.content_text AS delegation_result_content,
            result_event.reason_kind AS delegation_result_reason_kind,
            result_event.provenance_kind,
            result_event.provenance_session_id,
            result_event.provenance_turn_id,
            result_event.provenance_goal_generation,
            result_event.provenance_command_id,
            imported.source_speaker_kind AS imported_source_speaker_kind,
            imported.content_encoding AS imported_content_encoding,
            CASE WHEN accepted.accepted_input_id IS NULL THEN NULL
                 ELSE accepted_input_content_parts_json(
                    accepted.accepted_input_id)
            END AS origin_content,
            accepted.origin_turn_id,
            call.turn_id AS assistant_turn_id,
            result_attempt.request_id AS result_attempt_request_id,
            transcript_request.tool_name AS transcript_tool_name,
            transcript_request.arguments_text AS transcript_tool_arguments,
            result_attempt.terminal_disposition_kind AS result_disposition,
            result_attempt.result_text AS result_text,
            result_attempt.error_kind AS result_error_kind,
            result_attempt.error_detail AS result_error_detail,
            transcript_approval.decision_kind AS transcript_decision_kind,
            transcript_approval.decision_source AS transcript_decision_source,
            transcript_approval.denial_reason AS transcript_denial_reason,
            transcript_approval.user_command_id AS transcript_user_command_id,
            transcript_approval.delegate_model_selection_id AS transcript_delegate_model_selection_id,
            transcript_approval.delegate_model_call_id AS transcript_delegate_model_call_id,
            transcript_approval.rationale AS transcript_decision_rationale,
            transcript_approval.override_denied_request_id
                AS transcript_override_denied_request_id,
            transcript_override.command_id AS transcript_override_command_id
           FROM (
                SELECT
                    resolved.*,
                    count(*) OVER () AS actual_member_count
                  FROM resolve_context_frontier_members($1, $2) AS resolved
           ) AS member
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
           LEFT JOIN accepted_input AS accepted
             ON accepted.session_id = entry.source_session_id
            AND accepted.accepted_input_id = entry.origin_accepted_input_id
           LEFT JOIN model_call AS call
             ON call.session_id = entry.source_session_id
            AND call.model_call_id = entry.producing_model_call_id
           LEFT JOIN tool_attempt AS result_attempt
             ON result_attempt.session_id = entry.source_session_id
            AND result_attempt.attempt_id = entry.tool_result_attempt_id
           LEFT JOIN tool_request AS transcript_request
             ON transcript_request.session_id = entry.source_session_id
            AND transcript_request.request_id = COALESCE(
                entry.assistant_tool_request_id,
                entry.tool_result_request_id,
                result_attempt.request_id
            )
           LEFT JOIN tool_approval_decision AS transcript_approval
             ON transcript_approval.request_id = transcript_request.request_id
           LEFT JOIN tool_approval_user_override AS transcript_override
             ON transcript_override.denied_request_id =
                transcript_approval.override_denied_request_id
           LEFT JOIN imported_transcript_entry AS imported
             ON imported.imported_conversation_id =
                    entry.imported_conversation_id
            AND imported.imported_transcript_entry_id =
                    entry.imported_transcript_entry_id
           LEFT JOIN session_delegation_initial_task AS delegated_task
             ON delegated_task.spawning_tool_request_id =
                    entry.delegated_task_spawning_tool_request_id
            AND delegated_task.child_session_id = entry.source_session_id
            AND delegated_task.semantic_entry_id = entry.semantic_entry_id
           LEFT JOIN session_delegation AS task_relation
             ON task_relation.spawning_tool_request_id =
                    delegated_task.spawning_tool_request_id
           LEFT JOIN session_message_delivery AS message_delivery
             ON message_delivery.message_id = entry.delegation_message_id
            AND message_delivery.recipient_session_id = entry.source_session_id
           LEFT JOIN session_message AS delegated_message
             ON delegated_message.message_id = message_delivery.message_id
            AND delegated_message.spawning_tool_request_id =
                    message_delivery.spawning_tool_request_id
           LEFT JOIN session_delegation AS message_relation
             ON message_relation.spawning_tool_request_id =
                    delegated_message.spawning_tool_request_id
           LEFT JOIN session_child_result_delivery AS result_delivery
             ON result_delivery.awaiting_tool_request_id =
                    entry.delegation_result_awaiting_tool_request_id
            AND result_delivery.spawning_tool_request_id =
                    entry.delegation_result_spawning_tool_request_id
            AND result_delivery.parent_session_id = entry.source_session_id
           LEFT JOIN session_delegation_wait AS delegated_wait
             ON delegated_wait.awaiting_tool_request_id =
                    result_delivery.awaiting_tool_request_id
            AND delegated_wait.spawning_tool_request_id =
                    result_delivery.spawning_tool_request_id
            AND delegated_wait.parent_session_id = result_delivery.parent_session_id
           LEFT JOIN session_child_result AS delegated_result
             ON delegated_result.spawning_tool_request_id =
                    result_delivery.spawning_tool_request_id
           LEFT JOIN session_delegation_event AS result_event
             ON result_event.spawning_tool_request_id =
                    delegated_result.spawning_tool_request_id
            AND result_event.event_ordinal = delegated_result.event_ordinal
            AND result_event.event_kind = delegated_result.event_kind
          ORDER BY member.member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .execute(&mut **transaction)
    .await?;
    Ok(member_count)
}

async fn fetch_next_transcript_entry(
    transaction: &mut Transaction<'static, Postgres>,
    entry_index: u64,
    expected_entry_count: u64,
) -> Result<Option<ProcessTranscriptEntry>, ProcessReadError> {
    let row = sqlx::query("FETCH NEXT FROM signalbox_process_transcript_entries")
        .fetch_optional(&mut **transaction)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let actual_entry_count: i64 = required(&row, "actual_member_count")?;
    if u64::try_from(actual_entry_count)
        .map_err(|_| ProcessReadCorruption::InvalidOrdinal("transcript entry count"))?
        != expected_entry_count
    {
        return Err(
            ProcessReadCorruption::Inconsistent("context frontier declared membership").into(),
        );
    }
    let member_position =
        entry_index
            .checked_add(1)
            .ok_or(ProcessReadCorruption::InvalidOrdinal(
                "frontier member position",
            ))?;
    let stored_position = decode_positive(
        required(&row, "member_position")?,
        "frontier member position",
    )?;
    if stored_position != member_position {
        return Err(
            ProcessReadCorruption::Inconsistent("context frontier contiguous membership").into(),
        );
    }
    decode_transcript_entry(&row, entry_index).map(Some)
}

fn decode_transcript_entry(
    row: &PgRow,
    entry_index: u64,
) -> Result<ProcessTranscriptEntry, ProcessReadError> {
    let source_session = session_id_from_uuid(required(row, "source_session_id")?);
    let entry = SemanticTranscriptEntryId::from_uuid(required(row, "semantic_entry_id")?);
    let payload_kind: String = required(row, "payload_kind")?;
    let origin: Option<Uuid> = row.try_get("origin_accepted_input_id")?;
    let steering_source_turn: Option<Uuid> = row.try_get("steering_source_turn_id")?;
    let failed_turn: Option<Uuid> = row.try_get("failed_turn_id")?;
    let assistant_text: Option<String> = row.try_get("assistant_text_value")?;
    let producing_call: Option<Uuid> = row.try_get("producing_model_call_id")?;
    let tool_request: Option<Uuid> = row.try_get("assistant_tool_request_id")?;
    let tool_result_request: Option<Uuid> = row.try_get("tool_result_request_id")?;
    let tool_result_attempt: Option<Uuid> = row.try_get("tool_result_attempt_id")?;
    let completed_turn: Option<Uuid> = row.try_get("completed_turn_id")?;
    let cancelled_turn: Option<Uuid> = row.try_get("cancelled_turn_id")?;
    let imported_conversation: Option<Uuid> = row.try_get("imported_conversation_id")?;
    let imported_entry: Option<Uuid> = row.try_get("imported_transcript_entry_id")?;
    let model_identity_turn: Option<Uuid> = row.try_get("model_identity_turn_id")?;
    let model_identity_defaults_version: Option<Decimal> =
        row.try_get("model_identity_defaults_version")?;
    let model_identity_direct_selection: Option<Uuid> =
        row.try_get("model_identity_direct_selection_id")?;
    let context_summary_value: Option<String> = row.try_get("context_summary_value")?;
    let context_summary_call: Option<Uuid> = row.try_get("context_summary_producing_call_id")?;
    let context_summary_first_source_session: Option<Uuid> =
        row.try_get("context_summary_first_source_session_id")?;
    let context_summary_first_entry: Option<Uuid> =
        row.try_get("context_summary_first_entry_id")?;
    let context_summary_through_source_session: Option<Uuid> =
        row.try_get("context_summary_through_source_session_id")?;
    let context_summary_through_entry: Option<Uuid> =
        row.try_get("context_summary_through_entry_id")?;
    let imported_source_speaker: Option<String> = row.try_get("imported_source_speaker_kind")?;
    let imported_content: Option<Vec<u8>> = row.try_get("imported_content_encoding")?;
    let origin_content: Option<Value> = row.try_get("origin_content")?;
    let origin_turn: Option<Uuid> = row.try_get("origin_turn_id")?;
    let assistant_turn: Option<Uuid> = row.try_get("assistant_turn_id")?;
    let result_attempt_request: Option<Uuid> = row.try_get("result_attempt_request_id")?;
    let transcript_tool_name: Option<String> = row.try_get("transcript_tool_name")?;
    let transcript_tool_arguments: Option<String> = row.try_get("transcript_tool_arguments")?;
    let result_disposition: Option<String> = row.try_get("result_disposition")?;
    let result_text: Option<String> = row.try_get("result_text")?;
    let result_error_kind: Option<String> = row.try_get("result_error_kind")?;
    let result_error_detail: Option<String> = row.try_get("result_error_detail")?;
    let transcript_decision_kind: Option<String> = row.try_get("transcript_decision_kind")?;
    let transcript_denial_reason: Option<String> = row.try_get("transcript_denial_reason")?;
    let delegated_task_spawning_request: Option<Uuid> =
        row.try_get("delegated_task_spawning_tool_request_id")?;
    let delegation_message: Option<Uuid> = row.try_get("delegation_message_id")?;
    let delegation_result_awaiting_request: Option<Uuid> =
        row.try_get("delegation_result_awaiting_tool_request_id")?;
    let delegation_result_spawning_request: Option<Uuid> =
        row.try_get("delegation_result_spawning_tool_request_id")?;
    let delegated_task_content: Option<String> = row.try_get("delegated_task_content")?;
    let delegated_task_parent_session: Option<Uuid> =
        row.try_get("delegated_task_parent_session_id")?;
    let delegated_task_parent_turn: Option<Uuid> = row.try_get("delegated_task_parent_turn_id")?;
    let delegation_message_spawning_request: Option<Uuid> =
        row.try_get("delegation_message_spawning_request_id")?;
    let delegation_message_ordinal: Option<Decimal> = row.try_get("delegation_message_ordinal")?;
    let delegation_message_content: Option<String> = row.try_get("delegation_message_content")?;
    let delegation_message_sender: Option<Uuid> =
        row.try_get("delegation_message_sender_session_id")?;
    let delegation_message_recipient: Option<Uuid> =
        row.try_get("delegation_message_recipient_session_id")?;
    let delegation_message_delivery_sequence: Option<Decimal> =
        row.try_get("delegation_message_delivery_sequence")?;
    let delegation_result_child: Option<Uuid> =
        row.try_get("delegation_result_child_session_id")?;
    let delegation_result_wait_mode: Option<String> = row.try_get("delegation_result_wait_mode")?;
    let delegation_result_delivery_sequence: Option<Decimal> =
        row.try_get("delegation_result_delivery_sequence")?;
    let delegation_result_outcome: Option<String> =
        row.try_get("delegation_result_outcome_kind")?;
    let delegation_result_content: Option<String> = row.try_get("delegation_result_content")?;
    let delegation_result_reason: Option<String> = row.try_get("delegation_result_reason_kind")?;

    let legacy_payload_present = origin.is_some()
        || steering_source_turn.is_some()
        || failed_turn.is_some()
        || assistant_text.is_some()
        || producing_call.is_some()
        || tool_request.is_some()
        || tool_result_attempt.is_some()
        || completed_turn.is_some()
        || cancelled_turn.is_some()
        || imported_conversation.is_some()
        || imported_entry.is_some()
        || model_identity_turn.is_some()
        || model_identity_defaults_version.is_some()
        || model_identity_direct_selection.is_some()
        || context_summary_value.is_some()
        || context_summary_call.is_some()
        || context_summary_first_source_session.is_some()
        || context_summary_first_entry.is_some()
        || context_summary_through_source_session.is_some()
        || context_summary_through_entry.is_some();

    if payload_kind == "delegated_task" {
        let (Some(spawning_request), Some(parent_session), Some(parent_turn), Some(content)) = (
            delegated_task_spawning_request,
            delegated_task_parent_session,
            delegated_task_parent_turn,
            delegated_task_content,
        ) else {
            return Err(ProcessReadCorruption::Inconsistent("delegated-task entry shape").into());
        };
        if legacy_payload_present
            || tool_result_request.is_some()
            || delegation_message.is_some()
            || delegation_result_awaiting_request.is_some()
            || delegation_result_spawning_request.is_some()
            || content.is_empty()
        {
            return Err(ProcessReadCorruption::Inconsistent("delegated-task entry shape").into());
        }
        return Ok(ProcessTranscriptEntry::DelegatedTask {
            entry_index,
            source_session,
            entry,
            spawning_request: ToolRequestId::from_uuid(spawning_request),
            parent_session: SessionId::from_uuid(parent_session),
            parent_turn: TurnId::from_uuid(parent_turn),
            content,
        });
    }

    if payload_kind == "delegation_message" {
        let (
            Some(message),
            Some(spawning_request),
            Some(sender),
            Some(recipient),
            Some(ordinal),
            Some(delivery_sequence),
            Some(content),
        ) = (
            delegation_message,
            delegation_message_spawning_request,
            delegation_message_sender,
            delegation_message_recipient,
            delegation_message_ordinal,
            delegation_message_delivery_sequence,
            delegation_message_content,
        )
        else {
            return Err(
                ProcessReadCorruption::Inconsistent("delegation-message entry shape").into(),
            );
        };
        if legacy_payload_present
            || tool_result_request.is_some()
            || delegated_task_spawning_request.is_some()
            || delegation_result_awaiting_request.is_some()
            || delegation_result_spawning_request.is_some()
            || recipient != source_session.into_uuid()
            || content.is_empty()
        {
            return Err(
                ProcessReadCorruption::Inconsistent("delegation-message entry shape").into(),
            );
        }
        return Ok(ProcessTranscriptEntry::DelegationMessage {
            entry_index,
            source_session,
            entry,
            spawning_request: ToolRequestId::from_uuid(spawning_request),
            message: DelegationMessageId::from_uuid(message),
            sender: SessionId::from_uuid(sender),
            recipient: SessionId::from_uuid(recipient),
            ordinal: decode_positive(ordinal, "delegation message ordinal")?,
            delivery_sequence: decode_positive(
                delivery_sequence,
                "delegation message delivery sequence",
            )?,
            content,
        });
    }

    if payload_kind == "delegation_result" {
        let (
            Some(awaiting_request),
            Some(spawning_request),
            Some(child),
            Some(wait_mode),
            Some(outcome),
            Some(reason),
        ) = (
            delegation_result_awaiting_request,
            delegation_result_spawning_request,
            delegation_result_child,
            delegation_result_wait_mode.as_deref(),
            delegation_result_outcome.as_deref(),
            delegation_result_reason.as_deref(),
        )
        else {
            return Err(
                ProcessReadCorruption::Inconsistent("delegation-result entry shape").into(),
            );
        };
        let mode = decode_wait_mode(wait_mode)
            .map_err(|_| ProcessReadCorruption::Inconsistent("delegation-result wait mode"))?;
        let delivery_sequence = delegation_result_delivery_sequence
            .map(|value| decode_positive(value, "delegation result delivery sequence"))
            .transpose()?;
        let foreground_correlation = tool_result_request == Some(awaiting_request);
        if legacy_payload_present
            || delegated_task_spawning_request.is_some()
            || delegation_message.is_some()
            || (mode == DispatchedDelegationWaitMode::Foreground
                && (!foreground_correlation || delivery_sequence.is_some()))
            || (mode == DispatchedDelegationWaitMode::Background
                && (tool_result_request.is_some() || delivery_sequence.is_none()))
        {
            return Err(
                ProcessReadCorruption::Inconsistent("delegation-result entry shape").into(),
            );
        }
        return Ok(ProcessTranscriptEntry::DelegationResult {
            entry_index,
            source_session,
            entry,
            awaiting_request: ToolRequestId::from_uuid(awaiting_request),
            spawning_request: ToolRequestId::from_uuid(spawning_request),
            child: SessionId::from_uuid(child),
            mode,
            delivery_sequence,
            outcome: decode_delegation_outcome(outcome)
                .map_err(|_| ProcessReadCorruption::Inconsistent("delegation-result outcome"))?,
            content: delegation_result_content,
            reason: decode_delegation_reason(reason)
                .map_err(|_| ProcessReadCorruption::Inconsistent("delegation-result reason"))?,
            provenance: decode_delegation_provenance(row)
                .map_err(|_| ProcessReadCorruption::Inconsistent("delegation-result provenance"))?,
        });
    }

    if delegated_task_spawning_request.is_some()
        || delegation_message.is_some()
        || delegation_result_awaiting_request.is_some()
        || delegation_result_spawning_request.is_some()
    {
        return Err(
            ProcessReadCorruption::Inconsistent("non-delegation semantic entry fields").into(),
        );
    }

    let transcript_approval = decode_process_tool_approval(row)?;

    if payload_kind == "context_summary" {
        let (
            Some(content),
            Some(call),
            Some(first_source_session),
            Some(first_entry),
            Some(through_source_session),
            Some(through_entry),
        ) = (
            context_summary_value,
            context_summary_call,
            context_summary_first_source_session,
            context_summary_first_entry,
            context_summary_through_source_session,
            context_summary_through_entry,
        )
        else {
            return Err(ProcessReadCorruption::Inconsistent("context-summary entry shape").into());
        };
        if origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || assistant_text.is_some()
            || producing_call.is_some()
            || tool_request.is_some()
            || tool_result_request.is_some()
            || tool_result_attempt.is_some()
            || completed_turn.is_some()
            || cancelled_turn.is_some()
            || imported_conversation.is_some()
            || imported_entry.is_some()
            || model_identity_turn.is_some()
            || model_identity_defaults_version.is_some()
            || model_identity_direct_selection.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("context-summary entry shape").into());
        }
        return Ok(ProcessTranscriptEntry::ContextSummary {
            entry_index,
            source_session,
            entry,
            model_call: ModelCallId::from_uuid(call),
            first: signalbox_domain::SemanticTranscriptEntryRef::from_source(
                session_id_from_uuid(first_source_session),
                SemanticTranscriptEntryId::from_uuid(first_entry),
            ),
            through: signalbox_domain::SemanticTranscriptEntryRef::from_source(
                session_id_from_uuid(through_source_session),
                SemanticTranscriptEntryId::from_uuid(through_entry),
            ),
            content,
        });
    }
    if context_summary_value.is_some()
        || context_summary_call.is_some()
        || context_summary_first_source_session.is_some()
        || context_summary_first_entry.is_some()
        || context_summary_through_source_session.is_some()
        || context_summary_through_entry.is_some()
    {
        return Err(
            ProcessReadCorruption::Inconsistent("non-summary context-summary fields").into(),
        );
    }

    if payload_kind == "model_identity_changed" {
        if origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || assistant_text.is_some()
            || producing_call.is_some()
            || tool_request.is_some()
            || tool_result_request.is_some()
            || tool_result_attempt.is_some()
            || completed_turn.is_some()
            || cancelled_turn.is_some()
            || imported_conversation.is_some()
            || imported_entry.is_some()
        {
            return Err(
                ProcessReadCorruption::Inconsistent("model identity semantic entry shape").into(),
            );
        }
        return Ok(ProcessTranscriptEntry::ModelIdentityChanged {
            entry_index,
            source_session,
            entry,
            turn: TurnId::from_uuid(
                model_identity_turn.ok_or(ProcessReadCorruption::Missing("model identity turn"))?,
            ),
            defaults_version: decode_positive(
                model_identity_defaults_version.ok_or(ProcessReadCorruption::Missing(
                    "model identity defaults version",
                ))?,
                "model identity defaults version",
            )?,
            selected: DirectModelSelection::from_uuid(model_identity_direct_selection.ok_or(
                ProcessReadCorruption::Missing("model identity direct selection"),
            )?),
        });
    }
    if model_identity_turn.is_some()
        || model_identity_defaults_version.is_some()
        || model_identity_direct_selection.is_some()
    {
        return Err(
            ProcessReadCorruption::Inconsistent("native semantic model identity fields").into(),
        );
    }

    if payload_kind == "assistant_tool_use" {
        let (Some(call), Some(request), Some(turn), Some(name), Some(arguments)) = (
            producing_call,
            tool_request,
            assistant_turn,
            transcript_tool_name,
            transcript_tool_arguments,
        ) else {
            return Err(
                ProcessReadCorruption::Inconsistent("assistant tool-use entry shape").into(),
            );
        };
        if origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || assistant_text.is_some()
            || tool_result_request.is_some()
            || tool_result_attempt.is_some()
            || result_attempt_request.is_some()
            || completed_turn.is_some()
            || cancelled_turn.is_some()
            || origin_content.is_some()
            || origin_turn.is_some()
        {
            return Err(
                ProcessReadCorruption::Inconsistent("assistant tool-use entry shape").into(),
            );
        }
        return Ok(ProcessTranscriptEntry::AssistantToolUse {
            entry_index,
            source_session,
            entry,
            turn: TurnId::from_uuid(turn),
            model_call: ModelCallId::from_uuid(call),
            request: ToolRequestId::from_uuid(request),
            name,
            arguments,
            approval: transcript_approval,
        });
    }

    if payload_kind == "tool_execution_result" {
        let (Some(attempt), Some(request), Some(disposition)) = (
            tool_result_attempt,
            result_attempt_request,
            result_disposition.as_deref(),
        ) else {
            return Err(
                ProcessReadCorruption::Inconsistent("tool execution-result entry shape").into(),
            );
        };
        let disposition = decode_tool_result_disposition(disposition)?;
        let (disposition, content) = match (
            disposition,
            result_text,
            result_error_kind,
            result_error_detail,
        ) {
            (ToolAttemptDispositionStorageKind::Completed, Some(text), None, None) => {
                (ProcessToolExecutionResultDisposition::Completed, text)
            }
            (ToolAttemptDispositionStorageKind::KnownFailed, None, Some(kind), detail) => (
                ProcessToolExecutionResultDisposition::KnownFailed,
                serde_json::json!({
                    "error": {
                        "kind": kind,
                        "detail": detail,
                    }
                })
                .to_string(),
            ),
            (ToolAttemptDispositionStorageKind::Completed, _, _, _)
            | (ToolAttemptDispositionStorageKind::KnownFailed, _, _, _)
            | (ToolAttemptDispositionStorageKind::AwaitingChild, _, _, _)
            | (ToolAttemptDispositionStorageKind::Ambiguous, _, _, _) => {
                return Err(
                    ProcessReadCorruption::Inconsistent("tool execution-result evidence").into(),
                );
            }
        };
        if origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || assistant_text.is_some()
            || producing_call.is_some()
            || tool_request.is_some()
            || tool_result_request.is_some()
            || completed_turn.is_some()
            || cancelled_turn.is_some()
            || origin_content.is_some()
            || origin_turn.is_some()
            || assistant_turn.is_some()
        {
            return Err(
                ProcessReadCorruption::Inconsistent("tool execution-result entry shape").into(),
            );
        }
        return Ok(ProcessTranscriptEntry::ToolExecutionResult {
            entry_index,
            source_session,
            entry,
            request: ToolRequestId::from_uuid(request),
            attempt: ToolAttemptId::from_uuid(attempt),
            disposition,
            content,
        });
    }

    if matches!(
        payload_kind.as_str(),
        "tool_denied" | "tool_closed_by_turn_end"
    ) {
        let Some(request) = tool_result_request else {
            return Err(ProcessReadCorruption::Inconsistent("tool result entry shape").into());
        };
        if origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || assistant_text.is_some()
            || producing_call.is_some()
            || tool_request.is_some()
            || tool_result_attempt.is_some()
            || result_attempt_request.is_some()
            || completed_turn.is_some()
            || cancelled_turn.is_some()
            || origin_content.is_some()
            || origin_turn.is_some()
            || assistant_turn.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("tool result entry shape").into());
        }
        return Ok(if payload_kind == "tool_denied" {
            if transcript_decision_kind.as_deref() != Some("deny") {
                return Err(ProcessReadCorruption::Inconsistent("tool denial decision").into());
            }
            ProcessTranscriptEntry::ToolDenied {
                entry_index,
                source_session,
                entry,
                request: ToolRequestId::from_uuid(request),
                content: serde_json::json!({
                    "error": {
                        "kind": "denied",
                        "detail": transcript_denial_reason,
                    }
                })
                .to_string(),
            }
        } else {
            ProcessTranscriptEntry::ToolClosed {
                entry_index,
                source_session,
                entry,
                request: ToolRequestId::from_uuid(request),
                content: String::from(r#"{"error":{"detail":null,"kind":"closed_by_turn_end"}}"#),
            }
        });
    }

    if tool_result_request.is_some()
        || tool_result_attempt.is_some()
        || result_attempt_request.is_some()
    {
        return Err(ProcessReadCorruption::Inconsistent("semantic transcript tool fields").into());
    }

    if payload_kind == "imported_entry" {
        if origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || assistant_text.is_some()
            || producing_call.is_some()
            || tool_request.is_some()
            || completed_turn.is_some()
            || cancelled_turn.is_some()
            || origin_content.is_some()
            || origin_turn.is_some()
            || assistant_turn.is_some()
        {
            return Err(
                ProcessReadCorruption::Inconsistent("imported semantic entry shape").into(),
            );
        }
        let imported_conversation =
            ImportedConversationId::from_uuid(imported_conversation.ok_or(
                ProcessReadCorruption::Missing("imported conversation identity"),
            )?);
        let imported_entry = ImportedTranscriptEntryId::from_uuid(
            imported_entry.ok_or(ProcessReadCorruption::Missing("imported entry identity"))?,
        );
        let source_speaker = decode_imported_source_speaker(
            imported_source_speaker
                .ok_or(ProcessReadCorruption::Missing("imported source speaker"))?,
        )?;
        let content = decode_content(
            imported_content
                .as_deref()
                .ok_or(ProcessReadCorruption::Missing("imported content encoding"))?,
        )
        .map_err(|_| ProcessReadCorruption::Inconsistent("imported content encoding"))?;
        return Ok(project_imported_entry(
            entry_index,
            source_session,
            entry,
            imported_conversation,
            imported_entry,
            source_speaker,
            content,
        ));
    }

    if imported_conversation.is_some()
        || imported_entry.is_some()
        || imported_source_speaker.is_some()
        || imported_content.is_some()
    {
        return Err(ProcessReadCorruption::Inconsistent("native semantic entry shape").into());
    }

    let projected = match (
        payload_kind.as_str(),
        origin,
        steering_source_turn,
        failed_turn,
        assistant_text,
        producing_call,
        tool_request,
        completed_turn,
        cancelled_turn,
        origin_content,
        origin_turn,
        assistant_turn,
    ) {
        (
            "origin_accepted_input",
            Some(accepted_input),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(content),
            Some(turn),
            None,
        ) => ProcessTranscriptEntry::User {
            entry_index,
            source_session,
            entry,
            accepted_input: AcceptedInputId::from_uuid(accepted_input),
            turn: TurnId::from_uuid(turn),
            content: crate::user_content::decode(content).map_err(|_| {
                ProcessReadCorruption::Inconsistent("semantic accepted-input content")
            })?,
        },
        (
            "steering_accepted_input",
            Some(accepted_input),
            Some(turn),
            None,
            None,
            None,
            None,
            None,
            None,
            Some(content),
            None,
            None,
        ) => ProcessTranscriptEntry::User {
            entry_index,
            source_session,
            entry,
            accepted_input: AcceptedInputId::from_uuid(accepted_input),
            turn: TurnId::from_uuid(turn),
            content: crate::user_content::decode(content).map_err(|_| {
                ProcessReadCorruption::Inconsistent("semantic accepted-input content")
            })?,
        },
        (
            "assistant_text",
            None,
            None,
            None,
            Some(content),
            Some(call),
            None,
            None,
            None,
            None,
            None,
            Some(turn),
        ) if !content.is_empty() => ProcessTranscriptEntry::Assistant {
            entry_index,
            source_session,
            entry,
            turn: TurnId::from_uuid(turn),
            model_call: ModelCallId::from_uuid(call),
            content,
        },
        ("turn_failed", None, None, Some(turn), None, None, None, None, None, None, None, None) => {
            ProcessTranscriptEntry::TurnFailed {
                entry_index,
                source_session,
                entry,
                turn: TurnId::from_uuid(turn),
            }
        }
        (
            "turn_completed",
            None,
            None,
            None,
            None,
            None,
            None,
            Some(turn),
            None,
            None,
            None,
            None,
        ) => ProcessTranscriptEntry::TurnCompleted {
            entry_index,
            source_session,
            entry,
            turn: TurnId::from_uuid(turn),
        },
        (
            "turn_cancelled",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(turn),
            None,
            None,
            None,
        ) => ProcessTranscriptEntry::TurnCancelled {
            entry_index,
            source_session,
            entry,
            turn: TurnId::from_uuid(turn),
        },
        (
            "origin_accepted_input"
            | "steering_accepted_input"
            | "assistant_text"
            | "assistant_tool_use"
            | "tool_execution_result"
            | "tool_denied"
            | "tool_closed_by_turn_end"
            | "turn_failed"
            | "turn_completed"
            | "turn_cancelled",
            _,
            _,
            _,
            _,
            _,
            _,
            _,
            _,
            _,
            _,
            _,
        ) => {
            return Err(
                ProcessReadCorruption::Inconsistent("semantic transcript entry shape").into(),
            );
        }
        _ => {
            return Err(ProcessReadCorruption::Unsupported {
                field: "semantic transcript payload kind",
                value: payload_kind,
            }
            .into());
        }
    };
    Ok(projected)
}

fn decode_tool_result_disposition(
    value: &str,
) -> Result<ToolAttemptDispositionStorageKind, ProcessReadCorruption> {
    tool_attempt_disposition_from_str(value).ok_or_else(|| ProcessReadCorruption::Unsupported {
        field: "terminal_disposition_kind",
        value: value.to_owned(),
    })
}

fn decode_process_tool_approval(
    row: &PgRow,
) -> Result<Option<ProcessToolApproval>, ProcessReadError> {
    let decision_kind: Option<String> = row.try_get("transcript_decision_kind")?;
    let source: Option<String> = row.try_get("transcript_decision_source")?;
    let denial_reason: Option<String> = row.try_get("transcript_denial_reason")?;
    let user_command: Option<Uuid> = row.try_get("transcript_user_command_id")?;
    let delegate_model: Option<Uuid> = row.try_get("transcript_delegate_model_selection_id")?;
    let delegate_call: Option<Uuid> = row.try_get("transcript_delegate_model_call_id")?;
    let rationale: Option<String> = row.try_get("transcript_decision_rationale")?;
    let override_denied: Option<Uuid> = row.try_get("transcript_override_denied_request_id")?;
    let override_command: Option<Uuid> = row.try_get("transcript_override_command_id")?;
    let Some(source) = source else {
        if decision_kind.is_some()
            || denial_reason.is_some()
            || user_command.is_some()
            || delegate_model.is_some()
            || delegate_call.is_some()
            || rationale.is_some()
            || override_denied.is_some()
            || override_command.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent(
                "tool approval projection without source",
            )
            .into());
        }
        return Ok(None);
    };
    let source_kind = tool_approval_decision_source_from_str(&source).ok_or_else(|| {
        ProcessReadError::from(ProcessReadCorruption::Unsupported {
            field: "tool approval decision source",
            value: source,
        })
    })?;
    let decision = match decision_kind.as_deref() {
        Some("approve") if denial_reason.is_none() => ToolApprovalDecision::Approve,
        Some("deny") => ToolApprovalDecision::Deny {
            reason: denial_reason
                .map(ToolDenialReason::try_new)
                .transpose()
                .map_err(|_| ProcessReadCorruption::Inconsistent("tool denial reason"))?,
        },
        _ => {
            return Err(ProcessReadCorruption::Inconsistent("tool approval decision kind").into());
        }
    };
    if source_kind != ToolApprovalDecisionSourceStorageKind::UserOverride
        && (override_denied.is_some() || override_command.is_some())
    {
        return Err(ProcessReadCorruption::Inconsistent("tool approval provenance shape").into());
    }
    let runtime_safety_decision = ToolApprovalResolutionReconstitutionInput::runtime_safety(
        ToolRequestId::from_uuid(Uuid::nil()),
    )
    .reconstitute()
    .map_err(|_| ProcessReadCorruption::Inconsistent("runtime safety approval evidence"))?;
    match (
        source_kind,
        user_command,
        delegate_model,
        delegate_call,
        rationale,
    ) {
        (
            ToolApprovalDecisionSourceStorageKind::PolicyAuto
            | ToolApprovalDecisionSourceStorageKind::SessionBlanket,
            None,
            None,
            None,
            None,
        ) if decision == ToolApprovalDecision::Approve => Ok(None),
        (ToolApprovalDecisionSourceStorageKind::RuntimeSafety, None, None, None, None)
            if runtime_safety_decision.decision() == &decision =>
        {
            Ok(None)
        }
        (ToolApprovalDecisionSourceStorageKind::LifecycleClosure, Some(_), None, None, None)
            if decision == (ToolApprovalDecision::Deny { reason: None }) =>
        {
            Ok(None)
        }
        (ToolApprovalDecisionSourceStorageKind::UserCommand, Some(command), None, None, None) => {
            Ok(Some(ProcessToolApproval {
                decision,
                decider: ToolApprovalDecider::User {
                    command: durable_command_id_from_uuid(command).map_err(|_| {
                        ProcessReadCorruption::Inconsistent("tool approval user command")
                    })?,
                },
                rationale: None,
            }))
        }
        (
            ToolApprovalDecisionSourceStorageKind::Delegate,
            None,
            Some(model),
            Some(call),
            Some(rationale),
        ) => {
            let rationale = ToolDecisionRationale::try_new(rationale)
                .map_err(|_| ProcessReadCorruption::Inconsistent("tool decision rationale"))?;
            // A delegate denial's stored reason equals the derivation from
            // its rationale — null exactly when the rationale derives
            // nothing — so missing current evidence reads as corruption.
            if let ToolApprovalDecision::Deny { ref reason } = decision
                && *reason != ToolDenialReason::from_rationale(&rationale)
            {
                return Err(ProcessReadCorruption::Inconsistent("delegate denial payload").into());
            }
            Ok(Some(ProcessToolApproval {
                decision,
                decider: ToolApprovalDecider::Delegate {
                    model: DirectModelSelection::from_uuid(model),
                    call: ModelCallId::from_uuid(call),
                },
                rationale: Some(rationale),
            }))
        }
        (ToolApprovalDecisionSourceStorageKind::UserOverride, None, None, None, None)
            if decision == ToolApprovalDecision::Approve =>
        {
            match (override_denied, override_command) {
                (Some(denied_request), Some(command)) => Ok(Some(ProcessToolApproval {
                    decision,
                    decider: ToolApprovalDecider::UserOverride {
                        command: durable_command_id_from_uuid(command).map_err(|_| {
                            ProcessReadCorruption::Inconsistent("tool approval override command")
                        })?,
                        denied_request: ToolRequestId::from_uuid(denied_request),
                    },
                    rationale: None,
                })),
                (None, _) | (_, None) => Err(ProcessReadCorruption::Inconsistent(
                    "tool approval provenance shape",
                )
                .into()),
            }
        }
        (
            ToolApprovalDecisionSourceStorageKind::PolicyAuto
            | ToolApprovalDecisionSourceStorageKind::SessionBlanket
            | ToolApprovalDecisionSourceStorageKind::UserCommand
            | ToolApprovalDecisionSourceStorageKind::Delegate
            | ToolApprovalDecisionSourceStorageKind::RuntimeSafety
            | ToolApprovalDecisionSourceStorageKind::LifecycleClosure
            | ToolApprovalDecisionSourceStorageKind::UserOverride,
            ..,
        ) => Err(ProcessReadCorruption::Inconsistent("tool approval provenance shape").into()),
    }
}

fn decode_imported_source_speaker(
    value: String,
) -> Result<ProcessImportedSourceSpeaker, ProcessReadError> {
    match value.as_str() {
        "not_attested" => Ok(ProcessImportedSourceSpeaker::NotAttested),
        "attested_absent" => Ok(ProcessImportedSourceSpeaker::AttestedAbsent),
        "attested_user" => Ok(ProcessImportedSourceSpeaker::User),
        "attested_assistant" => Ok(ProcessImportedSourceSpeaker::Assistant),
        _ => Err(ProcessReadCorruption::Unsupported {
            field: "imported source speaker",
            value,
        }
        .into()),
    }
}

fn project_imported_entry(
    entry_index: u64,
    source_session: SessionId,
    entry: SemanticTranscriptEntryId,
    imported_conversation: ImportedConversationId,
    imported_entry: ImportedTranscriptEntryId,
    source_speaker: ProcessImportedSourceSpeaker,
    content: ImportedTranscriptContent,
) -> ProcessTranscriptEntry {
    match content {
        ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(content)) => {
            ProcessTranscriptEntry::ImportedText {
                entry_index,
                source_session,
                entry,
                imported_conversation,
                imported_entry,
                source_speaker,
                content: content.into_string(),
            }
        }
        content => ProcessTranscriptEntry::Imported {
            entry_index,
            source_session,
            entry,
            imported_conversation,
            imported_entry,
            source_speaker,
            content_kind: match content {
                ImportedTranscriptContent::SourceEvent { .. } => {
                    ProcessImportedContentKind::SourceEvent
                }
                ImportedTranscriptContent::SourceMessageBlock { .. } => {
                    ProcessImportedContentKind::SourceMessageBlock
                }
                ImportedTranscriptContent::Text(_) => ProcessImportedContentKind::Text,
                ImportedTranscriptContent::ToolCall { .. } => ProcessImportedContentKind::ToolCall,
                ImportedTranscriptContent::ToolResult { .. } => {
                    ProcessImportedContentKind::ToolResult
                }
                ImportedTranscriptContent::Thinking { .. } => ProcessImportedContentKind::Thinking,
                ImportedTranscriptContent::RedactedThinking { .. } => {
                    ProcessImportedContentKind::RedactedThinking
                }
                ImportedTranscriptContent::Document { .. } => ProcessImportedContentKind::Document,
                ImportedTranscriptContent::MessageContentAbsent(_) => {
                    ProcessImportedContentKind::MessageContentAbsent
                }
            },
        },
    }
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, ProcessReadError>
where
    for<'row> T: sqlx::Decode<'row, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or_else(|| ProcessReadCorruption::Missing(field).into())
}

fn decode_nonnegative(value: Decimal, field: &'static str) -> Result<u64, ProcessReadCorruption> {
    if !value.fract().is_zero() || value.is_sign_negative() {
        return Err(ProcessReadCorruption::InvalidOrdinal(field));
    }
    u64::try_from(value).map_err(|_| ProcessReadCorruption::InvalidOrdinal(field))
}

fn decode_positive(value: Decimal, field: &'static str) -> Result<u64, ProcessReadCorruption> {
    let value = decode_nonnegative(value, field)?;
    if value == 0 {
        Err(ProcessReadCorruption::InvalidOrdinal(field))
    } else {
        Ok(value)
    }
}

fn decode_runner_generation(
    value: Decimal,
    field: &'static str,
) -> Result<RunnerGeneration, ProcessReadCorruption> {
    RunnerGeneration::try_from_u64(decode_nonnegative(value, field)?)
        .ok_or(ProcessReadCorruption::InvalidOrdinal(field))
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;
    use signalbox_domain::{SessionId, ToolRequestId, TurnId};
    use sqlx::types::Uuid;

    use super::{
        DecodedTurnOrigin, ProcessModelCallInputTokenSemantics, ProcessModelCallUsageProvenance,
        ProcessReadCorruption, decode_execution_lineage_tip, decode_tool_result_disposition,
        decode_transcript_turn_origin,
    };

    fn turn(value: u128) -> TurnId {
        TurnId::from_uuid(Uuid::from_u128(value))
    }

    /// S24: acceptance order A, B, C may execute as A, C, B; the
    /// database lineage diagnostic selects B as the one complete-chain tip.
    #[test]
    fn s24_latest_tip_follows_execution_lineage() {
        let second = turn(2);

        assert_eq!(
            decode_execution_lineage_tip(3, 1, 3, 1, false, false, Some(second))
                .expect("the lineage is one complete chain"),
            Some(second)
        );
    }

    /// a branched persisted execution lineage cannot choose one
    /// authoritative snapshot frontier and therefore fails closed.
    #[test]
    fn latest_frontier_rejects_branched_execution_lineage() {
        assert!(decode_execution_lineage_tip(3, 1, 3, 2, true, false, Some(turn(2))).is_err());
    }

    #[test]
    fn delegated_transcript_origin_retains_exact_spawn_provenance() {
        let current_turn = turn(1);
        let spawning_request = Uuid::from_u128(2);
        let parent_session = Uuid::from_u128(3);
        let parent_turn = Uuid::from_u128(4);
        let content = String::from("delegated task");
        let decoded = decode_transcript_turn_origin(
            String::from("delegation"),
            None,
            None,
            None,
            None,
            None,
            Some(spawning_request),
            Some(parent_session),
            Some(parent_turn),
            Some(content.clone()),
            None,
            None,
            current_turn,
            1,
        )
        .expect("a complete delegated task origin is readable");
        let DecodedTurnOrigin::DelegatedTask {
            spawning_request: decoded_request,
            parent_session: decoded_session,
            parent_turn: decoded_turn,
            content: decoded_content,
        } = decoded
        else {
            panic!("the delegated fixture retains its origin family")
        };
        assert_eq!(decoded_request, ToolRequestId::from_uuid(spawning_request));
        assert_eq!(decoded_session, SessionId::from_uuid(parent_session));
        assert_eq!(decoded_turn, TurnId::from_uuid(parent_turn));
        assert_eq!(decoded_content, content);
    }

    #[test]
    fn delegated_transcript_origin_rejects_missing_spawn_provenance() {
        let error = decode_transcript_turn_origin(
            String::from("delegation"),
            None,
            None,
            None,
            None,
            None,
            None,
            Some(Uuid::from_u128(3)),
            Some(Uuid::from_u128(4)),
            Some(String::from("delegated task")),
            None,
            None,
            turn(1),
            1,
        )
        .expect_err("delegated origin provenance is all-or-nothing");
        assert!(error.to_string().contains("turn origin correlation"));
    }

    #[test]
    fn delegation_wake_origin_retains_exact_delivery_range() {
        let decoded = decode_transcript_turn_origin(
            String::from("delegation"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(Decimal::from(2)),
            Some(Decimal::from(4)),
            turn(1),
            2,
        )
        .expect("a complete delegation wake origin is readable");
        let DecodedTurnOrigin::DelegationWake {
            first_delivery_sequence,
            through_delivery_sequence,
        } = decoded
        else {
            panic!("the wake fixture retains its origin family")
        };
        assert_eq!(first_delivery_sequence, 2);
        assert_eq!(through_delivery_sequence, 4);
    }

    #[test]
    fn delegation_wake_origin_rejects_reversed_delivery_range() {
        let error = decode_transcript_turn_origin(
            String::from("delegation"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(Decimal::from(4)),
            Some(Decimal::from(2)),
            turn(1),
            2,
        )
        .expect_err("a delegation wake range cannot run backward");
        assert!(error.to_string().contains("delegation wake delivery range"));
    }

    #[test]
    fn model_call_usage_provenance_storage_mapping_is_closed() {
        assert_eq!(
            ProcessModelCallUsageProvenance::from_storage("reported"),
            Some(ProcessModelCallUsageProvenance::Reported)
        );
        assert_eq!(
            ProcessModelCallUsageProvenance::from_storage("estimated"),
            Some(ProcessModelCallUsageProvenance::Estimated)
        );
        assert_eq!(
            ProcessModelCallUsageProvenance::from_storage("inferred"),
            None
        );
    }

    #[test]
    fn historical_model_call_input_semantics_remain_unknown() {
        assert_eq!(
            ProcessModelCallInputTokenSemantics::from_storage(None),
            None
        );
        assert_eq!(
            ProcessModelCallInputTokenSemantics::from_storage(Some(false)),
            Some(ProcessModelCallInputTokenSemantics::CacheExclusive)
        );
        assert_eq!(
            ProcessModelCallInputTokenSemantics::from_storage(Some(true)),
            Some(ProcessModelCallInputTokenSemantics::CacheInclusive)
        );
    }

    #[test]
    fn tool_result_disposition_preserves_an_unsupported_spelling() {
        let unsupported = String::from("synthetic_future_disposition");

        assert_eq!(
            decode_tool_result_disposition(&unsupported),
            Err(ProcessReadCorruption::Unsupported {
                field: "terminal_disposition_kind",
                value: unsupported,
            })
        );
    }
}
