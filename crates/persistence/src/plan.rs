//! PostgreSQL storage for append-only session plan events.

use std::{collections::HashSet, error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
use signalbox_domain::{
    SessionId, ToolAttemptDispatchCorrelation, ToolAttemptDispatchCorrelationReconstitutionInput,
    ToolAttemptId, ToolDispatchGeneration, ToolRequestId, TurnAttemptId, TurnId,
};
use signalbox_tools_plan::{
    PLAN_WRITE_NAME, PlanAppendOutcome, PlanAppendRejection, PlanAppendRequest, PlanEntry,
    PlanEntryId, PlanEvent, PlanEventDraft, PlanEventKind, PlanEventOrdinal, PlanEventProvenance,
    PlanFoldError, PlanHistoryPage, PlanPageCompleteness, PlanReadPage, PlanReadRequest,
    PlanStatus, PlanText, SessionPlanPort, fold_plan_events,
};
use sqlx::{PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{commit_failure_is_ambiguous, mapping};

const REPEATABLE_READ_ONLY: &str = "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY";

const CURRENT_PLAN_SQL: &str = "SELECT created.event_ordinal AS creation_event_ordinal,
       created.entry_ordinal AS entry_ordinal,
       created.entry_text AS created_text,
       created.entry_status AS created_status,
       session_plan_event_has_authority(created) AS created_authorized,
       revision.event_ordinal AS revision_event_ordinal,
       revision.entry_text AS revised_text,
       revision.entry_status AS revised_status,
       revision.authorized AS revision_authorized,
       movement.event_ordinal AS status_event_ordinal,
       movement.entry_text AS moved_text,
       movement.entry_status AS moved_status,
       movement.authorized AS movement_authorized
  FROM session_plan_event AS created
  LEFT JOIN LATERAL (
      SELECT event_ordinal, entry_text, entry_status,
             session_plan_event_has_authority(candidate) AS authorized
        FROM session_plan_event AS candidate
       WHERE session_id = created.session_id
         AND entry_ordinal = created.event_ordinal
         AND event_kind = 'text_revised'
       ORDER BY event_ordinal DESC
       LIMIT 1
  ) AS revision ON TRUE
  LEFT JOIN LATERAL (
      SELECT event_ordinal, entry_text, entry_status,
             session_plan_event_has_authority(candidate) AS authorized
        FROM session_plan_event AS candidate
       WHERE session_id = created.session_id
         AND entry_ordinal = created.event_ordinal
         AND event_kind = 'status_changed'
       ORDER BY event_ordinal DESC
       LIMIT 1
  ) AS movement ON TRUE
 WHERE created.session_id = $1
   AND created.event_kind = 'created'
   AND ($2::numeric IS NULL OR created.event_ordinal > $2)
 ORDER BY created.event_ordinal
 LIMIT $3";

const ACTIVE_APPEND_AUTHORITY_SQL: &str = "SELECT attempt.attempt_id
  FROM tool_attempt AS attempt
  JOIN tool_request AS request
    ON request.request_id = attempt.request_id
 WHERE attempt.attempt_id = $1
   AND attempt.request_id = $2
   AND attempt.issuing_turn_attempt_id = $3
   AND attempt.dispatch_generation = $4
   AND attempt.turn_id = $5
   AND attempt.session_id = $6
   AND attempt.effect_class = 'external_effect'
   AND attempt.state_kind = 'in_flight'
   AND request.request_id = $2
   AND request.session_id = $6
   AND request.turn_id = $5
   AND request.tool_name = 'plan_write'
   AND request.arguments_kind = 'json'
   AND request.arguments_text::jsonb =
        CASE $7::text
            WHEN 'created' THEN jsonb_build_object(
                'kind', 'create',
                'text', $9::text
            )
            WHEN 'text_revised' THEN jsonb_build_object(
                'kind', 'revise',
                'entry_id', $8::numeric,
                'text', $9::text
            )
            WHEN 'status_changed' THEN jsonb_build_object(
                'kind', 'set_status',
                'entry_id', $8::numeric,
                'status', $10::text
            )
        END
 FOR SHARE OF attempt";

const UNSUPPORTED_EVENT_KIND_SQL: &str = "SELECT event_kind
  FROM session_plan_event
 WHERE session_id = $1
   AND event_kind NOT IN ('created', 'text_revised', 'status_changed')
 LIMIT 1";

const INVALID_EVENT_SEQUENCE_SQL: &str = "WITH ordered AS (
    SELECT event_ordinal,
           lag(event_ordinal, 1, 0::numeric) OVER (ORDER BY event_ordinal) AS prior_ordinal
      FROM session_plan_event
     WHERE session_id = $1
), invalid_ordinal AS (
    SELECT 1
      FROM ordered
     WHERE event_ordinal <> prior_ordinal + 1
     LIMIT 1
), missing_creation AS (
    SELECT 1
      FROM session_plan_event AS mutation
      LEFT JOIN session_plan_event AS creation
        ON creation.session_id = mutation.session_id
       AND creation.event_ordinal = mutation.entry_ordinal
       AND creation.event_kind = 'created'
     WHERE mutation.session_id = $1
       AND mutation.event_kind IN ('text_revised', 'status_changed')
       AND creation.event_ordinal IS NULL
     LIMIT 1
)
SELECT EXISTS (SELECT 1 FROM invalid_ordinal)
    OR EXISTS (SELECT 1 FROM missing_creation)";

const HISTORY_SQL: &str = "SELECT event.event_ordinal, event.event_kind,
       event.entry_ordinal, event.entry_text, event.entry_status,
       event.provenance_turn_id, event.provenance_issuing_turn_attempt_id,
       event.provenance_request_id, event.provenance_attempt_id,
       event.provenance_dispatch_generation,
       attempt.attempt_id AS authority_attempt_id,
       attempt.request_id AS authority_attempt_request_id,
       attempt.session_id AS authority_attempt_session_id,
       attempt.turn_id AS authority_attempt_turn_id,
       attempt.issuing_turn_attempt_id AS authority_issuing_turn_attempt_id,
       attempt.effect_class AS authority_effect_class,
       attempt.dispatch_generation AS authority_dispatch_generation,
       request.request_id AS authority_request_id,
       request.session_id AS authority_request_session_id,
       request.turn_id AS authority_request_turn_id,
       request.tool_name AS authority_tool_name,
       request.arguments_kind AS authority_arguments_kind,
       request.arguments_text AS authority_arguments_text
  FROM session_plan_event AS event
  LEFT JOIN tool_attempt AS attempt
    ON attempt.attempt_id = event.provenance_attempt_id
  LEFT JOIN tool_request AS request
    ON request.request_id = attempt.request_id
 WHERE event.session_id = $1
 ORDER BY event.event_ordinal
 LIMIT $2";

/// A durable plan row failed checked reconstruction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionPlanCorruption {
    /// A required durable field was absent.
    Missing(&'static str),
    /// A numeric value was not a positive u64.
    InvalidPositiveInteger(&'static str),
    /// A closed discriminator was unsupported.
    Unsupported {
        /// Durable field being decoded.
        field: &'static str,
        /// Unsupported spelling.
        value: String,
    },
    /// Two stored identity fields disagreed.
    MismatchedIdentity(&'static str),
    /// Nullable event payload fields did not match their discriminator.
    InvalidEventPayload(&'static str),
    /// Stored text violated the tool boundary.
    InvalidText,
    /// The durable event sequence has a gap or a mutation without a creation.
    InvalidEventSequence,
    /// The chronological durable prefix cannot be folded.
    InvalidHistory(PlanFoldError),
    /// Two durable events claim the same physical tool attempt.
    DuplicateProvenance,
    /// Durable provenance does not match tool-attempt authority.
    UntrustedProvenance,
}

impl fmt::Display for SessionPlanCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "session plan is missing {field}"),
            Self::InvalidPositiveInteger(field) => {
                write!(formatter, "session plan has invalid positive {field}")
            }
            Self::Unsupported { field, value } => {
                write!(formatter, "session plan has unsupported {field}: {value}")
            }
            Self::MismatchedIdentity(field) => {
                write!(formatter, "session plan has mismatched {field}")
            }
            Self::InvalidEventPayload(kind) => {
                write!(formatter, "session plan has invalid {kind} payload")
            }
            Self::InvalidText => formatter.write_str("session plan has invalid entry text"),
            Self::InvalidEventSequence => {
                formatter.write_str("session plan event sequence is invalid")
            }
            Self::InvalidHistory(error) => {
                write!(formatter, "session plan history is invalid: {error}")
            }
            Self::DuplicateProvenance => {
                formatter.write_str("session plan history repeats tool-attempt provenance")
            }
            Self::UntrustedProvenance => {
                formatter.write_str("session plan provenance lacks durable authority")
            }
        }
    }
}

