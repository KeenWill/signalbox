//! Durable explicit context-compaction command and call lifecycle.

use std::{collections::BTreeMap, error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
use signalbox_domain::{
    ContextCompactionId, ContextCompactionTokenUsage, ContextFrontierId, DirectModelSelection,
    DurableCommandId, ModelCallId, ResolvedProviderTarget, SemanticTranscriptEntryId,
    SemanticTranscriptEntryRef, SessionConfigurationDefaultsVersion, SessionId, TurnId,
};
use sqlx::{PgPool, Row, types::Uuid};

use crate::{
    commit_failure_is_ambiguous,
    mapping::{
        DurableCommandKind, durable_command_kind_from_str, durable_command_kind_to_str,
        session_id_to_uuid,
    },
    model_execution::{SnapshotAppend, SnapshotAppendError, insert_snapshot_append},
    outbox,
};

const COMMAND_KIND: &str = durable_command_kind_to_str(DurableCommandKind::CompactSession);

/// All caller and hub-minted facts for a fresh explicit command attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrepareContextCompactionRequest {
    /// User-global command identity.
    pub command: DurableCommandId,
    /// Session whose complete frontier is summarized.
    pub session: SessionId,
    /// Optional exact one-based complete-frontier position.
    pub requested_through_position: Option<u64>,
    /// Queued turn whose context guard owns this automatic attempt.
    pub automatic_for_turn: Option<TurnId>,
    /// Current defaults epoch observed before entering the transaction.
    pub defaults_version: SessionConfigurationDefaultsVersion,
    /// Current direct model selection after freezing any alias.
    pub selection: DirectModelSelection,
    /// Exact resolved provider target.
    pub target: ResolvedProviderTarget,
    /// Whether this call's provider input total includes both cache axes.
    pub input_includes_cache_tokens: bool,
    /// Non-secret credential reference pinned for the call.
    pub credential_reference: String,
    /// Fresh physical call candidate.
    pub call: ModelCallId,
    /// Fresh compaction candidate.
    pub compaction: ContextCompactionId,
    /// Fresh semantic summary-entry candidate.
    pub summary_entry: SemanticTranscriptEntryId,
    /// Fresh complete result-frontier candidate.
    pub result_frontier: ContextFrontierId,
}

/// Exact durable facts committed before provider preparation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedContextCompaction {
    command: DurableCommandId,
    session: SessionId,
    compaction: ContextCompactionId,
    predecessor: Option<ContextCompactionId>,
    call: ModelCallId,
    selection: DirectModelSelection,
    target: ResolvedProviderTarget,
    credential_reference: String,
    source_frontier: ContextFrontierId,
    first_position: u64,
    through_position: u64,
    first: SemanticTranscriptEntryRef,
    through: SemanticTranscriptEntryRef,
    summarized_entries: Box<[SemanticTranscriptEntryRef]>,
    summarized_positions: Box<[u64]>,
    summary_entry: SemanticTranscriptEntryId,
    result_frontier: ContextFrontierId,
}

/// Read-only model-visible inventory used to choose an automatic boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutomaticContextCompactionPreview {
    source_frontier: ContextFrontierId,
    members: Box<[AutomaticContextCompactionPreviewMember]>,
}

impl AutomaticContextCompactionPreview {
    /// Returns the complete frontier whose projected members were observed.
    pub const fn source_frontier(&self) -> ContextFrontierId {
        self.source_frontier
    }

    /// Returns projected members in model-visible order.
    pub fn members(&self) -> &[AutomaticContextCompactionPreviewMember] {
        &self.members
    }
}

/// One projected entry and whether it closes every preceding tool exchange.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutomaticContextCompactionPreviewMember {
    position: u64,
    reference: SemanticTranscriptEntryRef,
    safe_boundary: bool,
}

impl AutomaticContextCompactionPreviewMember {
    /// Returns the entry's one-based physical frontier position.
    pub const fn position(self) -> u64 {
        self.position
    }

    /// Returns the exact semantic entry reference.
    pub const fn reference(self) -> SemanticTranscriptEntryRef {
        self.reference
    }

    /// Reports whether a summary through this entry closes all tool exchanges.
    pub const fn is_safe_boundary(self) -> bool {
        self.safe_boundary
    }
}

impl PreparedContextCompaction {
    /// Returns the command identity.
    pub const fn command(&self) -> DurableCommandId {
        self.command
    }
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the proposed compaction identity.
    pub const fn compaction(&self) -> ContextCompactionId {
        self.compaction
    }
    /// Returns the preceding compaction, when one exists.
    pub const fn predecessor(&self) -> Option<ContextCompactionId> {
        self.predecessor
    }
    /// Returns the dedicated model-call identity.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }
    /// Returns the direct selection frozen for the call.
    pub const fn selection(&self) -> DirectModelSelection {
        self.selection
    }
    /// Returns the exact resolved target.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.target
    }
    /// Returns the pinned non-secret credential reference.
    pub fn credential_reference(&self) -> &str {
        &self.credential_reference
    }
    /// Returns the complete source-frontier identity.
    pub const fn source_frontier(&self) -> ContextFrontierId {
        self.source_frontier
    }
    /// Returns the one-based first summarized position.
    pub const fn first_position(&self) -> u64 {
        self.first_position
    }
    /// Returns the one-based through position.
    pub const fn through_position(&self) -> u64 {
        self.through_position
    }
    /// Returns the exact first summarized entry.
    pub const fn first(&self) -> SemanticTranscriptEntryRef {
        self.first
    }
    /// Returns the exact final summarized entry.
    pub const fn through(&self) -> SemanticTranscriptEntryRef {
        self.through
    }
    /// Returns the exact model-visible range supplied to the summary call.
    pub fn summarized_entries(&self) -> &[SemanticTranscriptEntryRef] {
        &self.summarized_entries
    }
    /// Returns each summarized entry's one-based physical frontier position.
    pub fn summarized_positions(&self) -> &[u64] {
        &self.summarized_positions
    }
    /// Returns the fresh summary-entry identity.
    pub const fn summary_entry(&self) -> SemanticTranscriptEntryId {
        self.summary_entry
    }
    /// Returns the fresh result-frontier identity.
    pub const fn result_frontier(&self) -> ContextFrontierId {
        self.result_frontier
    }
}

/// Stable receipt for one completed explicit compaction command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppliedContextCompaction {
    /// Compaction identity.
    pub compaction: ContextCompactionId,
    /// Producing model call.
    pub call: ModelCallId,
    /// Exact one-based through position.
    pub through_position: u64,
    /// Summary semantic entry.
    pub summary_entry: SemanticTranscriptEntryId,
    /// Complete result frontier.
    pub result_frontier: ContextFrontierId,
}

/// Result of claiming or replaying an explicit command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareContextCompactionOutcome {
    /// A fresh Prepared call and pending command were committed.
    Prepared(Box<PreparedContextCompaction>),
    /// Equal replay returned its original completed receipt.
    Replayed(AppliedContextCompaction),
    /// The command identity already names another payload or kind.
    ConflictingReuse,
    /// The selected session does not exist.
    SessionNotFound,
    /// The observed defaults epoch ceased to be current.
    DefaultsChanged,
    /// Active turn or another nonterminal compaction owns the session boundary.
    Busy,
    /// No nonempty complete frontier exists yet.
    NoBoundary,
    /// The requested through position is absent, before the visible start, or unsafe.
    InvalidBoundary,
    /// Equal replay names a previously recorded failed command.
    FailedReplay,
    /// This queued turn already owns one durable automatic attempt.
    AutomaticAlreadyAttempted,
}

