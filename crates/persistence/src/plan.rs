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
       revision.event_ordinal AS revision_event_ordinal,
       revision.entry_text AS revised_text,
       revision.entry_status AS revised_status,
       movement.event_ordinal AS status_event_ordinal,
       movement.entry_text AS moved_text,
       movement.entry_status AS moved_status
  FROM session_plan_event AS created
  LEFT JOIN LATERAL (
      SELECT event_ordinal, entry_text, entry_status
        FROM session_plan_event
       WHERE session_id = created.session_id
         AND entry_ordinal = created.event_ordinal
         AND event_kind = 'text_revised'
       ORDER BY event_ordinal DESC
       LIMIT 1
  ) AS revision ON TRUE
  LEFT JOIN LATERAL (
      SELECT event_ordinal, entry_text, entry_status
        FROM session_plan_event
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
       request.tool_name AS authority_tool_name
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
            Self::Missing(_)
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
        if source
            .as_database_error()
            .and_then(|database| database.constraint())
            == Some("session_plan_event_requires_active_plan_write_attempt")
        {
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

        let encoded = EncodedDraft::new(next, request.draft());

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
    let created_text: String = required(row, "created_text")?;
    let created_status: Option<String> = row.try_get("created_status")?;
    if created_status.is_some() {
        return Err(SessionPlanCorruption::InvalidEventPayload("current creation").into());
    }
    let revision_ordinal: Option<Decimal> = row.try_get("revision_event_ordinal")?;
    let revised_text: Option<String> = row.try_get("revised_text")?;
    let revised_status: Option<String> = row.try_get("revised_status")?;
    let text = match (revision_ordinal, revised_text, revised_status) {
        (None, None, None) => created_text,
        (Some(revision_ordinal), Some(revised_text), None) => {
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
        _ => {
            return Err(SessionPlanCorruption::InvalidEventPayload("current text revision").into());
        }
    };
    let text = PlanText::try_new(text).map_err(|_| SessionPlanCorruption::InvalidText)?;

    let status_ordinal: Option<Decimal> = row.try_get("status_event_ordinal")?;
    let moved_text: Option<String> = row.try_get("moved_text")?;
    let moved_status: Option<String> = row.try_get("moved_status")?;
    let status = match (status_ordinal, moved_text, moved_status) {
        (None, None, None) => PlanStatus::Pending,
        (Some(status_ordinal), None, Some(moved_status)) => {
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
    Ok(PlanEvent::new(ordinal, provenance, kind))
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