impl Error for SessionPlanCorruption {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidHistory(error) => Some(error),
            Self::InvalidEventSequence
            | Self::Missing(_)
            | Self::InvalidPositiveInteger(_)
            | Self::Unsupported { .. }
            | Self::MismatchedIdentity(_)
            | Self::InvalidEventPayload(_)
            | Self::InvalidText
            | Self::DuplicateProvenance
            | Self::UntrustedProvenance => None,
        }
    }
}

/// PostgreSQL plan storage failure.
#[derive(Debug)]
pub enum SessionPlanRepositoryError {
    /// PostgreSQL failed before or during commit.
    Database {
        /// Source database error.
        source: sqlx::Error,
        /// Whether the final commit outcome is unknown.
        commit_ambiguous: bool,
    },
    /// Durable rows cannot satisfy the port contract.
    Corruption(SessionPlanCorruption),
    /// The caller supplied provenance that is not an active plan-write attempt.
    InvalidAppendProvenance,
}

impl fmt::Display for SessionPlanRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database { source, .. } => {
                write!(formatter, "session plan database failure: {source}")
            }
            Self::Corruption(error) => error.fmt(formatter),
            Self::InvalidAppendProvenance => {
                formatter.write_str("session plan append provenance is not active")
            }
        }
    }
}