/// Read-only disposition of one user-global compaction command lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextCompactionCommandLookup {
    /// No durable command has claimed the supplied identity.
    Unseen,
    /// An equal completed command returned its stable receipt.
    Replayed(AppliedContextCompaction),
    /// The identity names another command kind or caller payload.
    ConflictingReuse,
    /// An equal command is still nonterminal.
    Pending,
    /// An equal command already recorded terminal failure.
    Failed,
}

/// Terminal disposition recorded for a failed dedicated call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailedContextCompactionDisposition {
    /// Provider interaction definitively failed.
    KnownFailed,
    /// The provider returned an explicit refusal.
    Refused,
    /// Cancellation was confirmed.
    Cancelled,
    /// Provider acceptance or completion remained uncertain.
    Ambiguous,
}

impl FailedContextCompactionDisposition {
    const fn as_str(self) -> &'static str {
        match self {
            Self::KnownFailed => "known_failed",
            Self::Refused => "refused",
            Self::Cancelled => "cancelled",
            Self::Ambiguous => "ambiguous",
        }
    }
}

/// PostgreSQL-backed explicit compaction command lifecycle.
#[derive(Clone, Debug)]
pub struct ContextCompactionRepository {
    pool: PgPool,
}

impl ContextCompactionRepository {
    /// Uses the supplied shared pool.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Claims an exact command and durably records its call as Prepared.
    pub async fn prepare(
        &self,
        request: PrepareContextCompactionRequest,
    ) -> Result<PrepareContextCompactionOutcome, ContextCompactionRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let outcome = prepare_in_transaction(&mut transaction, &request).await;
        match outcome {
            Ok((true, outcome)) => {
                transaction
                    .commit()
                    .await
                    .map_err(ContextCompactionRepositoryError::commit)?;
                Ok(outcome)
            }
            Ok((false, outcome)) => {
                transaction.rollback().await?;
                Ok(outcome)
            }
            Err(error) => {
                transaction.rollback().await?;
                Err(error)
            }
        }
    }

    /// Looks up replay state before resolving configuration needed only by a
    /// fresh command.
    pub async fn lookup_command(
        &self,
        command: DurableCommandId,
        session: SessionId,
        requested_through_position: Option<u64>,
    ) -> Result<ContextCompactionCommandLookup, ContextCompactionRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        lookup_command_on_connection(
            &mut connection,
            command,
            session,
            requested_through_position,
            None,
        )
        .await
    }

    /// Reads the current projected frontier without claiming a compaction.
    ///
    /// Automatic callers render this immutable inventory before choosing the
    /// exact boundary they later commit. The prepare transaction reselects the
    /// frontier and validates that boundary before any provider interaction.
    pub async fn preview_automatic_range(
        &self,
        session: SessionId,
    ) -> Result<Option<AutomaticContextCompactionPreview>, ContextCompactionRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let source = load_compaction_source(&mut transaction, session).await?;
        let preview = match source {
            Some(source) if source.member_count > 0 => {
                let visible =
                    load_projected_frontier_members(&mut transaction, session, source.frontier)
                        .await?;
                let members = preview_members(&visible)?;
                Some(AutomaticContextCompactionPreview {
                    source_frontier: source.frontier,
                    members: members.into_boxed_slice(),
                })
            }
            Some(_) | None => None,
        };
        transaction.rollback().await?;
        Ok(preview)
    }

    /// Commits InFlight before any provider interaction begins.
    ///
    /// An exact InFlight replay proves that an earlier ambiguous commit landed,
    /// so the caller may continue to provider interaction without abandoning an
    /// authorized but unsent call.
    pub async fn authorize(
        &self,
        prepared: &PreparedContextCompaction,
    ) -> Result<(), ContextCompactionRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_lifecycle_session(&mut transaction, prepared.session).await?;
        let state: String = sqlx::query_scalar(
            "SELECT state_kind
               FROM context_compaction_model_call
              WHERE model_call_id = $1
                AND session_id = $2",
        )
        .bind(prepared.call.into_uuid())
        .bind(session_id_to_uuid(prepared.session))
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(ContextCompactionCorruption::Missing(
            "compaction model call",
        ))?;
        if state == "in_flight" {
            transaction.rollback().await?;
            return Ok(());
        }
        if state != "prepared" {
            transaction.rollback().await?;
            return Err(ContextCompactionCorruption::Inconsistent(
                "compaction call authorization state",
            )
            .into());
        }
        let rows = sqlx::query(
            "UPDATE context_compaction_model_call
                SET state_kind = 'in_flight',
                    in_flight_at = statement_timestamp()
              WHERE model_call_id = $1
                AND session_id = $2
                AND state_kind = 'prepared'",
        )
        .bind(prepared.call.into_uuid())
        .bind(session_id_to_uuid(prepared.session))
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        require_single(rows, "compaction call authorization")?;
        transaction
            .commit()
            .await
            .map_err(ContextCompactionRepositoryError::commit)
    }

    /// Atomically records successful call evidence, summary, result frontier,
    /// compaction provenance, and replay receipt.
    ///
    /// Repeating the exact completion after an ambiguous commit validates and
    /// returns its first durable result without appending duplicate evidence.
    pub async fn complete(
        &self,
        prepared: &PreparedContextCompaction,
        summary: &str,
        usage: ContextCompactionTokenUsage,
    ) -> Result<AppliedContextCompaction, ContextCompactionRepositoryError> {
        if summary.is_empty() || summary.contains('\0') {
            return Err(ContextCompactionCorruption::InvalidSummary.into());
        }
        let mut transaction = self.pool.begin().await?;
        lock_lifecycle_session(&mut transaction, prepared.session).await?;
        let lifecycle = load_lifecycle(&mut transaction, prepared).await?;
        if lifecycle.call_state == "terminal" {
            let exact = lifecycle.call_disposition.as_deref() == Some("completed")
                && lifecycle.command_result == "applied"
                && lifecycle.input_tokens == usage.input_tokens().map(Decimal::from)
                && lifecycle.output_tokens == usage.output_tokens().map(Decimal::from)
                && lifecycle.cache_creation_input_tokens
                    == usage.cache_creation_input_tokens().map(Decimal::from)
                && lifecycle.cache_read_input_tokens
                    == usage.cache_read_input_tokens().map(Decimal::from)
                && lifecycle.result_compaction == Some(prepared.compaction.into_uuid())
                && lifecycle.result_through_position
                    == Some(Decimal::from(prepared.through_position))
                && lifecycle.result_summary_entry == Some(prepared.summary_entry.into_uuid())
                && lifecycle.result_frontier == Some(prepared.result_frontier.into_uuid())
                && exact_completed_evidence(&mut transaction, prepared, summary).await?;
            transaction.rollback().await?;
            if exact {
                return Ok(prepared.applied());
            }
            return Err(
                ContextCompactionCorruption::Inconsistent("completed compaction replay").into(),
            );
        }
        if lifecycle.call_state != "in_flight" || lifecycle.command_result != "pending" {
            transaction.rollback().await?;
            return Err(
                ContextCompactionCorruption::Inconsistent("compaction completion state").into(),
            );
        }
        let source_count: Decimal = sqlx::query_scalar(
            "SELECT member_count
               FROM context_frontier
              WHERE owning_session_id = $1
                AND context_frontier_id = $2
              FOR SHARE",
        )
        .bind(session_id_to_uuid(prepared.session))
        .bind(prepared.source_frontier.into_uuid())
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(ContextCompactionCorruption::Missing("source frontier"))?;
        let source_count = decode_u64(source_count, "source frontier member count")?;
        let result_count =
            source_count
                .checked_add(1)
                .ok_or(ContextCompactionCorruption::InvalidOrdinal(
                    "result frontier member count",
                ))?;
        let call_rows = sqlx::query(
            "UPDATE context_compaction_model_call
                SET state_kind = 'terminal',
                    terminal_at = statement_timestamp(),
                    terminal_disposition_kind = 'completed',
                    input_tokens = $1,
                    output_tokens = $2,
                    cache_creation_input_tokens = $3,
                    cache_read_input_tokens = $4
              WHERE model_call_id = $5
                AND session_id = $6
                AND state_kind = 'in_flight'",
        )
        .bind(usage.input_tokens().map(Decimal::from))
        .bind(usage.output_tokens().map(Decimal::from))
        .bind(usage.cache_creation_input_tokens().map(Decimal::from))
        .bind(usage.cache_read_input_tokens().map(Decimal::from))
        .bind(prepared.call.into_uuid())
        .bind(session_id_to_uuid(prepared.session))
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        require_single(call_rows, "completed compaction call")?;
        sqlx::query(
            "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 context_summary_value, context_summary_producing_call_id,
                 context_summary_first_source_session_id,
                 context_summary_first_entry_id,
                 context_summary_through_source_session_id,
                 context_summary_through_entry_id)
             VALUES ($1, $2, 'context_summary', $3, $4, $5, $6, $7, $8)",
        )
        .bind(session_id_to_uuid(prepared.session))
        .bind(prepared.summary_entry.into_uuid())
        .bind(summary)
        .bind(prepared.call.into_uuid())
        .bind(session_id_to_uuid(prepared.first.source_session()))
        .bind(prepared.first.entry().into_uuid())
        .bind(session_id_to_uuid(prepared.through.source_session()))
        .bind(prepared.through.entry().into_uuid())
        .execute(&mut *transaction)
        .await
        .map_err(classify_completion_write)?;
        insert_snapshot_append(
            &mut transaction,
            SnapshotAppend {
                owning_session: prepared.session,
                frontier: prepared.result_frontier,
                prefix: Some(prepared.source_frontier),
                member_count: result_count,
                prefix_member_count: source_count,
                appended_entries: [SemanticTranscriptEntryRef::from_source(
                    prepared.session,
                    prepared.summary_entry,
                )],
            },
        )
        .await
        .map_err(|error| match error {
            SnapshotAppendError::FrontierInsert(error)
            | SnapshotAppendError::MemberInsert(error) => classify_completion_write(error),
            SnapshotAppendError::MemberPositionOverflow => {
                ContextCompactionCorruption::InvalidOrdinal("result frontier member position")
                    .into()
            }
        })?;
        sqlx::query(
            "INSERT INTO context_compaction
                (context_compaction_id, session_id, predecessor_compaction_id,
                 source_frontier_id, result_frontier_id, producing_call_id,
                 first_source_session_id, first_entry_id,
                 through_source_session_id, through_entry_id, summary_entry_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(prepared.compaction.into_uuid())
        .bind(session_id_to_uuid(prepared.session))
        .bind(prepared.predecessor.map(ContextCompactionId::into_uuid))
        .bind(prepared.source_frontier.into_uuid())
        .bind(prepared.result_frontier.into_uuid())
        .bind(prepared.call.into_uuid())
        .bind(session_id_to_uuid(prepared.first.source_session()))
        .bind(prepared.first.entry().into_uuid())
        .bind(session_id_to_uuid(prepared.through.source_session()))
        .bind(prepared.through.entry().into_uuid())
        .bind(prepared.summary_entry.into_uuid())
        .execute(&mut *transaction)
        .await
        .map_err(classify_completion_write)?;
        let command_rows = sqlx::query(
            "UPDATE compact_session_command
                SET result_kind = 'applied',
                    result_context_compaction_id = $1,
                    result_through_position = $2,
                    result_summary_entry_id = $3,
                    result_frontier_id = $4
              WHERE command_id = $5
                AND session_id = $6
                AND result_kind = 'pending'",
        )
        .bind(prepared.compaction.into_uuid())
        .bind(Decimal::from(prepared.through_position))
        .bind(prepared.summary_entry.into_uuid())
        .bind(prepared.result_frontier.into_uuid())
        .bind(prepared.command.into_uuid())
        .bind(session_id_to_uuid(prepared.session))
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        require_single(command_rows, "applied compaction command")?;
        outbox::append(
            &mut transaction,
            outbox::OutboxEvent::ContextCompacted {
                session: prepared.session,
                compaction: prepared.compaction,
                call: prepared.call,
                through_position: prepared.through_position,
                summary_entry: prepared.summary_entry,
                result_frontier: prepared.result_frontier,
            },
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(ContextCompactionRepositoryError::commit)?;
        Ok(prepared.applied())
    }

    /// Records one failed or uncertain dedicated call and failed command.
    ///
    /// Repeating the exact terminal disposition after an ambiguous commit is a
    /// successful replay; a different terminal fact fails closed.
    pub async fn fail(
        &self,
        prepared: &PreparedContextCompaction,
        disposition: FailedContextCompactionDisposition,
    ) -> Result<(), ContextCompactionRepositoryError> {
        self.fail_record(
            prepared,
            disposition,
            ContextCompactionTokenUsage::unreported(),
        )
        .await
    }

    /// Records a failed call together with provider-reported terminal usage.
    pub async fn fail_with_usage(
        &self,
        prepared: &PreparedContextCompaction,
        disposition: FailedContextCompactionDisposition,
        usage: ContextCompactionTokenUsage,
    ) -> Result<(), ContextCompactionRepositoryError> {
        self.fail_record(prepared, disposition, usage).await
    }

    async fn fail_record(
        &self,
        prepared: &PreparedContextCompaction,
        disposition: FailedContextCompactionDisposition,
        usage: ContextCompactionTokenUsage,
    ) -> Result<(), ContextCompactionRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_lifecycle_session(&mut transaction, prepared.session).await?;
        let lifecycle = load_lifecycle(&mut transaction, prepared).await?;
        if lifecycle.call_state == "terminal" {
            let exact = lifecycle.call_disposition.as_deref() == Some(disposition.as_str())
                && lifecycle.command_result == "failed"
                && lifecycle.input_tokens == usage.input_tokens().map(Decimal::from)
                && lifecycle.output_tokens == usage.output_tokens().map(Decimal::from)
                && lifecycle.cache_creation_input_tokens
                    == usage.cache_creation_input_tokens().map(Decimal::from)
                && lifecycle.cache_read_input_tokens
                    == usage.cache_read_input_tokens().map(Decimal::from);
            transaction.rollback().await?;
            if exact {
                return Ok(());
            }
            return Err(
                ContextCompactionCorruption::Inconsistent("failed compaction replay").into(),
            );
        }
        if !matches!(lifecycle.call_state.as_str(), "prepared" | "in_flight")
            || lifecycle.command_result != "pending"
        {
            transaction.rollback().await?;
            return Err(
                ContextCompactionCorruption::Inconsistent("compaction failure state").into(),
            );
        }
        if usage != ContextCompactionTokenUsage::unreported() && lifecycle.call_state != "in_flight"
        {
            transaction.rollback().await?;
            return Err(ContextCompactionCorruption::Inconsistent(
                "compaction failure usage before authorization",
            )
            .into());
        }
        let call_rows = sqlx::query(
            "UPDATE context_compaction_model_call
                SET state_kind = 'terminal', terminal_at = statement_timestamp(),
                    terminal_disposition_kind = $1,
                    input_tokens = $2, output_tokens = $3,
                    cache_creation_input_tokens = $4,
                    cache_read_input_tokens = $5
              WHERE model_call_id = $6
                AND session_id = $7
                AND state_kind IN ('prepared', 'in_flight')",
        )
        .bind(disposition.as_str())
        .bind(usage.input_tokens().map(Decimal::from))
        .bind(usage.output_tokens().map(Decimal::from))
        .bind(usage.cache_creation_input_tokens().map(Decimal::from))
        .bind(usage.cache_read_input_tokens().map(Decimal::from))
        .bind(prepared.call.into_uuid())
        .bind(session_id_to_uuid(prepared.session))
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        require_single(call_rows, "failed compaction call")?;
        let command_rows = sqlx::query(
            "UPDATE compact_session_command
                SET result_kind = 'failed'
              WHERE command_id = $1
                AND session_id = $2
                AND result_kind = 'pending'",
        )
        .bind(prepared.command.into_uuid())
        .bind(session_id_to_uuid(prepared.session))
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        require_single(command_rows, "failed compaction command")?;
        transaction
            .commit()
            .await
            .map_err(ContextCompactionRepositoryError::commit)
    }
}

