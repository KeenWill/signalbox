//! PostgreSQL storage for append-only session plan events.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
use signalbox_domain::{
    SessionId, ToolAttemptDispatchCorrelation, ToolAttemptDispatchCorrelationReconstitutionInput,
    ToolAttemptId, ToolDispatchGeneration, ToolRequestId, TurnAttemptId, TurnId,
};
use signalbox_tools_plan::{
    PlanAppendOutcome, PlanAppendRejection, PlanAppendRequest, PlanEntry, PlanEntryId, PlanEvent,
    PlanEventDraft, PlanEventKind, PlanEventOrdinal, PlanEventProvenance, PlanHistoryPage,
    PlanPageCompleteness, PlanReadPage, PlanReadRequest, PlanText, SessionPlanPort,
};
use sqlx::{PgPool, Row, postgres::PgRow};

use crate::{commit_failure_is_ambiguous, mapping};

const REPEATABLE_READ_ONLY: &str = "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY";

const CURRENT_PLAN_SQL: &str = "SELECT created.event_ordinal AS creation_event_ordinal,
       created.entry_ordinal AS entry_ordinal,
       COALESCE((
           SELECT revision.entry_text
             FROM session_plan_event AS revision
            WHERE revision.session_id = created.session_id
              AND revision.entry_ordinal = created.event_ordinal
              AND revision.event_kind = 'text_revised'
            ORDER BY revision.event_ordinal DESC
            LIMIT 1
       ), created.entry_text) AS current_text,
       COALESCE((
           SELECT movement.entry_status
             FROM session_plan_event AS movement
            WHERE movement.session_id = created.session_id
              AND movement.entry_ordinal = created.event_ordinal
              AND movement.event_kind = 'status_changed'
            ORDER BY movement.event_ordinal DESC
            LIMIT 1
       ), 'pending') AS current_status
  FROM session_plan_event AS created
 WHERE created.session_id = $1
   AND created.event_kind = 'created'
   AND ($2::numeric IS NULL OR created.event_ordinal > $2)
 ORDER BY created.event_ordinal
 LIMIT $3";

const HISTORY_SQL: &str = "SELECT event_ordinal, event_kind, entry_ordinal,
       entry_text, entry_status, provenance_turn_id,
       provenance_issuing_turn_attempt_id, provenance_request_id,
       provenance_attempt_id, provenance_dispatch_generation
  FROM session_plan_event
 WHERE session_id = $1
 ORDER BY event_ordinal
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
        }
    }
}

impl Error for SessionPlanCorruption {}

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
}

impl fmt::Display for SessionPlanRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database { source, .. } => {
                write!(formatter, "session plan database failure: {source}")
            }
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for SessionPlanRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for SessionPlanRepositoryError {
    fn from(source: sqlx::Error) -> Self {
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
        .bind(encoded.kind)
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
                let has_more = events.len() > limit;
                events.truncate(limit);
                Some(PlanHistoryPage::new(events, page_completeness(has_more)))
            }
            None => None,
        };
        let page = PlanReadPage::new(entries, page_completeness(has_more_entries), history);
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
    kind: &'static str,
    entry: PlanEntryId,
    text: Option<&'a str>,
    status: Option<&'static str>,
}

impl<'a> EncodedDraft<'a> {
    fn new(ordinal: PlanEventOrdinal, draft: &'a PlanEventDraft) -> Self {
        match draft {
            PlanEventDraft::Create { text } => Self {
                kind: "created",
                entry: PlanEntryId::from_creation_ordinal(ordinal),
                text: Some(text.as_str()),
                status: None,
            },
            PlanEventDraft::Revise { entry, text } => Self {
                kind: "text_revised",
                entry: *entry,
                text: Some(text.as_str()),
                status: None,
            },
            PlanEventDraft::SetStatus { entry, status } => Self {
                kind: "status_changed",
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
    let text: String = required(row, "current_text")?;
    let text = PlanText::try_new(text).map_err(|_| SessionPlanCorruption::InvalidText)?;
    let status_text: String = required(row, "current_status")?;
    let status = mapping::plan_status_from_str(&status_text).ok_or({
        SessionPlanCorruption::Unsupported {
            field: "current status",
            value: status_text,
        }
    })?;
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
    let entry_text: Option<String> = row.try_get("entry_text")?;
    let entry_status: Option<String> = row.try_get("entry_status")?;
    let kind = match event_kind.as_str() {
        "created" => {
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
        "text_revised" => {
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
        "status_changed" => {
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
        _ => {
            return Err(SessionPlanCorruption::Unsupported {
                field: "event kind",
                value: event_kind,
            }
            .into());
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
    let generation = positive_u64(
        row.try_get("provenance_dispatch_generation")?,
        "provenance dispatch generation",
    )?;
    let generation = ToolDispatchGeneration::try_from_u64(generation).ok_or(
        SessionPlanCorruption::InvalidPositiveInteger("provenance dispatch generation"),
    )?;
    Ok(PlanEventProvenance::from_invocation(
        ToolAttemptDispatchCorrelation::reconstitute(
            ToolAttemptDispatchCorrelationReconstitutionInput {
                session,
                turn: TurnId::from_uuid(row.try_get("provenance_turn_id")?),
                issuing_attempt: TurnAttemptId::from_uuid(
                    row.try_get("provenance_issuing_turn_attempt_id")?,
                ),
                request: ToolRequestId::from_uuid(row.try_get("provenance_request_id")?),
                attempt: ToolAttemptId::from_uuid(row.try_get("provenance_attempt_id")?),
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