impl Error for SessionPlanRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::Corruption(error) => Some(error),
            Self::InvalidAppendProvenance => None,
        }
    }
}

impl From<sqlx::Error> for SessionPlanRepositoryError {
    fn from(source: sqlx::Error) -> Self {
        let constraint = source
            .as_database_error()
            .and_then(|database| database.constraint());
        if matches!(
            constraint,
            Some(
                "session_plan_event_requires_active_plan_write_attempt"
                    | "session_plan_event_provenance_attempt_id_key"
            )
        ) {
            return Self::InvalidAppendProvenance;
        }
        Self::Database {
            source,
            commit_ambiguous: false,
        }
    }
}

impl From<SessionPlanCorruption> for SessionPlanRepositoryError {
    fn from(error: SessionPlanCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl ClassifyOperatorFailure for SessionPlanRepositoryError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Database {
                commit_ambiguous, ..
            } => OperatorFailureClass::Infrastructure {
                commit_ambiguous: *commit_ambiguous,
            },
            Self::Corruption(_) => OperatorFailureClass::FailClosedCorruption,
            Self::InvalidAppendProvenance => OperatorFailureClass::CallerOrHubBug,
        }
    }
}

/// PostgreSQL repository for session plan appends and reads.
#[derive(Clone, Debug)]
pub struct SessionPlanRepository {
    pool: PgPool,
}