#[derive(Debug)]
struct StoredContextCompactionLifecycle {
    call_state: String,
    call_disposition: Option<String>,
    input_tokens: Option<Decimal>,
    output_tokens: Option<Decimal>,
    cache_creation_input_tokens: Option<Decimal>,
    cache_read_input_tokens: Option<Decimal>,
    command_result: String,
    result_compaction: Option<Uuid>,
    result_through_position: Option<Decimal>,
    result_summary_entry: Option<Uuid>,
    result_frontier: Option<Uuid>,
}

async fn lock_lifecycle_session(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: SessionId,
) -> Result<(), ContextCompactionRepositoryError> {
    let exists =
        sqlx::query_scalar::<_, Uuid>(crate::lock_inventory::CONTEXT_COMPACTION_LIFECYCLE_SESSION)
            .bind(session_id_to_uuid(session))
            .fetch_optional(&mut **transaction)
            .await?
            .is_some();
    if exists {
        Ok(())
    } else {
        Err(ContextCompactionCorruption::Missing("compaction session").into())
    }
}

async fn load_lifecycle(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    prepared: &PreparedContextCompaction,
) -> Result<StoredContextCompactionLifecycle, ContextCompactionRepositoryError> {
    let row = sqlx::query(
        "SELECT call.state_kind, call.terminal_disposition_kind,
                call.input_tokens, call.output_tokens,
                call.cache_creation_input_tokens, call.cache_read_input_tokens,
                command.result_kind, command.result_context_compaction_id,
                command.result_through_position, command.result_summary_entry_id,
                command.result_frontier_id
           FROM context_compaction_model_call AS call
           JOIN compact_session_command AS command
             ON command.model_call_id = call.model_call_id
            AND command.session_id = call.session_id
          WHERE call.model_call_id = $1
            AND call.session_id = $2
            AND command.command_id = $3",
    )
    .bind(prepared.call.into_uuid())
    .bind(session_id_to_uuid(prepared.session))
    .bind(prepared.command.into_uuid())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(ContextCompactionCorruption::Missing("compaction lifecycle"))?;
    Ok(StoredContextCompactionLifecycle {
        call_state: row.try_get("state_kind")?,
        call_disposition: row.try_get("terminal_disposition_kind")?,
        input_tokens: row.try_get("input_tokens")?,
        output_tokens: row.try_get("output_tokens")?,
        cache_creation_input_tokens: row.try_get("cache_creation_input_tokens")?,
        cache_read_input_tokens: row.try_get("cache_read_input_tokens")?,
        command_result: row.try_get("result_kind")?,
        result_compaction: row.try_get("result_context_compaction_id")?,
        result_through_position: row.try_get("result_through_position")?,
        result_summary_entry: row.try_get("result_summary_entry_id")?,
        result_frontier: row.try_get("result_frontier_id")?,
    })
}

