//! Durable explicit context-compaction command and call lifecycle.

use std::{collections::BTreeMap, error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    ContextCompactionId, ContextCompactionTokenUsage, ContextFrontierId, DirectModelSelection,
    DurableCommandId, ModelCallId, ResolvedProviderTarget, SemanticTranscriptEntryId,
    SemanticTranscriptEntryRef, SessionConfigurationDefaultsVersion, SessionId,
};
use sqlx::{PgPool, Row, types::Uuid};

use crate::{commit_failure_is_ambiguous, mapping::session_id_to_uuid};

/// All caller and hub-minted facts for a fresh explicit command attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrepareContextCompactionRequest {
    /// Owner-global command identity.
    pub command: DurableCommandId,
    /// Session whose complete frontier is summarized.
    pub session: SessionId,
    /// Optional exact one-based complete-frontier position.
    pub requested_through_position: Option<u64>,
    /// Current defaults epoch observed before entering the transaction.
    pub defaults_version: SessionConfigurationDefaultsVersion,
    /// Current direct model selection after freezing any alias.
    pub selection: DirectModelSelection,
    /// Exact resolved provider target.
    pub target: ResolvedProviderTarget,
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
    summary_entry: SemanticTranscriptEntryId,
    result_frontier: ContextFrontierId,
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
}

/// Terminal disposition recorded for a failed dedicated call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailedContextCompactionDisposition {
    /// Provider interaction definitively failed.
    KnownFailed,
    /// Cancellation was confirmed.
    Cancelled,
    /// Provider acceptance or completion remained uncertain.
    Ambiguous,
}

impl FailedContextCompactionDisposition {
    const fn as_str(self) -> &'static str {
        match self {
            Self::KnownFailed => "known_failed",
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

    /// Commits InFlight before any provider interaction begins.
    pub async fn authorize(
        &self,
        prepared: &PreparedContextCompaction,
    ) -> Result<(), ContextCompactionRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let rows = sqlx::query(
            "UPDATE context_compaction_model_call
                SET state_kind = 'in_flight'
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
        .await?;
        sqlx::query(
            "INSERT INTO context_frontier
                (owning_session_id, context_frontier_id,
                 prefix_context_frontier_id, member_count)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(session_id_to_uuid(prepared.session))
        .bind(prepared.result_frontier.into_uuid())
        .bind(prepared.source_frontier.into_uuid())
        .bind(Decimal::from(result_count))
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO context_frontier_delta
                (owning_session_id, context_frontier_id, member_position,
                 source_session_id, semantic_entry_id)
             VALUES ($1, $2, $3, $1, $4)",
        )
        .bind(session_id_to_uuid(prepared.session))
        .bind(prepared.result_frontier.into_uuid())
        .bind(Decimal::from(result_count))
        .bind(prepared.summary_entry.into_uuid())
        .execute(&mut *transaction)
        .await?;
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
        .await?;
        let command_rows = sqlx::query(
            "UPDATE compact_session_command
                SET result_kind = 'applied',
                    result_context_compaction_id = $1,
                    result_model_call_id = $2,
                    result_through_position = $3,
                    result_summary_entry_id = $4,
                    result_frontier_id = $5
              WHERE command_id = $6
                AND session_id = $7
                AND result_kind = 'pending'",
        )
        .bind(prepared.compaction.into_uuid())
        .bind(prepared.call.into_uuid())
        .bind(Decimal::from(prepared.through_position))
        .bind(prepared.summary_entry.into_uuid())
        .bind(prepared.result_frontier.into_uuid())
        .bind(prepared.command.into_uuid())
        .bind(session_id_to_uuid(prepared.session))
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        require_single(command_rows, "applied compaction command")?;
        transaction
            .commit()
            .await
            .map_err(ContextCompactionRepositoryError::commit)?;
        Ok(prepared.applied())
    }

    /// Records one failed or uncertain dedicated call and failed command.
    pub async fn fail(
        &self,
        prepared: &PreparedContextCompaction,
        disposition: FailedContextCompactionDisposition,
    ) -> Result<(), ContextCompactionRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let call_rows = sqlx::query(
            "UPDATE context_compaction_model_call
                SET state_kind = 'terminal', terminal_disposition_kind = $1
              WHERE model_call_id = $2
                AND session_id = $3
                AND state_kind IN ('prepared', 'in_flight')",
        )
        .bind(disposition.as_str())
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

async fn prepare_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request: &PrepareContextCompactionRequest,
) -> Result<(bool, PrepareContextCompactionOutcome), ContextCompactionRepositoryError> {
    let existing_kind: Option<String> =
        sqlx::query_scalar("SELECT command_kind FROM durable_command WHERE command_id = $1")
            .bind(request.command.into_uuid())
            .fetch_optional(&mut **transaction)
            .await?;
    if let Some(kind) = existing_kind {
        if kind != "compact_session" {
            return Ok((false, PrepareContextCompactionOutcome::ConflictingReuse));
        }
        return load_replay(transaction, request)
            .await
            .map(|outcome| (false, outcome));
    }
    let current_version: Option<Decimal> = sqlx::query_scalar(
        "SELECT current.current_version
           FROM session
           LEFT JOIN session_current_defaults AS current
             ON current.session_id = session.session_id
          WHERE session.session_id = $1
          FOR UPDATE OF session",
    )
    .bind(session_id_to_uuid(request.session))
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(current_version) = current_version else {
        return Ok((false, PrepareContextCompactionOutcome::SessionNotFound));
    };
    if decode_u64(current_version, "current defaults version")? != request.defaults_version.as_u64()
    {
        return Ok((false, PrepareContextCompactionOutcome::DefaultsChanged));
    }
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM turn_lifecycle
              WHERE session_id = $1 AND state_kind = 'active'
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
    let source = sqlx::query(
        "WITH candidate (frontier_id) AS (
            SELECT terminal_frontier_id
              FROM turn_lifecycle
             WHERE session_id = $1 AND state_kind = 'terminal'
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
    .bind(session_id_to_uuid(request.session))
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(source) = source else {
        return Ok((false, PrepareContextCompactionOutcome::NoBoundary));
    };
    let source_frontier = ContextFrontierId::from_uuid(source.try_get("context_frontier_id")?);
    let member_count = decode_u64(source.try_get("member_count")?, "source member count")?;
    if member_count == 0 {
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
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'compact_session', 1, clock_timestamp())",
    )
    .bind(request.command.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO compact_session_command
            (command_id, command_kind, storage_version, session_id,
             requested_through_position, result_kind)
         VALUES ($1, 'compact_session', 1, $2, $3, 'pending')",
    )
    .bind(request.command.into_uuid())
    .bind(session_id_to_uuid(request.session))
    .bind(request.requested_through_position.map(Decimal::from))
    .execute(&mut **transaction)
    .await?;
    let insert_call = sqlx::query(
        "INSERT INTO context_compaction_model_call
            (model_call_id, session_id, direct_model_selection_id,
             resolved_provider_model_identity_id, source_frontier_id,
             credential_reference, state_kind)
         VALUES ($1, $2, $3, $4, $5, $6, 'prepared')",
    )
    .bind(request.call.into_uuid())
    .bind(session_id_to_uuid(request.session))
    .bind(request.selection.into_uuid())
    .bind(request.target.identity().into_uuid())
    .bind(source_frontier.into_uuid())
    .bind(&request.credential_reference)
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
            summary_entry: request.summary_entry,
            result_frontier: request.result_frontier,
        })),
    ))
}