impl SessionPlanRepository {
    /// Uses the supplied guarded application pool.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Atomically appends one event under the owning session lock.
    pub async fn append(
        &self,
        request: PlanAppendRequest,
    ) -> Result<PlanAppendOutcome, SessionPlanRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let next: Decimal = sqlx::query_scalar("SELECT next_session_plan_event_ordinal($1)")
            .bind(request.session().into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let next = PlanEventOrdinal::try_from_u64(positive_u64(next, "next event ordinal")?)
            .ok_or(SessionPlanCorruption::InvalidPositiveInteger(
                "next event ordinal",
            ))?;
        let encoded = EncodedDraft::new(next, request.draft());
        let correlation = request.provenance().correlation();
        let authorized: Option<Uuid> = sqlx::query_scalar(ACTIVE_APPEND_AUTHORITY_SQL)
            .bind(correlation.attempt().into_uuid())
            .bind(correlation.request().into_uuid())
            .bind(correlation.issuing_attempt().into_uuid())
            .bind(Decimal::from(correlation.generation().as_u64()))
            .bind(correlation.turn().into_uuid())
            .bind(request.session().into_uuid())
            .bind(mapping::plan_event_kind_to_str(encoded.kind))
            .bind(Decimal::from(encoded.entry.as_u64()))
            .bind(encoded.text)
            .bind(encoded.status)
            .fetch_optional(&mut *transaction)
            .await?;
        if authorized.is_none() {
            transaction.rollback().await?;
            return Err(SessionPlanRepositoryError::InvalidAppendProvenance);
        }

        if let Some(entry) = draft_target(request.draft()) {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                    SELECT 1
                      FROM session_plan_event
                     WHERE session_id = $1
                       AND event_ordinal = $2
                       AND event_kind = 'created'
                )",
            )
            .bind(request.session().into_uuid())
            .bind(Decimal::from(entry.as_u64()))
            .fetch_one(&mut *transaction)
            .await?;
            if !exists {
                transaction.rollback().await?;
                return Ok(PlanAppendOutcome::Rejected(
                    PlanAppendRejection::UnknownEntry { entry },
                ));
            }
        }

        sqlx::query(
            "INSERT INTO session_plan_event
                (session_id, event_ordinal, event_kind, entry_ordinal,
                 entry_text, entry_status, provenance_turn_id,
                 provenance_issuing_turn_attempt_id, provenance_request_id,
                 provenance_attempt_id, provenance_dispatch_generation)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(request.session().into_uuid())
        .bind(Decimal::from(next.as_u64()))
        .bind(mapping::plan_event_kind_to_str(encoded.kind))
        .bind(Decimal::from(encoded.entry.as_u64()))
        .bind(encoded.text)
        .bind(encoded.status)
        .bind(request.provenance().correlation().turn().into_uuid())
        .bind(
            request
                .provenance()
                .correlation()
                .issuing_attempt()
                .into_uuid(),
        )
        .bind(request.provenance().correlation().request().into_uuid())
        .bind(request.provenance().correlation().attempt().into_uuid())
        .bind(Decimal::from(
            request.provenance().correlation().generation().as_u64(),
        ))
        .execute(&mut *transaction)
        .await?;

        transaction.commit().await.map_err(|source| {
            let commit_ambiguous = commit_failure_is_ambiguous(&source);
            SessionPlanRepositoryError::Database {
                source,
                commit_ambiguous,
            }
        })?;
        Ok(PlanAppendOutcome::Appended(PlanEvent::new(
            next,
            request.provenance(),
            event_kind_from_draft(request.draft().clone()),
        )))
    }

    /// Reads one bounded current page and optional history prefix.
    pub async fn read(
        &self,
        request: PlanReadRequest,
    ) -> Result<PlanReadPage, SessionPlanRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(REPEATABLE_READ_ONLY)
            .execute(&mut *transaction)
            .await?;
        let after = request
            .after_entry()
            .map(|entry| Decimal::from(entry.as_u64()));
        let current_limit = request
            .max_entries()
            .checked_add(1)
            .and_then(|limit| i64::try_from(limit).ok())
            .ok_or(SessionPlanCorruption::InvalidPositiveInteger(
                "current limit",
            ))?;
        let unsupported_kind: Option<String> = sqlx::query_scalar(UNSUPPORTED_EVENT_KIND_SQL)
            .bind(request.session().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        if let Some(value) = unsupported_kind {
            return Err(SessionPlanCorruption::Unsupported {
                field: "event kind",
                value,
            }
            .into());
        }
        let invalid_sequence: bool = sqlx::query_scalar(INVALID_EVENT_SEQUENCE_SQL)
            .bind(request.session().into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        if invalid_sequence {
            return Err(SessionPlanCorruption::InvalidEventSequence.into());
        }
        let mut entries = sqlx::query(CURRENT_PLAN_SQL)
            .bind(request.session().into_uuid())
            .bind(after)
            .bind(current_limit)
            .fetch_all(&mut *transaction)
            .await?
            .iter()
            .map(decode_entry)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more_entries = entries.len() > request.max_entries();
        entries.truncate(request.max_entries());

        let history = match request.history_limit() {
            Some(limit) => {
                let query_limit = limit
                    .checked_add(1)
                    .and_then(|limit| i64::try_from(limit).ok())
                    .ok_or(SessionPlanCorruption::InvalidPositiveInteger(
                        "history limit",
                    ))?;
                let mut events = sqlx::query(HISTORY_SQL)
                    .bind(request.session().into_uuid())
                    .bind(query_limit)
                    .fetch_all(&mut *transaction)
                    .await?
                    .iter()
                    .map(|row| decode_event(request.session(), row))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut provenance_attempts = HashSet::with_capacity(events.len());
                if !events.iter().all(|event| {
                    provenance_attempts.insert(event.provenance().correlation().attempt())
                }) {
                    return Err(SessionPlanCorruption::DuplicateProvenance.into());
                }
                fold_plan_events(&events).map_err(SessionPlanCorruption::InvalidHistory)?;
                let has_more = events.len() > limit;
                events.truncate(limit);
                Some(PlanHistoryPage::new(events, page_completeness(has_more)))
            }
            None => None,
        };
        let page = PlanReadPage::new(
            request.session(),
            entries,
            page_completeness(has_more_entries),
            history,
        );
        transaction.commit().await?;
        Ok(page)
    }
}