async fn exact_completed_evidence(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    prepared: &PreparedContextCompaction,
    summary: &str,
) -> Result<bool, ContextCompactionRepositoryError> {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM semantic_transcript_entry AS entry
               JOIN context_compaction AS compaction
                 ON compaction.session_id = entry.source_session_id
                AND compaction.summary_entry_id = entry.semantic_entry_id
              WHERE entry.source_session_id = $1
                AND entry.semantic_entry_id = $2
                AND entry.payload_kind = 'context_summary'
                AND entry.context_summary_value = $3
                AND entry.context_summary_producing_call_id = $4
                AND entry.context_summary_first_source_session_id = $5
                AND entry.context_summary_first_entry_id = $6
                AND entry.context_summary_through_source_session_id = $7
                AND entry.context_summary_through_entry_id = $8
                AND compaction.context_compaction_id = $9
                AND compaction.predecessor_compaction_id IS NOT DISTINCT FROM $10
                AND compaction.source_frontier_id = $11
                AND compaction.result_frontier_id = $12
                AND compaction.producing_call_id = $4
                AND compaction.first_source_session_id = $5
                AND compaction.first_entry_id = $6
                AND compaction.through_source_session_id = $7
                AND compaction.through_entry_id = $8
         )",
    )
    .bind(session_id_to_uuid(prepared.session))
    .bind(prepared.summary_entry.into_uuid())
    .bind(summary)
    .bind(prepared.call.into_uuid())
    .bind(session_id_to_uuid(prepared.first.source_session()))
    .bind(prepared.first.entry().into_uuid())
    .bind(session_id_to_uuid(prepared.through.source_session()))
    .bind(prepared.through.entry().into_uuid())
    .bind(prepared.compaction.into_uuid())
    .bind(prepared.predecessor.map(ContextCompactionId::into_uuid))
    .bind(prepared.source_frontier.into_uuid())
    .bind(prepared.result_frontier.into_uuid())
    .fetch_one(&mut **transaction)
    .await
    .map_err(Into::into)
}