async fn load_replay(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request: &PrepareContextCompactionRequest,
) -> Result<PrepareContextCompactionOutcome, ContextCompactionRepositoryError> {
    let row = sqlx::query(
        "SELECT session_id, requested_through_position, result_kind,
                result_context_compaction_id, result_model_call_id,
                result_through_position, result_summary_entry_id, result_frontier_id
           FROM compact_session_command
          WHERE command_id = $1",
    )
    .bind(request.command.into_uuid())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(ContextCompactionCorruption::Missing(
        "compaction command detail",
    ))?;
    let session: Uuid = row.try_get("session_id")?;
    let requested: Option<Decimal> = row.try_get("requested_through_position")?;
    if session != session_id_to_uuid(request.session)
        || requested
            .map(|value| decode_u64(value, "requested through position"))
            .transpose()?
            != request.requested_through_position
    {
        return Ok(PrepareContextCompactionOutcome::ConflictingReuse);
    }
    let kind: String = row.try_get("result_kind")?;
    if kind == "pending" {
        return Ok(PrepareContextCompactionOutcome::Busy);
    }
    if kind == "failed" {
        return Ok(PrepareContextCompactionOutcome::FailedReplay);
    }
    if kind != "applied" {
        return Err(ContextCompactionCorruption::UnsupportedResult(kind).into());
    }
    Ok(PrepareContextCompactionOutcome::Replayed(
        AppliedContextCompaction {
            compaction: ContextCompactionId::from_uuid(required(
                &row,
                "result_context_compaction_id",
            )?),
            call: ModelCallId::from_uuid(required(&row, "result_model_call_id")?),
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

async fn load_projected_frontier_members(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
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
    .fetch_all(&mut **transaction)
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
    /// A daemon-minted call identity collided globally and may be reminted.
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
    use super::{ProjectedFrontierMember, Uuid, latest_safe_boundary, project_frontier_members};
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

    /// INV-015: successor compaction selection follows model-visible order even
    /// when the retained suffix physically precedes the predecessor summary.
    #[test]
    fn inv015_successor_boundary_uses_projected_order() {
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

    /// INV-015: a successor summary can replace a boundary whose physical
    /// position precedes the summary that begins its visible range.
    #[test]
    fn inv015_successor_summary_projects_over_physical_reversal() {
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
}