impl SessionPlanPort for SessionPlanRepository {
    type Error = SessionPlanRepositoryError;

    async fn append_plan_event(
        &mut self,
        request: PlanAppendRequest,
    ) -> Result<PlanAppendOutcome, Self::Error> {
        self.append(request).await
    }

    async fn read_plan(&mut self, request: PlanReadRequest) -> Result<PlanReadPage, Self::Error> {
        self.read(request).await
    }
}

struct EncodedDraft<'a> {
    kind: mapping::PlanEventStorageKind,
    entry: PlanEntryId,
    text: Option<&'a str>,
    status: Option<&'static str>,
}

impl<'a> EncodedDraft<'a> {
    fn new(ordinal: PlanEventOrdinal, draft: &'a PlanEventDraft) -> Self {
        match draft {
            PlanEventDraft::Create { text } => Self {
                kind: mapping::PlanEventStorageKind::Created,
                entry: PlanEntryId::from_creation_ordinal(ordinal),
                text: Some(text.as_str()),
                status: None,
            },
            PlanEventDraft::Revise { entry, text } => Self {
                kind: mapping::PlanEventStorageKind::TextRevised,
                entry: *entry,
                text: Some(text.as_str()),
                status: None,
            },
            PlanEventDraft::SetStatus { entry, status } => Self {
                kind: mapping::PlanEventStorageKind::StatusChanged,
                entry: *entry,
                text: None,
                status: Some(mapping::plan_status_to_str(*status)),
            },
        }
    }
}

fn draft_target(draft: &PlanEventDraft) -> Option<PlanEntryId> {
    match draft {
        PlanEventDraft::Create { .. } => None,
        PlanEventDraft::Revise { entry, .. } | PlanEventDraft::SetStatus { entry, .. } => {
            Some(*entry)
        }
    }
}

fn event_kind_from_draft(draft: PlanEventDraft) -> PlanEventKind {
    match draft {
        PlanEventDraft::Create { text } => PlanEventKind::Created { text },
        PlanEventDraft::Revise { entry, text } => PlanEventKind::TextRevised { entry, text },
        PlanEventDraft::SetStatus { entry, status } => {
            PlanEventKind::StatusChanged { entry, status }
        }
    }
}