impl PreparedContextCompaction {
    const fn applied(&self) -> AppliedContextCompaction {
        AppliedContextCompaction {
            compaction: self.compaction,
            call: self.call,
            through_position: self.through_position,
            summary_entry: self.summary_entry,
            result_frontier: self.result_frontier,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompactionSource {
    frontier: ContextFrontierId,
    member_count: u64,
}

async fn load_compaction_source(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: SessionId,
) -> Result<Option<CompactionSource>, ContextCompactionRepositoryError> {
    let row = sqlx::query(
        "WITH candidate (frontier_id) AS (
            SELECT turn_lifecycle_effective_terminal_frontier(session_id, turn_id)
              FROM turn_lifecycle
             WHERE session_id = $1
               AND (state_kind = 'terminal' OR delegation_runtime_terminal)
            UNION ALL
            SELECT seed_context_frontier_id
              FROM imported_session_seed
             WHERE session_id = $1
            UNION ALL
            SELECT result_frontier_id
              FROM context_compaction
             WHERE session_id = $1
         )
         SELECT frontier.context_frontier_id, frontier.member_count
           FROM candidate
           JOIN context_frontier AS frontier
             ON frontier.owning_session_id = $1
            AND frontier.context_frontier_id = candidate.frontier_id
          ORDER BY frontier.member_count DESC
          LIMIT 1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut **transaction)
    .await?;
    row.map(|row| {
        Ok(CompactionSource {
            frontier: ContextFrontierId::from_uuid(row.try_get("context_frontier_id")?),
            member_count: decode_u64(row.try_get("member_count")?, "source member count")?,
        })
    })
    .transpose()
}

async fn prepare_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request: &PrepareContextCompactionRequest,
) -> Result<(bool, PrepareContextCompactionOutcome), ContextCompactionRepositoryError> {
    if request.automatic_for_turn.is_some() && request.requested_through_position.is_none() {
        return Ok((false, PrepareContextCompactionOutcome::InvalidBoundary));
    }
    match lookup_command_on_connection(
        transaction,
        request.command,
        request.session,
        request.requested_through_position,
        request.automatic_for_turn,
    )
    .await?
    {
        ContextCompactionCommandLookup::Unseen => {}
        ContextCompactionCommandLookup::Replayed(applied) => {
            return Ok((false, PrepareContextCompactionOutcome::Replayed(applied)));
        }
        ContextCompactionCommandLookup::ConflictingReuse => {
            return Ok((false, PrepareContextCompactionOutcome::ConflictingReuse));
        }
        ContextCompactionCommandLookup::Pending => {
            return Ok((false, PrepareContextCompactionOutcome::Busy));
        }
        ContextCompactionCommandLookup::Failed => {
            return Ok((false, PrepareContextCompactionOutcome::FailedReplay));
        }
    }
    // The arbiter is left unnamed on purpose. `durable_command` carries two
    // unique indexes over the claimed identity — the `command_id` primary key
    // and `durable_command_kind_version_key` over
    // `(command_id, command_kind, storage_version)` — and `DO NOTHING`
    // suppresses a violation only of the index it arbitrates. Naming
    // `(command_id)` left the second index unguarded, so a concurrent claimant
    // whose speculative insertion reached that index before the primary key
    // saw the winner raised a raw uniqueness violation instead of losing the
    // claim. An unnamed arbiter covers every unique index on the row, which is
    // what every other durable command claim in this crate already does.
    let issuer = crate::command_registry::issuer_columns(if request.automatic_for_turn.is_some() {
        signalbox_domain::CommandPrincipal::Core
    } else {
        signalbox_domain::CommandPrincipal::Operator
    });
    let claimed = sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at,
             issuer_kind, issuer_module)
         VALUES ($1, $2, 1, clock_timestamp(), $3, $4)
         ON CONFLICT DO NOTHING",
    )
    .bind(request.command.into_uuid())
    .bind(COMMAND_KIND)
    .bind(issuer.0)
    .bind(issuer.1)
    .execute(&mut **transaction)
    .await?
    .rows_affected();
    if claimed == 0 {
        let winner = lookup_command_on_connection(
            transaction,
            request.command,
            request.session,
            request.requested_through_position,
            request.automatic_for_turn,
        )
        .await?;
        let outcome = match winner {
            ContextCompactionCommandLookup::Replayed(applied) => {
                PrepareContextCompactionOutcome::Replayed(applied)
            }
            ContextCompactionCommandLookup::ConflictingReuse => {
                PrepareContextCompactionOutcome::ConflictingReuse
            }
            ContextCompactionCommandLookup::Pending => PrepareContextCompactionOutcome::Busy,
            ContextCompactionCommandLookup::Failed => PrepareContextCompactionOutcome::FailedReplay,
            ContextCompactionCommandLookup::Unseen => {
                return Err(ContextCompactionCorruption::Inconsistent(
                    "compaction command claim winner",
                )
                .into());
            }
        };
        return Ok((false, outcome));
    }
    // The three result identities are remintable only here. `complete` writes
    // them under global uniqueness after the provider has produced the summary,
    // and by then the in-flight lifecycle pins them, so a collision discovered
    // there costs a paid call and can only fail closed. Deciding it now routes
    // it through the same remint path the call identity already takes. This
    // reads rather than reserves — nothing durable can name an unproduced
    // summary — so `complete` still fails closed on a later collision.
    let result_identity_taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM semantic_transcript_entry
              WHERE semantic_entry_id = $1
         ) OR EXISTS (
             SELECT 1
               FROM context_frontier
              WHERE context_frontier_id = $2
         ) OR EXISTS (
             SELECT 1
               FROM context_compaction
              WHERE context_compaction_id = $3
         )",
    )
    .bind(request.summary_entry.into_uuid())
    .bind(request.result_frontier.into_uuid())
    .bind(request.compaction.into_uuid())
    .fetch_one(&mut **transaction)
    .await?;
    if result_identity_taken {
        return Err(ContextCompactionRepositoryError::IdentityCollision);
    }
    // Lock inventory: the session scheduler follows the user-global command
    // claim, then the current-defaults pointer. Holding the scheduler while
    // selecting and recording the boundary makes compaction preparation and
    // turn activation mutually exclusive.
    let session_uuid = session_id_to_uuid(request.session);
    let (session_exists, scheduler_session) = sqlx::query_as::<_, (bool, Option<Uuid>)>(
        crate::lock_inventory::CONTEXT_COMPACTION_SCHEDULER,
    )
    .bind(session_uuid)
    .fetch_one(&mut **transaction)
    .await?;
    if !session_exists {
        return Ok((false, PrepareContextCompactionOutcome::SessionNotFound));
    }
    if scheduler_session.is_none() {
        return Err(ContextCompactionCorruption::Missing("session scheduler row").into());
    }
    let current_version: Decimal =
        sqlx::query_scalar(crate::lock_inventory::CONTEXT_COMPACTION_DEFAULTS)
            .bind(session_uuid)
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(ContextCompactionCorruption::Missing(
                "session current defaults",
            ))?;
    if decode_u64(current_version, "current defaults version")? != request.defaults_version.as_u64()
    {
        return Ok((false, PrepareContextCompactionOutcome::DefaultsChanged));
    }
    if let Some(turn) = request.automatic_for_turn {
        let already_attempted: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1
                   FROM compact_session_command
                  WHERE session_id = $1
                    AND automatic_for_turn_id = $2
             )",
        )
        .bind(session_id_to_uuid(request.session))
        .bind(turn.into_uuid())
        .fetch_one(&mut **transaction)
        .await?;
        if already_attempted {
            return Ok((
                false,
                PrepareContextCompactionOutcome::AutomaticAlreadyAttempted,
            ));
        }
    }
    // A cascade-terminalized delegated child keeps its physical `active` row and
    // carries the logical terminal instead, so only runtime-relevant active
    // turns own the boundary. The retained row's effective terminal frontier is
    // the logical terminal's frontier, which the source selection below reads
    // through `turn_lifecycle_effective_terminal_frontier`.
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM turn_lifecycle
              WHERE session_id = $1
                AND state_kind = 'active'
                AND NOT delegation_runtime_terminal
         ) OR EXISTS (
             SELECT 1 FROM context_compaction_model_call
              WHERE session_id = $1 AND state_kind <> 'terminal'
         )",
    )
    .bind(session_id_to_uuid(request.session))
    .fetch_one(&mut **transaction)
    .await?;
    if busy {
        return Ok((false, PrepareContextCompactionOutcome::Busy));
    }
    let source = load_compaction_source(transaction, request.session).await?;
    let Some(source) = source else {
        return Ok((false, PrepareContextCompactionOutcome::NoBoundary));
    };
    let source_frontier = source.frontier;
    if source.member_count == 0 {
        return Ok((false, PrepareContextCompactionOutcome::NoBoundary));
    }
    let predecessor = sqlx::query(
        "SELECT candidate.context_compaction_id
           FROM context_compaction AS candidate
          WHERE candidate.session_id = $1
            AND NOT EXISTS (
                SELECT 1 FROM context_compaction AS successor
                 WHERE successor.session_id = candidate.session_id
                   AND successor.predecessor_compaction_id = candidate.context_compaction_id
            )",
    )
    .bind(session_id_to_uuid(request.session))
    .fetch_optional(&mut **transaction)
    .await?
    .map(|row| {
        row.try_get("context_compaction_id")
            .map(ContextCompactionId::from_uuid)
    })
    .transpose()?;
    let visible =
        load_projected_frontier_members(transaction, request.session, source_frontier).await?;
    let Some(first) = visible.first() else {
        return Ok((false, PrepareContextCompactionOutcome::NoBoundary));
    };
    let through_index = match request.requested_through_position {
        Some(position) => visible
            .iter()
            .position(|member| member.position == position),
        None => latest_safe_boundary(&visible),
    };
    let Some(through_index) = through_index else {
        return Ok((false, PrepareContextCompactionOutcome::InvalidBoundary));
    };
    if !range_closes_tool_exchanges(&visible[..=through_index]) {
        return Ok((false, PrepareContextCompactionOutcome::InvalidBoundary));
    }
    let first_position = first.position;
    let first = first.reference;
    let through_position = visible[through_index].position;
    let through = visible[through_index].reference;
    let summarized_entries = visible[..=through_index]
        .iter()
        .map(|member| member.reference)
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let summarized_positions = visible[..=through_index]
        .iter()
        .map(|member| member.position)
        .collect::<Vec<_>>()
        .into_boxed_slice();
    sqlx::query(
        "INSERT INTO compact_session_command
            (command_id, command_kind, storage_version, session_id,
             requested_through_position, automatic_for_turn_id,
             model_call_id, result_kind)
         VALUES ($1, $2, 1, $3, $4, $5, $6, 'pending')",
    )
    .bind(request.command.into_uuid())
    .bind(COMMAND_KIND)
    .bind(session_id_to_uuid(request.session))
    .bind(request.requested_through_position.map(Decimal::from))
    .bind(request.automatic_for_turn.map(TurnId::into_uuid))
    .bind(request.call.into_uuid())
    .execute(&mut **transaction)
    .await?;
    let insert_call = sqlx::query(
        "INSERT INTO context_compaction_model_call
            (model_call_id, session_id, direct_model_selection_id,
             resolved_provider_model_identity_id, source_frontier_id,
             credential_reference, usage_input_includes_cache_tokens, state_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'prepared')",
    )
    .bind(request.call.into_uuid())
    .bind(session_id_to_uuid(request.session))
    .bind(request.selection.into_uuid())
    .bind(request.target.identity().into_uuid())
    .bind(source_frontier.into_uuid())
    .bind(&request.credential_reference)
    .bind(request.input_includes_cache_tokens)
    .execute(&mut **transaction)
    .await;
    if let Err(error) = insert_call {
        if error
            .as_database_error()
            .and_then(|database| database.constraint())
            == Some("model_call_identity_pkey")
        {
            return Err(ContextCompactionRepositoryError::IdentityCollision);
        }
        return Err(error.into());
    }
    Ok((
        true,
        PrepareContextCompactionOutcome::Prepared(Box::new(PreparedContextCompaction {
            command: request.command,
            session: request.session,
            compaction: request.compaction,
            predecessor,
            call: request.call,
            selection: request.selection,
            target: request.target,
            credential_reference: request.credential_reference.clone(),
            source_frontier,
            first_position,
            through_position,
            first,
            through,
            summarized_entries,
            summarized_positions,
            summary_entry: request.summary_entry,
            result_frontier: request.result_frontier,
        })),
    ))
}