fn decode_entry(row: &PgRow) -> Result<PlanEntry, SessionPlanRepositoryError> {
    let entry = PlanEntryId::try_from_u64(positive_u64(
        row.try_get("entry_ordinal")?,
        "entry ordinal",
    )?)
    .ok_or(SessionPlanCorruption::InvalidPositiveInteger(
        "entry ordinal",
    ))?;
    let creation_ordinal = PlanEventOrdinal::try_from_u64(positive_u64(
        row.try_get("creation_event_ordinal")?,
        "creation event ordinal",
    )?)
    .ok_or(SessionPlanCorruption::InvalidPositiveInteger(
        "creation event ordinal",
    ))?;
    if entry.creation_ordinal() != creation_ordinal {
        return Err(SessionPlanCorruption::MismatchedIdentity("current entry identity").into());
    }
    let created_authorized: bool = required(row, "created_authorized")?;
    if !created_authorized {
        return Err(SessionPlanCorruption::UntrustedProvenance.into());
    }
    let created_text: String = required(row, "created_text")?;
    let created_status: Option<String> = row.try_get("created_status")?;
    if created_status.is_some() {
        return Err(SessionPlanCorruption::InvalidEventPayload("current creation").into());
    }
    let revision_ordinal: Option<Decimal> = row.try_get("revision_event_ordinal")?;
    let revised_text: Option<String> = row.try_get("revised_text")?;
    let revised_status: Option<String> = row.try_get("revised_status")?;
    let revision_authorized: Option<bool> = row.try_get("revision_authorized")?;
    let text = match (
        revision_ordinal,
        revised_text,
        revised_status,
        revision_authorized,
    ) {
        (None, None, None, None) => created_text,
        (Some(revision_ordinal), Some(revised_text), None, Some(true)) => {
            let revision_ordinal = PlanEventOrdinal::try_from_u64(positive_u64(
                revision_ordinal,
                "revision event ordinal",
            )?)
            .ok_or(SessionPlanCorruption::InvalidPositiveInteger(
                "revision event ordinal",
            ))?;
            if revision_ordinal <= creation_ordinal {
                return Err(
                    SessionPlanCorruption::MismatchedIdentity("current revision ordering").into(),
                );
            }
            revised_text
        }
        (Some(_), Some(_), None, Some(false)) => {
            return Err(SessionPlanCorruption::UntrustedProvenance.into());
        }
        _ => {
            return Err(SessionPlanCorruption::InvalidEventPayload("current text revision").into());
        }
    };
    let text = PlanText::try_new(text).map_err(|_| SessionPlanCorruption::InvalidText)?;

    let status_ordinal: Option<Decimal> = row.try_get("status_event_ordinal")?;
    let moved_text: Option<String> = row.try_get("moved_text")?;
    let moved_status: Option<String> = row.try_get("moved_status")?;
    let movement_authorized: Option<bool> = row.try_get("movement_authorized")?;
    let status = match (
        status_ordinal,
        moved_text,
        moved_status,
        movement_authorized,
    ) {
        (None, None, None, None) => PlanStatus::Pending,
        (Some(status_ordinal), None, Some(moved_status), Some(true)) => {
            let status_ordinal = PlanEventOrdinal::try_from_u64(positive_u64(
                status_ordinal,
                "status event ordinal",
            )?)
            .ok_or(SessionPlanCorruption::InvalidPositiveInteger(
                "status event ordinal",
            ))?;
            if status_ordinal <= creation_ordinal {
                return Err(
                    SessionPlanCorruption::MismatchedIdentity("current status ordering").into(),
                );
            }
            mapping::plan_status_from_str(&moved_status).ok_or({
                SessionPlanCorruption::Unsupported {
                    field: "current status",
                    value: moved_status,
                }
            })?
        }
        (Some(_), None, Some(_), Some(false)) => {
            return Err(SessionPlanCorruption::UntrustedProvenance.into());
        }
        _ => {
            return Err(SessionPlanCorruption::InvalidEventPayload("current status change").into());
        }
    };
    Ok(PlanEntry::new(entry, text, status))
}

fn decode_event(session: SessionId, row: &PgRow) -> Result<PlanEvent, SessionPlanRepositoryError> {
    let ordinal = PlanEventOrdinal::try_from_u64(positive_u64(
        row.try_get("event_ordinal")?,
        "event ordinal",
    )?)
    .ok_or(SessionPlanCorruption::InvalidPositiveInteger(
        "event ordinal",
    ))?;
    let entry = PlanEntryId::try_from_u64(positive_u64(
        row.try_get("entry_ordinal")?,
        "entry ordinal",
    )?)
    .ok_or(SessionPlanCorruption::InvalidPositiveInteger(
        "entry ordinal",
    ))?;
    let provenance = decode_provenance(session, row)?;
    let event_kind: String = required(row, "event_kind")?;
    let storage_kind = mapping::plan_event_kind_from_str(&event_kind).ok_or(
        SessionPlanCorruption::Unsupported {
            field: "event kind",
            value: event_kind,
        },
    )?;
    let entry_text: Option<String> = row.try_get("entry_text")?;
    let entry_status: Option<String> = row.try_get("entry_status")?;
    let kind = match storage_kind {
        mapping::PlanEventStorageKind::Created => {
            let (Some(value), None) = (entry_text, entry_status) else {
                return Err(SessionPlanCorruption::InvalidEventPayload("created event").into());
            };
            if entry.creation_ordinal() != ordinal {
                return Err(
                    SessionPlanCorruption::MismatchedIdentity("created entry identity").into(),
                );
            }
            PlanEventKind::Created {
                text: decode_text(value)?,
            }
        }
        mapping::PlanEventStorageKind::TextRevised => {
            let (Some(value), None) = (entry_text, entry_status) else {
                return Err(
                    SessionPlanCorruption::InvalidEventPayload("text-revised event").into(),
                );
            };
            if entry.creation_ordinal() >= ordinal {
                return Err(SessionPlanCorruption::MismatchedIdentity(
                    "text-revised target ordering",
                )
                .into());
            }
            PlanEventKind::TextRevised {
                entry,
                text: decode_text(value)?,
            }
        }
        mapping::PlanEventStorageKind::StatusChanged => {
            let (None, Some(value)) = (entry_text, entry_status) else {
                return Err(
                    SessionPlanCorruption::InvalidEventPayload("status-changed event").into(),
                );
            };
            if entry.creation_ordinal() >= ordinal {
                return Err(SessionPlanCorruption::MismatchedIdentity(
                    "status-changed target ordering",
                )
                .into());
            }
            let status = mapping::plan_status_from_str(&value).ok_or({
                SessionPlanCorruption::Unsupported {
                    field: "entry status",
                    value,
                }
            })?;
            PlanEventKind::StatusChanged { entry, status }
        }
    };
    if !authority_payload_matches(row, &kind)? {
        return Err(SessionPlanCorruption::UntrustedProvenance.into());
    }
    Ok(PlanEvent::new(ordinal, provenance, kind))
}

fn authority_payload_matches(row: &PgRow, event: &PlanEventKind) -> Result<bool, sqlx::Error> {
    let arguments_kind: Option<String> = row.try_get("authority_arguments_kind")?;
    let arguments_text: Option<String> = row.try_get("authority_arguments_text")?;
    if arguments_kind.as_deref() != Some("json") {
        return Ok(false);
    }
    let Some(arguments_text) = arguments_text else {
        return Ok(false);
    };
    let Ok(actual) = serde_json::from_str::<serde_json::Value>(&arguments_text) else {
        return Ok(false);
    };
    let expected = match event {
        PlanEventKind::Created { text } => serde_json::json!({
            "kind": "create",
            "text": text.as_str(),
        }),
        PlanEventKind::TextRevised { entry, text } => serde_json::json!({
            "kind": "revise",
            "entry_id": entry.as_u64(),
            "text": text.as_str(),
        }),
        PlanEventKind::StatusChanged { entry, status } => serde_json::json!({
            "kind": "set_status",
            "entry_id": entry.as_u64(),
            "status": mapping::plan_status_to_str(*status),
        }),
    };
    Ok(actual == expected)
}