async fn lookup_command_on_connection(
    connection: &mut sqlx::PgConnection,
    command: DurableCommandId,
    session: SessionId,
    requested_through_position: Option<u64>,
    automatic_for_turn: Option<TurnId>,
) -> Result<ContextCompactionCommandLookup, ContextCompactionRepositoryError> {
    let existing_kind: Option<String> =
        sqlx::query_scalar("SELECT command_kind FROM durable_command WHERE command_id = $1")
            .bind(command.into_uuid())
            .fetch_optional(&mut *connection)
            .await?;
    let Some(kind) = existing_kind else {
        return Ok(ContextCompactionCommandLookup::Unseen);
    };
    let Some(kind) = durable_command_kind_from_str(&kind) else {
        return Err(ContextCompactionCorruption::UnsupportedCommandKind(kind).into());
    };
    if kind != DurableCommandKind::CompactSession {
        return Ok(ContextCompactionCommandLookup::ConflictingReuse);
    }
    let row = sqlx::query(
        "SELECT session_id, requested_through_position,
                automatic_for_turn_id, result_kind,
                result_context_compaction_id, model_call_id,
                result_through_position, result_summary_entry_id, result_frontier_id
           FROM compact_session_command
          WHERE command_id = $1",
    )
    .bind(command.into_uuid())
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ContextCompactionCorruption::Missing(
        "compaction command detail",
    ))?;
    let stored_session: Uuid = row.try_get("session_id")?;
    let requested: Option<Decimal> = row.try_get("requested_through_position")?;
    let stored_automatic_turn: Option<Uuid> = row.try_get("automatic_for_turn_id")?;
    if stored_session != session_id_to_uuid(session)
        || requested
            .map(|value| decode_u64(value, "requested through position"))
            .transpose()?
            != requested_through_position
        || stored_automatic_turn.map(TurnId::from_uuid) != automatic_for_turn
    {
        return Ok(ContextCompactionCommandLookup::ConflictingReuse);
    }
    let kind: String = row.try_get("result_kind")?;
    if kind == "pending" {
        return Ok(ContextCompactionCommandLookup::Pending);
    }
    if kind == "failed" {
        return Ok(ContextCompactionCommandLookup::Failed);
    }
    if kind != "applied" {
        return Err(ContextCompactionCorruption::UnsupportedResult(kind).into());
    }
    Ok(ContextCompactionCommandLookup::Replayed(
        AppliedContextCompaction {
            compaction: ContextCompactionId::from_uuid(required(
                &row,
                "result_context_compaction_id",
            )?),
            call: ModelCallId::from_uuid(required(&row, "model_call_id")?),
            through_position: decode_u64(
                required(&row, "result_through_position")?,
                "result through position",
            )?,
            summary_entry: SemanticTranscriptEntryId::from_uuid(required(
                &row,
                "result_summary_entry_id",
            )?),
            result_frontier: ContextFrontierId::from_uuid(required(&row, "result_frontier_id")?),
        },
    ))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectedFrontierMember {
    position: u64,
    reference: SemanticTranscriptEntryRef,
    payload_kind: String,
    summary_range: Option<(SemanticTranscriptEntryRef, SemanticTranscriptEntryRef)>,
}

/// Names one committed frontier's model-visible entries in projected order.
///
/// Entries a compaction summarized away are absent: they are no longer part of
/// any request built on this frontier. Callers scoring prospective content read
/// membership here rather than from physical frontier positions, which a
/// summary appended after its own boundary reorders.
pub(crate) async fn projected_frontier_membership(
    connection: &mut sqlx::PgConnection,
    session: SessionId,
    frontier: ContextFrontierId,
) -> Result<Vec<SemanticTranscriptEntryRef>, ContextCompactionRepositoryError> {
    Ok(
        load_projected_frontier_members(connection, session, frontier)
            .await?
            .into_iter()
            .map(|member| member.reference)
            .collect(),
    )
}

async fn load_projected_frontier_members(
    connection: &mut sqlx::PgConnection,
    session: SessionId,
    frontier: ContextFrontierId,
) -> Result<Vec<ProjectedFrontierMember>, ContextCompactionRepositoryError> {
    let rows = sqlx::query(
        "SELECT member.member_position, member.source_session_id,
                member.semantic_entry_id, entry.payload_kind,
                entry.context_summary_first_source_session_id,
                entry.context_summary_first_entry_id,
                entry.context_summary_through_source_session_id,
                entry.context_summary_through_entry_id
           FROM context_frontier_member AS member
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
          WHERE member.owning_session_id = $1
            AND member.context_frontier_id = $2
          ORDER BY member.member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    let mut complete = Vec::with_capacity(rows.len());
    for row in rows {
        let payload_kind: String = row.try_get("payload_kind")?;
        let summary_values = (
            row.try_get::<Option<Uuid>, _>("context_summary_first_source_session_id")?,
            row.try_get::<Option<Uuid>, _>("context_summary_first_entry_id")?,
            row.try_get::<Option<Uuid>, _>("context_summary_through_source_session_id")?,
            row.try_get::<Option<Uuid>, _>("context_summary_through_entry_id")?,
        );
        let summary_range = match summary_values {
            (
                Some(first_session),
                Some(first_entry),
                Some(through_session),
                Some(through_entry),
            ) if payload_kind == "context_summary" => Some((
                SemanticTranscriptEntryRef::from_source(
                    SessionId::from_uuid(first_session),
                    SemanticTranscriptEntryId::from_uuid(first_entry),
                ),
                SemanticTranscriptEntryRef::from_source(
                    SessionId::from_uuid(through_session),
                    SemanticTranscriptEntryId::from_uuid(through_entry),
                ),
            )),
            (None, None, None, None) if payload_kind != "context_summary" => None,
            _ => {
                return Err(ContextCompactionCorruption::Inconsistent(
                    "context summary payload shape",
                )
                .into());
            }
        };
        complete.push(ProjectedFrontierMember {
            position: decode_u64(row.try_get("member_position")?, "frontier member position")?,
            reference: SemanticTranscriptEntryRef::from_source(
                SessionId::from_uuid(row.try_get("source_session_id")?),
                SemanticTranscriptEntryId::from_uuid(row.try_get("semantic_entry_id")?),
            ),
            payload_kind,
            summary_range,
        });
    }
    project_frontier_members(complete)
}

fn project_frontier_members(
    complete: Vec<ProjectedFrontierMember>,
) -> Result<Vec<ProjectedFrontierMember>, ContextCompactionRepositoryError> {
    let physical_positions = complete
        .iter()
        .map(|member| (member.reference, member.position))
        .collect::<BTreeMap<_, _>>();
    let mut visible = complete.clone();
    for summary in &complete {
        let Some((first, through)) = summary.summary_range else {
            continue;
        };
        let first_index = visible
            .iter()
            .position(|member| member.reference == first)
            .ok_or(ContextCompactionCorruption::Inconsistent(
                "context summary first endpoint",
            ))?;
        let through_index = visible
            .iter()
            .position(|member| member.reference == through)
            .ok_or(ContextCompactionCorruption::Inconsistent(
                "context summary through endpoint",
            ))?;
        let summary_index = visible
            .iter()
            .position(|member| member.reference == summary.reference)
            .ok_or(ContextCompactionCorruption::Inconsistent(
                "context summary entry",
            ))?;
        let physical_through = physical_positions.get(&through).copied().ok_or(
            ContextCompactionCorruption::Inconsistent("context summary physical boundary"),
        )?;
        if first_index != 0
            || through_index < first_index
            || summary.position <= physical_through
            || summary_index <= through_index
            || !range_closes_tool_exchanges(&visible[..=through_index])
        {
            return Err(
                ContextCompactionCorruption::Inconsistent("context frontier projection").into(),
            );
        }
        visible = std::iter::once(summary.clone())
            .chain(
                visible[through_index + 1..]
                    .iter()
                    .filter(|member| member.reference != summary.reference)
                    .cloned(),
            )
            .collect();
    }
    Ok(visible)
}

fn range_closes_tool_exchanges(members: &[ProjectedFrontierMember]) -> bool {
    let mut open_requests = 0usize;
    for member in members {
        match member.payload_kind.as_str() {
            "assistant_tool_use" => open_requests = open_requests.saturating_add(1),
            "tool_execution_result" | "tool_denied" | "tool_closed_by_turn_end" => {
                let Some(remaining) = open_requests.checked_sub(1) else {
                    return false;
                };
                open_requests = remaining;
            }
            _ => {}
        }
    }
    open_requests == 0
}

fn preview_members(
    members: &[ProjectedFrontierMember],
) -> Result<Vec<AutomaticContextCompactionPreviewMember>, ContextCompactionRepositoryError> {
    let mut open_requests = 0_usize;
    let mut preview = Vec::with_capacity(members.len());
    for member in members {
        match member.payload_kind.as_str() {
            "assistant_tool_use" => open_requests = open_requests.saturating_add(1),
            "tool_execution_result" | "tool_denied" | "tool_closed_by_turn_end" => {
                open_requests = open_requests.checked_sub(1).ok_or(
                    ContextCompactionCorruption::Inconsistent("context compaction tool exchange"),
                )?;
            }
            _ => {}
        }
        preview.push(AutomaticContextCompactionPreviewMember {
            position: member.position,
            reference: member.reference,
            safe_boundary: open_requests == 0,
        });
    }
    Ok(preview)
}

fn latest_safe_boundary(members: &[ProjectedFrontierMember]) -> Option<usize> {
    let mut latest = None;
    let mut open_requests = 0usize;
    for (index, member) in members.iter().enumerate() {
        match member.payload_kind.as_str() {
            "assistant_tool_use" => open_requests = open_requests.saturating_add(1),
            "tool_execution_result" | "tool_denied" | "tool_closed_by_turn_end" => {
                open_requests = open_requests.checked_sub(1)?;
            }
            _ => {}
        }
        if open_requests == 0 {
            latest = Some(index);
        }
    }
    latest
}

fn required<T>(
    row: &sqlx::postgres::PgRow,
    field: &'static str,
) -> Result<T, ContextCompactionRepositoryError>
where
    for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or_else(|| ContextCompactionCorruption::Missing(field).into())
}