fn decode_text(value: String) -> Result<PlanText, SessionPlanRepositoryError> {
    PlanText::try_new(value)
        .map_err(|_| SessionPlanCorruption::InvalidText)
        .map_err(Into::into)
}

fn decode_provenance(
    session: SessionId,
    row: &PgRow,
) -> Result<PlanEventProvenance, SessionPlanRepositoryError> {
    let turn: Uuid = row.try_get("provenance_turn_id")?;
    let issuing_attempt: Uuid = row.try_get("provenance_issuing_turn_attempt_id")?;
    let request: Uuid = row.try_get("provenance_request_id")?;
    let attempt: Uuid = row.try_get("provenance_attempt_id")?;
    let generation_value: Decimal = row.try_get("provenance_dispatch_generation")?;
    let generation = positive_u64(generation_value, "provenance dispatch generation")?;
    let generation = ToolDispatchGeneration::try_from_u64(generation).ok_or(
        SessionPlanCorruption::InvalidPositiveInteger("provenance dispatch generation"),
    )?;
    let session_uuid = session.into_uuid();
    let authority_matches = row.try_get::<Option<Uuid>, _>("authority_attempt_id")?
        == Some(attempt)
        && row.try_get::<Option<Uuid>, _>("authority_attempt_request_id")? == Some(request)
        && row.try_get::<Option<Uuid>, _>("authority_attempt_session_id")? == Some(session_uuid)
        && row.try_get::<Option<Uuid>, _>("authority_attempt_turn_id")? == Some(turn)
        && row.try_get::<Option<Uuid>, _>("authority_issuing_turn_attempt_id")?
            == Some(issuing_attempt)
        && row
            .try_get::<Option<String>, _>("authority_effect_class")?
            .as_deref()
            == Some("external_effect")
        && row.try_get::<Option<Decimal>, _>("authority_dispatch_generation")?
            == Some(generation_value)
        && row.try_get::<Option<Uuid>, _>("authority_request_id")? == Some(request)
        && row.try_get::<Option<Uuid>, _>("authority_request_session_id")? == Some(session_uuid)
        && row.try_get::<Option<Uuid>, _>("authority_request_turn_id")? == Some(turn)
        && row
            .try_get::<Option<String>, _>("authority_tool_name")?
            .as_deref()
            == Some(PLAN_WRITE_NAME);
    if !authority_matches {
        return Err(SessionPlanCorruption::UntrustedProvenance.into());
    }
    Ok(PlanEventProvenance::from_invocation(
        ToolAttemptDispatchCorrelation::reconstitute(
            ToolAttemptDispatchCorrelationReconstitutionInput {
                session,
                turn: TurnId::from_uuid(turn),
                issuing_attempt: TurnAttemptId::from_uuid(issuing_attempt),
                request: ToolRequestId::from_uuid(request),
                attempt: ToolAttemptId::from_uuid(attempt),
                generation,
            },
        ),
    ))
}

fn page_completeness(has_more: bool) -> PlanPageCompleteness {
    if has_more {
        PlanPageCompleteness::Truncated
    } else {
        PlanPageCompleteness::Complete
    }
}

fn positive_u64(value: Decimal, field: &'static str) -> Result<u64, SessionPlanCorruption> {
    if value.fract().is_zero() && value > Decimal::ZERO {
        return u64::try_from(value)
            .map_err(|_| SessionPlanCorruption::InvalidPositiveInteger(field));
    }
    Err(SessionPlanCorruption::InvalidPositiveInteger(field))
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, SessionPlanRepositoryError>
where
    for<'decode> T: sqlx::Decode<'decode, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or(SessionPlanCorruption::Missing(field).into())
}