fn decode_u64(
    value: Decimal,
    field: &'static str,
) -> Result<u64, ContextCompactionRepositoryError> {
    u64::try_from(value).map_err(|_| ContextCompactionCorruption::InvalidOrdinal(field).into())
}

/// Classifies one failed completion write that carries a result identity.
///
/// A uniqueness violation here names durable rows the prepared identities
/// cannot own. Repeating the transition is futile: the identities are pinned by
/// the in-flight lifecycle and every attempt writes exactly the same rows, so
/// the daemon's resolve loop must stop rather than resubmit the identical
/// statement forever. `prepare` rejects an identity already taken before the
/// provider is called; this is the fail-closed backstop for one taken after
/// that read.
fn classify_completion_write(error: sqlx::Error) -> ContextCompactionRepositoryError {
    if error
        .as_database_error()
        .is_some_and(|database| database.code().as_deref() == Some("23505"))
    {
        return ContextCompactionCorruption::Inconsistent("compaction result identity").into();
    }
    error.into()
}

fn require_single(
    rows: u64,
    relationship: &'static str,
) -> Result<(), ContextCompactionRepositoryError> {
    if rows == 1 {
        Ok(())
    } else {
        Err(ContextCompactionCorruption::Inconsistent(relationship).into())
    }
}

/// Committed storage facts could not form one exact compaction lifecycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextCompactionCorruption {
    /// Required durable fact was absent.
    Missing(&'static str),
    /// Stored ordinal was outside the admitted u64 range.
    InvalidOrdinal(&'static str),
    /// Related lifecycle facts disagreed.
    Inconsistent(&'static str),
    /// Stored command result discriminator is unknown.
    UnsupportedResult(String),
    /// Stored user-global command-kind discriminator is unknown.
    UnsupportedCommandKind(String),
    /// Summary text did not satisfy the semantic entry scalar.
    InvalidSummary,
}

impl fmt::Display for ContextCompactionCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("context compaction storage is inconsistent")
    }
}

impl Error for ContextCompactionCorruption {}

/// Database, collision, or fail-closed corruption from compaction persistence.
#[derive(Debug)]
pub enum ContextCompactionRepositoryError {
    /// Database operation failed before an ambiguous commit boundary.
    Database(sqlx::Error),
    /// Commit outcome could not be proven.
    CommitAmbiguous(sqlx::Error),
    /// A daemon-minted call or result identity collided globally and may be
    /// reminted.
    IdentityCollision,
    /// Durable rows contradicted the closed lifecycle.
    Corruption(ContextCompactionCorruption),
}

impl ContextCompactionRepositoryError {
    fn commit(error: sqlx::Error) -> Self {
        if commit_failure_is_ambiguous(&error) {
            Self::CommitAmbiguous(error)
        } else {
            Self::Database(error)
        }
    }
}

impl fmt::Display for ContextCompactionRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("context compaction persistence failed")
    }
}

impl Error for ContextCompactionRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::Corruption(error) => Some(error),
            Self::IdentityCollision => None,
        }
    }
}

impl ClassifyOperatorFailure for ContextCompactionRepositoryError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Database(_) => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::CommitAmbiguous(_) => OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            },
            Self::IdentityCollision => OperatorFailureClass::IdentityCollision,
            Self::Corruption(_) => OperatorFailureClass::FailClosedCorruption,
        }
    }
}

impl From<sqlx::Error> for ContextCompactionRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<ContextCompactionCorruption> for ContextCompactionRepositoryError {
    fn from(error: ContextCompactionCorruption) -> Self {
        Self::Corruption(error)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ProjectedFrontierMember, Uuid, latest_safe_boundary, preview_members,
        project_frontier_members,
    };
    use signalbox_domain::{SemanticTranscriptEntryId, SemanticTranscriptEntryRef, SessionId};

    fn entry(value: u128) -> SemanticTranscriptEntryRef {
        SemanticTranscriptEntryRef::from_source(
            SessionId::from_uuid(Uuid::from_u128(0x7000)),
            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value)),
        )
    }

    fn ordinary(position: u64, reference: SemanticTranscriptEntryRef) -> ProjectedFrontierMember {
        ProjectedFrontierMember {
            position,
            reference,
            payload_kind: String::from("origin_accepted_input"),
            summary_range: None,
        }
    }

    fn summary(
        position: u64,
        reference: SemanticTranscriptEntryRef,
        first: SemanticTranscriptEntryRef,
        through: SemanticTranscriptEntryRef,
    ) -> ProjectedFrontierMember {
        ProjectedFrontierMember {
            position,
            reference,
            payload_kind: String::from("context_summary"),
            summary_range: Some((first, through)),
        }
    }

    /// successor compaction selection follows model-visible order even
    /// when the retained suffix physically precedes the predecessor summary.
    #[test]
    fn successor_boundary_uses_projected_order() {
        let first = entry(0x7001);
        let root_through = entry(0x7002);
        let retained_suffix = entry(0x7003);
        let root_summary = entry(0x7004);
        let complete = vec![
            ordinary(1, first),
            ordinary(2, root_through),
            ordinary(3, retained_suffix),
            summary(4, root_summary, first, root_through),
        ];
        let visible = project_frontier_members(complete).expect("the root projection is valid");

        assert_eq!(visible[0].reference, root_summary);
        assert_eq!(visible[0].position, 4);
        assert_eq!(visible[1].reference, retained_suffix);
        assert_eq!(visible[1].position, 3);
        assert_eq!(latest_safe_boundary(&visible), Some(1));
    }

    /// a successor summary can replace a boundary whose physical
    /// position precedes the summary that begins its visible range.
    #[test]
    fn successor_summary_projects_over_physical_reversal() {
        let first = entry(0x7011);
        let root_through = entry(0x7012);
        let retained_suffix = entry(0x7013);
        let root_summary = entry(0x7014);
        let successor_summary = entry(0x7015);
        let complete = vec![
            ordinary(1, first),
            ordinary(2, root_through),
            ordinary(3, retained_suffix),
            summary(4, root_summary, first, root_through),
            summary(5, successor_summary, root_summary, retained_suffix),
        ];
        let visible =
            project_frontier_members(complete).expect("the successor projection is valid");

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].reference, successor_summary);
        assert_eq!(visible[0].position, 5);
    }

    #[test]
    fn automatic_preview_marks_only_closed_tool_exchange_boundaries_safe() {
        let visible = vec![
            ProjectedFrontierMember {
                position: 1,
                reference: entry(0x7031),
                payload_kind: "assistant_tool_use".to_owned(),
                summary_range: None,
            },
            ProjectedFrontierMember {
                position: 2,
                reference: entry(0x7032),
                payload_kind: "assistant_tool_use".to_owned(),
                summary_range: None,
            },
            ProjectedFrontierMember {
                position: 3,
                reference: entry(0x7033),
                payload_kind: "tool_execution_result".to_owned(),
                summary_range: None,
            },
            ProjectedFrontierMember {
                position: 4,
                reference: entry(0x7034),
                payload_kind: "tool_execution_result".to_owned(),
                summary_range: None,
            },
            ordinary(5, entry(0x7035)),
        ];
        let preview = preview_members(&visible).expect("the tool exchange is balanced");

        assert!(!preview[0].is_safe_boundary());
        assert!(!preview[1].is_safe_boundary());
        assert!(!preview[2].is_safe_boundary());
        assert!(preview[3].is_safe_boundary());
        assert!(preview[4].is_safe_boundary());
    }
}
