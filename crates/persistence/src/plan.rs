//! PostgreSQL storage for append-only session plan events.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    error::Error,
    fmt,
};

use rust_decimal::Decimal;
use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
use signalbox_domain::{
    SessionId, ToolAttemptDispatchCorrelation, ToolAttemptDispatchCorrelationReconstitutionInput,
    ToolAttemptId, ToolDispatchGeneration, ToolRequestId, TurnAttemptId, TurnId,
};
use signalbox_tools_plan::{
    MAX_PLAN_DEPENDENCIES_PER_ENTRY, PLAN_WRITE_NAME, PlanAppendOutcome, PlanAppendRejection,
    PlanAppendRequest, PlanDependencyCycle, PlanEntry, PlanEntryId, PlanEvent, PlanEventDraft,
    PlanEventKind, PlanEventOrdinal, PlanEventProvenance, PlanFoldError, PlanHistoryPage,
    PlanPageCompleteness, PlanReadPage, PlanReadRequest, PlanReadiness, PlanStatus, PlanText,
    SessionPlanPort, fold_plan_events,
};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

use crate::{commit_failure_is_ambiguous, lock_inventory, mapping};

const REPEATABLE_READ_ONLY: &str = "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY";

const CURRENT_PLAN_SQL: &str = "SELECT created.event_ordinal AS creation_event_ordinal,
       created.entry_ordinal AS entry_ordinal,
       created.dependency_ordinal AS created_dependency_ordinal,
       created.entry_text AS created_text,
       created.entry_status AS created_status,
       session_plan_event_has_authority(created) AS created_authorized,
       revision.event_ordinal AS revision_event_ordinal,
       revision.dependency_ordinal AS revision_dependency_ordinal,
       revision.entry_text AS revised_text,
       revision.entry_status AS revised_status,
       revision.authorized AS revision_authorized,
       movement.event_ordinal AS status_event_ordinal,
       movement.dependency_ordinal AS status_dependency_ordinal,
       movement.entry_text AS moved_text,
       movement.entry_status AS moved_status,
       movement.authorized AS movement_authorized
  FROM session_plan_event AS created
  LEFT JOIN LATERAL (
      SELECT event_ordinal, dependency_ordinal, entry_text, entry_status,
             session_plan_event_has_authority(candidate) AS authorized
        FROM session_plan_event AS candidate
       WHERE session_id = created.session_id
         AND entry_ordinal = created.event_ordinal
         AND event_kind = 'text_revised'
       ORDER BY event_ordinal DESC
       LIMIT 1
  ) AS revision ON TRUE
  LEFT JOIN LATERAL (
      SELECT event_ordinal, dependency_ordinal, entry_text, entry_status,
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

const CURRENT_DEPENDENCIES_SQL: &str = "WITH edge AS (
    SELECT candidate.entry_ordinal, candidate.dependency_ordinal,
           min(candidate.event_ordinal) AS first_event_ordinal,
           bool_and(session_plan_event_has_authority(candidate)) AS authorized,
           bool_and(
               candidate.dependency_ordinal IS NOT NULL
               AND candidate.entry_text IS NULL
               AND candidate.entry_status IS NULL
               AND candidate.event_ordinal > candidate.entry_ordinal
               AND candidate.event_ordinal > candidate.dependency_ordinal
               AND candidate.entry_ordinal <> candidate.dependency_ordinal
           ) AS payload_valid,
           row_number() OVER (
               PARTITION BY candidate.entry_ordinal
               ORDER BY min(candidate.event_ordinal)
           ) AS dependency_position
      FROM session_plan_event AS candidate
     WHERE candidate.session_id = $1
       AND candidate.event_kind = 'depends_on'
       AND candidate.entry_ordinal = ANY($2::numeric[])
     GROUP BY candidate.entry_ordinal, candidate.dependency_ordinal
)
SELECT edge.entry_ordinal, edge.dependency_ordinal,
       edge.first_event_ordinal, edge.authorized, edge.payload_valid,
       dependency.event_ordinal IS NOT NULL AS dependency_created,
       dependency.entry_ordinal AS dependency_entry_ordinal,
       dependency.dependency_ordinal AS dependency_payload_dependency_ordinal,
       dependency.entry_text AS dependency_created_text,
       dependency.entry_status AS dependency_created_status,
       session_plan_event_has_authority(dependency) AS dependency_authorized,
       movement.event_ordinal AS dependency_status_event_ordinal,
       movement.entry_status AS dependency_status,
       movement.authorized AS dependency_status_authorized,
       movement.dependency_ordinal AS dependency_status_dependency_ordinal,
       movement.entry_text AS dependency_status_text
  FROM edge
  LEFT JOIN session_plan_event AS dependency
    ON dependency.session_id = $1
   AND dependency.event_ordinal = edge.dependency_ordinal
   AND dependency.event_kind = 'created'
  LEFT JOIN LATERAL (
      SELECT candidate.event_ordinal, candidate.entry_status,
             candidate.dependency_ordinal, candidate.entry_text,
             session_plan_event_has_authority(candidate) AS authorized
        FROM session_plan_event AS candidate
       WHERE candidate.session_id = $1
         AND candidate.entry_ordinal = edge.dependency_ordinal
         AND candidate.event_kind = 'status_changed'
       ORDER BY candidate.event_ordinal DESC
       LIMIT 1
  ) AS movement ON TRUE
 WHERE edge.dependency_position <= $3
 ORDER BY edge.entry_ordinal, edge.first_event_ordinal";

const RELEVANT_DEPENDENCY_GRAPH_SQL: &str = "WITH RECURSIVE reachable_node(node) AS (
    SELECT unnest($2::numeric[])
    UNION
    SELECT current_edge.dependency_ordinal
      FROM reachable_node
      JOIN LATERAL (
          SELECT candidate.dependency_ordinal
            FROM session_plan_event AS candidate
           WHERE candidate.session_id = $1
             AND candidate.event_kind = 'depends_on'
             AND candidate.entry_ordinal = reachable_node.node
           GROUP BY candidate.dependency_ordinal
           ORDER BY min(candidate.event_ordinal)
           LIMIT $3
      ) AS current_edge ON TRUE
),
ranked_edge AS (
    SELECT candidate.entry_ordinal, candidate.dependency_ordinal,
           min(candidate.event_ordinal) AS first_event_ordinal,
           bool_and(session_plan_event_has_authority(candidate)) AS authorized,
           bool_and(
               candidate.dependency_ordinal IS NOT NULL
               AND candidate.entry_text IS NULL
               AND candidate.entry_status IS NULL
               AND candidate.event_ordinal > candidate.entry_ordinal
               AND candidate.event_ordinal > candidate.dependency_ordinal
               AND candidate.entry_ordinal <> candidate.dependency_ordinal
           ) AS payload_valid,
           row_number() OVER (
               PARTITION BY candidate.entry_ordinal
               ORDER BY min(candidate.event_ordinal)
           ) AS dependency_position
      FROM session_plan_event AS candidate
      JOIN reachable_node
        ON reachable_node.node = candidate.entry_ordinal
     WHERE candidate.session_id = $1
       AND candidate.event_kind = 'depends_on'
     GROUP BY candidate.entry_ordinal, candidate.dependency_ordinal
),
edge AS (
    SELECT entry_ordinal, dependency_ordinal,
           first_event_ordinal, authorized, payload_valid, dependency_position
      FROM ranked_edge
     WHERE dependency_position <= $3
)
SELECT edge.entry_ordinal, edge.dependency_ordinal,
       edge.first_event_ordinal, edge.dependency_position,
       edge.authorized AS edge_authorized,
       edge.payload_valid AS edge_payload_valid,
       entry.event_ordinal IS NOT NULL AS entry_created,
       coalesce(
           entry.entry_ordinal = entry.event_ordinal
           AND entry.dependency_ordinal IS NULL
           AND entry.entry_text IS NOT NULL
           AND char_length(entry.entry_text) BETWEEN 1 AND 4096
           AND entry.entry_status IS NULL,
           FALSE
       ) AS entry_payload_valid,
       session_plan_event_has_authority(entry) AS entry_authorized,
       dependency.event_ordinal IS NOT NULL AS dependency_created,
       coalesce(
           dependency.entry_ordinal = dependency.event_ordinal
           AND dependency.dependency_ordinal IS NULL
           AND dependency.entry_text IS NOT NULL
           AND char_length(dependency.entry_text) BETWEEN 1 AND 4096
           AND dependency.entry_status IS NULL,
           FALSE
       ) AS dependency_payload_valid,
       session_plan_event_has_authority(dependency) AS dependency_authorized
  FROM edge
  LEFT JOIN session_plan_event AS entry
    ON entry.session_id = $1
   AND entry.event_ordinal = edge.entry_ordinal
   AND entry.event_kind = 'created'
  LEFT JOIN session_plan_event AS dependency
    ON dependency.session_id = $1
   AND dependency.event_ordinal = edge.dependency_ordinal
   AND dependency.event_kind = 'created'
 ORDER BY edge.entry_ordinal, edge.first_event_ordinal";

const RELEVANT_DEPENDENCY_VALIDATION_SQL: &str = "WITH RECURSIVE reachable_node(node) AS (
    SELECT unnest($2::numeric[])
    UNION
    SELECT current_edge.dependency_ordinal
      FROM reachable_node
      JOIN LATERAL (
          SELECT candidate.dependency_ordinal
            FROM session_plan_event AS candidate
           WHERE candidate.session_id = $1
             AND candidate.event_kind = 'depends_on'
             AND candidate.entry_ordinal = reachable_node.node
           GROUP BY candidate.dependency_ordinal
           ORDER BY min(candidate.event_ordinal)
           LIMIT $3
      ) AS current_edge ON TRUE
),
ranked_edge AS (
    SELECT candidate.entry_ordinal, candidate.dependency_ordinal,
           min(candidate.event_ordinal) AS first_event_ordinal,
           bool_and(session_plan_event_has_authority(candidate)) AS authorized,
           bool_and(
               candidate.dependency_ordinal IS NOT NULL
               AND candidate.entry_text IS NULL
               AND candidate.entry_status IS NULL
               AND candidate.event_ordinal > candidate.entry_ordinal
               AND candidate.event_ordinal > candidate.dependency_ordinal
               AND candidate.entry_ordinal <> candidate.dependency_ordinal
           ) AS payload_valid,
           row_number() OVER (
               PARTITION BY candidate.entry_ordinal
               ORDER BY min(candidate.event_ordinal)
           ) AS dependency_position
      FROM session_plan_event AS candidate
      JOIN reachable_node
        ON reachable_node.node = candidate.entry_ordinal
     WHERE candidate.session_id = $1
       AND candidate.event_kind = 'depends_on'
     GROUP BY candidate.entry_ordinal, candidate.dependency_ordinal
),
edge AS (
    SELECT entry_ordinal, dependency_ordinal, first_event_ordinal,
           authorized, payload_valid, dependency_position
      FROM ranked_edge
     WHERE dependency_position <= $3
),
relevant_edge AS (
    SELECT edge.entry_ordinal, edge.dependency_ordinal,
           edge.first_event_ordinal, edge.dependency_position,
           edge.authorized AS edge_authorized,
           edge.payload_valid AS edge_payload_valid,
           entry.event_ordinal IS NOT NULL AS entry_created,
           coalesce(
               entry.entry_ordinal = entry.event_ordinal
               AND entry.dependency_ordinal IS NULL
               AND entry.entry_text IS NOT NULL
               AND char_length(entry.entry_text) BETWEEN 1 AND 4096
               AND entry.entry_status IS NULL,
               FALSE
           ) AS entry_payload_valid,
           session_plan_event_has_authority(entry) AS entry_authorized,
           dependency.event_ordinal IS NOT NULL AS dependency_created,
           coalesce(
               dependency.entry_ordinal = dependency.event_ordinal
               AND dependency.dependency_ordinal IS NULL
               AND dependency.entry_text IS NOT NULL
               AND char_length(dependency.entry_text) BETWEEN 1 AND 4096
               AND dependency.entry_status IS NULL,
               FALSE
           ) AS dependency_payload_valid,
           session_plan_event_has_authority(dependency) AS dependency_authorized
      FROM edge
      LEFT JOIN session_plan_event AS entry
        ON entry.session_id = $1
       AND entry.event_ordinal = edge.entry_ordinal
       AND entry.event_kind = 'created'
      LEFT JOIN session_plan_event AS dependency
        ON dependency.session_id = $1
       AND dependency.event_ordinal = edge.dependency_ordinal
       AND dependency.event_kind = 'created'
),
walk(origin, node) AS (
    SELECT edge.entry_ordinal, edge.dependency_ordinal
      FROM edge
    UNION
    SELECT walk.origin, edge.dependency_ordinal
      FROM walk
      JOIN edge
        ON edge.entry_ordinal = walk.node
)
SELECT EXISTS (
           SELECT 1
             FROM relevant_edge
            WHERE dependency_position > $4
               OR NOT edge_payload_valid
               OR NOT entry_created
               OR NOT entry_payload_valid
               OR NOT dependency_created
               OR NOT dependency_payload_valid
               OR first_event_ordinal <= entry_ordinal
               OR first_event_ordinal <= dependency_ordinal
               OR entry_ordinal = dependency_ordinal
       ) AS invalid_sequence,
       EXISTS (
           SELECT 1
             FROM relevant_edge
            WHERE NOT edge_authorized
               OR NOT entry_authorized
               OR NOT dependency_authorized
       ) AS untrusted,
       EXISTS (
           SELECT 1
             FROM walk
            WHERE origin = node
       ) AS cyclic";

const DEPENDENCY_LIMIT_REACHED_SQL: &str = "SELECT
    NOT EXISTS (
        SELECT 1
          FROM session_plan_event AS edge
         WHERE edge.session_id = $1
           AND edge.event_kind = 'depends_on'
           AND edge.entry_ordinal = $2
           AND edge.dependency_ordinal = $3
    )
    AND (
        SELECT count(*)
          FROM (
              SELECT edge.dependency_ordinal
                FROM session_plan_event AS edge
               WHERE edge.session_id = $1
                 AND edge.event_kind = 'depends_on'
                 AND edge.entry_ordinal = $2
               GROUP BY edge.dependency_ordinal
          ) AS current_dependency
    ) >= $4";

const UNSUPPORTED_EVENT_KIND_SQL: &str = "SELECT event_kind
  FROM session_plan_event
 WHERE session_id = $1
   AND (
        event_kind IS NULL
        OR event_kind NOT IN ('created', 'text_revised', 'status_changed', 'depends_on')
   )
 LIMIT 1";

const INVALID_EVENT_SEQUENCE_SQL: &str = "SELECT CASE
           WHEN head.row_present IS NULL THEN latest.row_present IS NOT NULL
           ELSE head.event_ordinal IS NULL
                OR latest.event_ordinal IS DISTINCT FROM head.event_ordinal
                OR certified.event_ordinal IS NULL
                OR NOT session_plan_event_has_authority(certified)
       END
  FROM (VALUES (1)) AS singleton(marker)
  LEFT JOIN LATERAL (
      SELECT session_id, event_ordinal, TRUE AS row_present
        FROM session_plan_head
       WHERE session_id = $1
  ) AS head ON TRUE
  LEFT JOIN LATERAL (
      SELECT event_ordinal, TRUE AS row_present
        FROM session_plan_event
       WHERE session_id = $1
       ORDER BY event_ordinal DESC
       LIMIT 1
  ) AS latest ON TRUE
  LEFT JOIN session_plan_event AS certified
    ON certified.session_id = head.session_id
   AND certified.event_ordinal = head.event_ordinal";

const HISTORY_SQL: &str = "SELECT event.event_ordinal, event.event_kind,
       event.entry_ordinal, event.dependency_ordinal,
       event.entry_text, event.entry_status,
       event.provenance_turn_id, event.provenance_issuing_turn_attempt_id,
       event.provenance_request_id, event.provenance_attempt_id,
       event.provenance_dispatch_generation,
       session_plan_event_has_authority(event) AS event_authorized,
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
    /// One physical plan-write attempt was submitted more than once.
    DuplicateAppendAttempt,
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
            Self::DuplicateAppendAttempt => {
                formatter.write_str("session plan append attempt was already used")
            }
        }
    }
}

impl Error for SessionPlanRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::Corruption(error) => Some(error),
            Self::InvalidAppendProvenance | Self::DuplicateAppendAttempt => None,
        }
    }
}

impl From<sqlx::Error> for SessionPlanRepositoryError {
    fn from(source: sqlx::Error) -> Self {
        let constraint = source
            .as_database_error()
            .and_then(|database| database.constraint());
        if constraint == Some("session_plan_event_requires_active_plan_write_attempt") {
            return Self::InvalidAppendProvenance;
        }
        if constraint == Some("session_plan_event_provenance_attempt_id_key") {
            return Self::DuplicateAppendAttempt;
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
            Self::InvalidAppendProvenance | Self::DuplicateAppendAttempt => {
                OperatorFailureClass::CallerOrHubBug
            }
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
        let next: Option<Decimal> =
            sqlx::query_scalar("SELECT next_session_plan_event_ordinal($1)")
                .bind(request.session().into_uuid())
                .fetch_one(&mut *transaction)
                .await?;
        let Some(next) = next else {
            transaction.rollback().await?;
            return Err(SessionPlanRepositoryError::InvalidAppendProvenance);
        };
        let next = PlanEventOrdinal::try_from_u64(positive_u64(next, "next event ordinal")?)
            .ok_or(SessionPlanCorruption::InvalidPositiveInteger(
                "next event ordinal",
            ))?;
        let encoded = EncodedDraft::new(next, request.draft());
        let correlation = request.provenance().correlation();
        let authorized: Option<Uuid> = sqlx::query_scalar(lock_inventory::PLAN_APPEND_ATTEMPT)
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
            .bind(
                encoded
                    .dependency
                    .map(|entry| Decimal::from(entry.as_u64())),
            )
            .fetch_optional(&mut *transaction)
            .await?;
        if authorized.is_none() {
            transaction.rollback().await?;
            return Err(SessionPlanRepositoryError::InvalidAppendProvenance);
        }

        for entry in draft_targets(request.draft()) {
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

        if let PlanEventDraft::DependsOn { entry, dependency } = request.draft() {
            if let Some(cycle) =
                find_dependency_cycle(&mut transaction, request.session(), *entry, *dependency)
                    .await?
            {
                transaction.rollback().await?;
                return Ok(PlanAppendOutcome::Rejected(
                    PlanAppendRejection::DependencyCycle(cycle),
                ));
            }
            let limit_reached: bool = sqlx::query_scalar(DEPENDENCY_LIMIT_REACHED_SQL)
                .bind(request.session().into_uuid())
                .bind(Decimal::from(entry.as_u64()))
                .bind(Decimal::from(dependency.as_u64()))
                .bind(dependency_capacity()?)
                .fetch_one(&mut *transaction)
                .await?;
            if limit_reached {
                transaction.rollback().await?;
                return Ok(PlanAppendOutcome::Rejected(
                    PlanAppendRejection::DependencyLimitReached { entry: *entry },
                ));
            }
        }

        let prior = next
            .as_u64()
            .checked_sub(1)
            .filter(|prior| *prior > 0)
            .map(Decimal::from);
        sqlx::query(
            "INSERT INTO session_plan_event
                (session_id, event_ordinal, prior_event_ordinal,
                 event_kind, entry_ordinal, dependency_ordinal,
                 entry_text, entry_status, provenance_turn_id,
                 provenance_issuing_turn_attempt_id, provenance_request_id,
                 provenance_attempt_id, provenance_dispatch_generation)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(request.session().into_uuid())
        .bind(Decimal::from(next.as_u64()))
        .bind(prior)
        .bind(mapping::plan_event_kind_to_str(encoded.kind))
        .bind(Decimal::from(encoded.entry.as_u64()))
        .bind(
            encoded
                .dependency
                .map(|entry| Decimal::from(entry.as_u64())),
        )
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
        let unsupported_kind: Option<Option<String>> =
            sqlx::query_scalar(UNSUPPORTED_EVENT_KIND_SQL)
                .bind(request.session().into_uuid())
                .fetch_optional(&mut *transaction)
                .await?;
        if let Some(value) = unsupported_kind {
            let value = value.ok_or(SessionPlanCorruption::Missing("event kind"))?;
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
        let entry_ordinals = entries
            .iter()
            .map(|entry| Decimal::from(entry.id().as_u64()))
            .collect::<Vec<_>>();
        let dependency_query_limit = dependency_query_limit()?;
        let dependency_rows = if entry_ordinals.is_empty() {
            Vec::new()
        } else {
            validate_relevant_dependency_graph(
                &mut transaction,
                request.session(),
                &entries.iter().map(PlanEntry::id).collect::<Vec<_>>(),
            )
            .await?;
            sqlx::query(CURRENT_DEPENDENCIES_SQL)
                .bind(request.session().into_uuid())
                .bind(&entry_ordinals)
                .bind(dependency_query_limit)
                .fetch_all(&mut *transaction)
                .await?
        };
        entries = apply_dependency_rows(entries, &dependency_rows)?;

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
    dependency: Option<PlanEntryId>,
    text: Option<&'a str>,
    status: Option<&'static str>,
}

impl<'a> EncodedDraft<'a> {
    fn new(ordinal: PlanEventOrdinal, draft: &'a PlanEventDraft) -> Self {
        match draft {
            PlanEventDraft::Create { text } => Self {
                kind: mapping::PlanEventStorageKind::Created,
                entry: PlanEntryId::from_creation_ordinal(ordinal),
                dependency: None,
                text: Some(text.as_str()),
                status: None,
            },
            PlanEventDraft::Revise { entry, text } => Self {
                kind: mapping::PlanEventStorageKind::TextRevised,
                entry: *entry,
                dependency: None,
                text: Some(text.as_str()),
                status: None,
            },
            PlanEventDraft::SetStatus { entry, status } => Self {
                kind: mapping::PlanEventStorageKind::StatusChanged,
                entry: *entry,
                dependency: None,
                text: None,
                status: Some(mapping::plan_status_to_str(*status)),
            },
            PlanEventDraft::DependsOn { entry, dependency } => Self {
                kind: mapping::PlanEventStorageKind::DependsOn,
                entry: *entry,
                dependency: Some(*dependency),
                text: None,
                status: None,
            },
        }
    }
}

fn draft_targets(draft: &PlanEventDraft) -> Vec<PlanEntryId> {
    match draft {
        PlanEventDraft::Create { .. } => Vec::new(),
        PlanEventDraft::Revise { entry, .. } | PlanEventDraft::SetStatus { entry, .. } => {
            vec![*entry]
        }
        PlanEventDraft::DependsOn { entry, dependency } => vec![*entry, *dependency],
    }
}

fn event_kind_from_draft(draft: PlanEventDraft) -> PlanEventKind {
    match draft {
        PlanEventDraft::Create { text } => PlanEventKind::Created { text },
        PlanEventDraft::Revise { entry, text } => PlanEventKind::TextRevised { entry, text },
        PlanEventDraft::SetStatus { entry, status } => {
            PlanEventKind::StatusChanged { entry, status }
        }
        PlanEventDraft::DependsOn { entry, dependency } => {
            PlanEventKind::DependsOn { entry, dependency }
        }
    }
}

fn dependency_capacity() -> Result<i64, SessionPlanRepositoryError> {
    i64::try_from(MAX_PLAN_DEPENDENCIES_PER_ENTRY)
        .map_err(|_| SessionPlanCorruption::InvalidPositiveInteger("dependency limit").into())
}

fn dependency_query_limit() -> Result<i64, SessionPlanRepositoryError> {
    MAX_PLAN_DEPENDENCIES_PER_ENTRY
        .checked_add(1)
        .and_then(|limit| i64::try_from(limit).ok())
        .ok_or(SessionPlanCorruption::InvalidPositiveInteger("dependency query limit").into())
}

async fn validate_relevant_dependency_graph(
    transaction: &mut Transaction<'_, Postgres>,
    session: SessionId,
    roots: &[PlanEntryId],
) -> Result<(), SessionPlanRepositoryError> {
    let root_ordinals = roots
        .iter()
        .map(|root| Decimal::from(root.as_u64()))
        .collect::<Vec<_>>();
    let row = sqlx::query(RELEVANT_DEPENDENCY_VALIDATION_SQL)
        .bind(session.into_uuid())
        .bind(&root_ordinals)
        .bind(dependency_query_limit()?)
        .bind(dependency_capacity()?)
        .fetch_one(&mut **transaction)
        .await?;
    if required::<bool>(&row, "invalid_sequence")? || required::<bool>(&row, "cyclic")? {
        return Err(SessionPlanCorruption::InvalidEventSequence.into());
    }
    if required::<bool>(&row, "untrusted")? {
        return Err(SessionPlanCorruption::UntrustedProvenance.into());
    }
    Ok(())
}

async fn find_dependency_cycle(
    transaction: &mut Transaction<'_, Postgres>,
    session: SessionId,
    entry: PlanEntryId,
    dependency: PlanEntryId,
) -> Result<Option<PlanDependencyCycle>, SessionPlanRepositoryError> {
    let graph = load_relevant_dependency_graph(transaction, session, &[dependency]).await?;
    let mut queued = VecDeque::from([dependency]);
    let mut visited = HashSet::from([dependency]);
    let mut parents = HashMap::<PlanEntryId, PlanEntryId>::new();
    while let Some(current) = queued.pop_front() {
        if current == entry {
            let mut tail = vec![entry];
            let mut cursor = entry;
            while cursor != dependency {
                cursor = *parents
                    .get(&cursor)
                    .ok_or(SessionPlanCorruption::InvalidEventSequence)?;
                tail.push(cursor);
            }
            tail.reverse();
            let mut path = vec![entry];
            path.extend(tail);
            let cycle = PlanDependencyCycle::try_new(entry, dependency, path)
                .ok_or(SessionPlanCorruption::InvalidEventSequence)?;
            return Ok(Some(cycle));
        }
        for next in graph.get(&current).into_iter().flatten().copied() {
            if visited.insert(next) {
                parents.insert(next, current);
                queued.push_back(next);
            }
        }
    }
    Ok(None)
}

async fn load_relevant_dependency_graph(
    transaction: &mut Transaction<'_, Postgres>,
    session: SessionId,
    roots: &[PlanEntryId],
) -> Result<HashMap<PlanEntryId, Vec<PlanEntryId>>, SessionPlanRepositoryError> {
    let root_ordinals = roots
        .iter()
        .map(|root| Decimal::from(root.as_u64()))
        .collect::<Vec<_>>();
    let rows = sqlx::query(RELEVANT_DEPENDENCY_GRAPH_SQL)
        .bind(session.into_uuid())
        .bind(&root_ordinals)
        .bind(dependency_query_limit()?)
        .fetch_all(&mut **transaction)
        .await?;
    let capacity = dependency_capacity()?;
    let mut ordered_graph = HashMap::<PlanEntryId, Vec<(PlanEventOrdinal, PlanEntryId)>>::new();
    for row in &rows {
        let position: i64 = required(row, "dependency_position")?;
        if position > capacity
            || !required::<bool>(row, "edge_payload_valid")?
            || !required::<bool>(row, "entry_created")?
            || !required::<bool>(row, "entry_payload_valid")?
            || !required::<bool>(row, "dependency_created")?
            || !required::<bool>(row, "dependency_payload_valid")?
        {
            return Err(SessionPlanCorruption::InvalidEventSequence.into());
        }
        if !required::<bool>(row, "edge_authorized")?
            || !required::<bool>(row, "entry_authorized")?
            || !required::<bool>(row, "dependency_authorized")?
        {
            return Err(SessionPlanCorruption::UntrustedProvenance.into());
        }
        let entry = dependency_path_entry(required(row, "entry_ordinal")?)?;
        let dependency = dependency_path_entry(required(row, "dependency_ordinal")?)?;
        let first_event = PlanEventOrdinal::try_from_u64(positive_u64(
            required(row, "first_event_ordinal")?,
            "dependency event ordinal",
        )?)
        .ok_or(SessionPlanCorruption::InvalidPositiveInteger(
            "dependency event ordinal",
        ))?;
        if first_event <= entry.creation_ordinal()
            || first_event <= dependency.creation_ordinal()
            || entry == dependency
        {
            return Err(SessionPlanCorruption::InvalidEventSequence.into());
        }
        ordered_graph
            .entry(entry)
            .or_default()
            .push((first_event, dependency));
    }
    Ok(ordered_graph
        .into_iter()
        .map(|(entry, mut dependencies)| {
            dependencies.sort_by_key(|(ordinal, _dependency)| *ordinal);
            (
                entry,
                dependencies
                    .into_iter()
                    .map(|(_ordinal, dependency)| dependency)
                    .collect(),
            )
        })
        .collect())
}

fn dependency_path_entry(value: Decimal) -> Result<PlanEntryId, SessionPlanRepositoryError> {
    PlanEntryId::try_from_u64(positive_u64(value, "dependency cycle path")?)
        .ok_or(SessionPlanCorruption::InvalidPositiveInteger("dependency cycle path").into())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProjectedAuthority {
    Trusted,
    Untrusted,
}

fn decode_entry(row: &PgRow) -> Result<PlanEntry, SessionPlanRepositoryError> {
    let entry = PlanEntryId::try_from_u64(positive_u64(
        required(row, "entry_ordinal")?,
        "entry ordinal",
    )?)
    .ok_or(SessionPlanCorruption::InvalidPositiveInteger(
        "entry ordinal",
    ))?;
    let creation_ordinal = PlanEventOrdinal::try_from_u64(positive_u64(
        required(row, "creation_event_ordinal")?,
        "creation event ordinal",
    )?)
    .ok_or(SessionPlanCorruption::InvalidPositiveInteger(
        "creation event ordinal",
    ))?;
    if entry.creation_ordinal() != creation_ordinal {
        return Err(SessionPlanCorruption::MismatchedIdentity("current entry identity").into());
    }
    let created_authority = required_projected_authority(row, "created_authorized")?;
    if created_authority == ProjectedAuthority::Untrusted {
        return Err(SessionPlanCorruption::UntrustedProvenance.into());
    }
    let created_dependency: Option<Decimal> = row.try_get("created_dependency_ordinal")?;
    let created_text: String = required(row, "created_text")?;
    let created_status: Option<String> = row.try_get("created_status")?;
    if created_dependency.is_some() || created_status.is_some() {
        return Err(SessionPlanCorruption::InvalidEventPayload("current creation").into());
    }
    let revision_ordinal: Option<Decimal> = row.try_get("revision_event_ordinal")?;
    let revision_dependency: Option<Decimal> = row.try_get("revision_dependency_ordinal")?;
    let revised_text: Option<String> = row.try_get("revised_text")?;
    let revised_status: Option<String> = row.try_get("revised_status")?;
    let revision_authority = optional_projected_authority(row, "revision_authorized")?;
    let text = match (
        revision_ordinal,
        revision_dependency,
        revised_text,
        revised_status,
        revision_authority,
    ) {
        (None, None, None, None, None) => created_text,
        (
            Some(revision_ordinal),
            None,
            Some(revised_text),
            None,
            Some(ProjectedAuthority::Trusted),
        ) => {
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
        (Some(_), None, Some(_), None, Some(ProjectedAuthority::Untrusted)) => {
            return Err(SessionPlanCorruption::UntrustedProvenance.into());
        }
        _ => {
            return Err(SessionPlanCorruption::InvalidEventPayload("current text revision").into());
        }
    };
    let text = PlanText::try_new(text).map_err(|_| SessionPlanCorruption::InvalidText)?;

    let status_ordinal: Option<Decimal> = row.try_get("status_event_ordinal")?;
    let status_dependency: Option<Decimal> = row.try_get("status_dependency_ordinal")?;
    let moved_text: Option<String> = row.try_get("moved_text")?;
    let moved_status: Option<String> = row.try_get("moved_status")?;
    let movement_authority = optional_projected_authority(row, "movement_authorized")?;
    let status = match (
        status_ordinal,
        status_dependency,
        moved_text,
        moved_status,
        movement_authority,
    ) {
        (None, None, None, None, None) => PlanStatus::Pending,
        (
            Some(status_ordinal),
            None,
            None,
            Some(moved_status),
            Some(ProjectedAuthority::Trusted),
        ) => {
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
        (Some(_), None, None, Some(_), Some(ProjectedAuthority::Untrusted)) => {
            return Err(SessionPlanCorruption::UntrustedProvenance.into());
        }
        _ => {
            return Err(SessionPlanCorruption::InvalidEventPayload("current status change").into());
        }
    };
    Ok(PlanEntry::new(entry, text, status))
}

fn apply_dependency_rows(
    entries: Vec<PlanEntry>,
    rows: &[PgRow],
) -> Result<Vec<PlanEntry>, SessionPlanRepositoryError> {
    let mut projected = HashMap::<PlanEntryId, (Vec<PlanEntryId>, bool)>::new();
    for row in rows {
        let entry = dependency_path_entry(required(row, "entry_ordinal")?)?;
        let dependency = dependency_path_entry(required(row, "dependency_ordinal")?)?;
        let first_event = PlanEventOrdinal::try_from_u64(positive_u64(
            required(row, "first_event_ordinal")?,
            "dependency event ordinal",
        )?)
        .ok_or(SessionPlanCorruption::InvalidPositiveInteger(
            "dependency event ordinal",
        ))?;
        if entry == dependency
            || entry.creation_ordinal() >= first_event
            || dependency.creation_ordinal() >= first_event
            || !required::<bool>(row, "payload_valid")?
            || !required::<bool>(row, "dependency_created")?
        {
            return Err(SessionPlanCorruption::InvalidEventSequence.into());
        }
        let dependency_entry: Option<Decimal> = row.try_get("dependency_entry_ordinal")?;
        let dependency_payload_dependency: Option<Decimal> =
            row.try_get("dependency_payload_dependency_ordinal")?;
        let dependency_text: Option<String> = row.try_get("dependency_created_text")?;
        let dependency_status: Option<String> = row.try_get("dependency_created_status")?;
        if dependency_entry != Some(Decimal::from(dependency.as_u64()))
            || dependency_payload_dependency.is_some()
            || dependency_status.is_some()
        {
            return Err(SessionPlanCorruption::InvalidEventPayload("dependency creation").into());
        }
        let dependency_text = dependency_text.ok_or(SessionPlanCorruption::InvalidEventPayload(
            "dependency creation",
        ))?;
        PlanText::try_new(dependency_text).map_err(|_| SessionPlanCorruption::InvalidText)?;
        if required_projected_authority(row, "authorized")? != ProjectedAuthority::Trusted
            || required_projected_authority(row, "dependency_authorized")?
                != ProjectedAuthority::Trusted
        {
            return Err(SessionPlanCorruption::UntrustedProvenance.into());
        }

        let status_event: Option<Decimal> = row.try_get("dependency_status_event_ordinal")?;
        let status: Option<String> = row.try_get("dependency_status")?;
        let status_authority = optional_projected_authority(row, "dependency_status_authorized")?;
        let status_dependency: Option<Decimal> =
            row.try_get("dependency_status_dependency_ordinal")?;
        let status_text: Option<String> = row.try_get("dependency_status_text")?;
        let status = match (
            status_event,
            status,
            status_authority,
            status_dependency,
            status_text,
        ) {
            (None, None, None, None, None) => PlanStatus::Pending,
            (Some(status_event), Some(status), Some(ProjectedAuthority::Trusted), None, None) => {
                let status_event = PlanEventOrdinal::try_from_u64(positive_u64(
                    status_event,
                    "dependency status event ordinal",
                )?)
                .ok_or(SessionPlanCorruption::InvalidPositiveInteger(
                    "dependency status event ordinal",
                ))?;
                if status_event <= dependency.creation_ordinal() {
                    return Err(SessionPlanCorruption::InvalidEventSequence.into());
                }
                mapping::plan_status_from_str(&status).ok_or({
                    SessionPlanCorruption::Unsupported {
                        field: "dependency status",
                        value: status,
                    }
                })?
            }
            (Some(_), Some(_), Some(ProjectedAuthority::Untrusted), _, _) => {
                return Err(SessionPlanCorruption::UntrustedProvenance.into());
            }
            _ => {
                return Err(SessionPlanCorruption::InvalidEventPayload("dependency status").into());
            }
        };
        if !entries.iter().any(|candidate| candidate.id() == entry) {
            return Err(SessionPlanCorruption::InvalidEventSequence.into());
        }
        let (dependencies, waiting) = projected.entry(entry).or_default();
        if dependencies.contains(&dependency) {
            return Err(SessionPlanCorruption::InvalidEventSequence.into());
        }
        if dependencies.len() >= MAX_PLAN_DEPENDENCIES_PER_ENTRY {
            return Err(SessionPlanCorruption::InvalidEventSequence.into());
        }
        dependencies.push(dependency);
        *waiting |= status != PlanStatus::Completed;
    }

    let mut result = Vec::with_capacity(entries.len());
    for entry in entries {
        let (dependencies, waiting) = projected.remove(&entry.id()).unwrap_or_default();
        let readiness = if waiting {
            PlanReadiness::Waiting
        } else {
            PlanReadiness::Ready
        };
        result.push(PlanEntry::with_dependencies(
            entry.id(),
            entry.text().clone(),
            entry.status(),
            dependencies,
            readiness,
        ));
    }
    if !projected.is_empty() {
        return Err(SessionPlanCorruption::InvalidEventSequence.into());
    }
    Ok(result)
}

fn decode_event(session: SessionId, row: &PgRow) -> Result<PlanEvent, SessionPlanRepositoryError> {
    let ordinal = PlanEventOrdinal::try_from_u64(positive_u64(
        required(row, "event_ordinal")?,
        "event ordinal",
    )?)
    .ok_or(SessionPlanCorruption::InvalidPositiveInteger(
        "event ordinal",
    ))?;
    let entry = PlanEntryId::try_from_u64(positive_u64(
        required(row, "entry_ordinal")?,
        "entry ordinal",
    )?)
    .ok_or(SessionPlanCorruption::InvalidPositiveInteger(
        "entry ordinal",
    ))?;
    if required_projected_authority(row, "event_authorized")? != ProjectedAuthority::Trusted {
        return Err(SessionPlanCorruption::UntrustedProvenance.into());
    }
    let provenance = decode_provenance(session, row)?;
    let event_kind: String = required(row, "event_kind")?;
    let storage_kind = mapping::plan_event_kind_from_str(&event_kind).ok_or(
        SessionPlanCorruption::Unsupported {
            field: "event kind",
            value: event_kind,
        },
    )?;
    let dependency_ordinal: Option<Decimal> = row.try_get("dependency_ordinal")?;
    let entry_text: Option<String> = row.try_get("entry_text")?;
    let entry_status: Option<String> = row.try_get("entry_status")?;
    let kind = match storage_kind {
        mapping::PlanEventStorageKind::Created => {
            let (None, Some(value), None) = (dependency_ordinal, entry_text, entry_status) else {
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
            let (None, Some(value), None) = (dependency_ordinal, entry_text, entry_status) else {
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
            let (None, None, Some(value)) = (dependency_ordinal, entry_text, entry_status) else {
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
        mapping::PlanEventStorageKind::DependsOn => {
            let (Some(value), None, None) = (dependency_ordinal, entry_text, entry_status) else {
                return Err(SessionPlanCorruption::InvalidEventPayload("depends-on event").into());
            };
            let dependency = dependency_path_entry(value)?;
            if entry.creation_ordinal() >= ordinal || dependency.creation_ordinal() >= ordinal {
                return Err(SessionPlanCorruption::MismatchedIdentity(
                    "depends-on target ordering",
                )
                .into());
            }
            PlanEventKind::DependsOn { entry, dependency }
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
        PlanEventKind::DependsOn { entry, dependency } => serde_json::json!({
            "kind": "depends_on",
            "entry_id": entry.as_u64(),
            "dependency_id": dependency.as_u64(),
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
    let turn: Uuid = required(row, "provenance_turn_id")?;
    let issuing_attempt: Uuid = required(row, "provenance_issuing_turn_attempt_id")?;
    let request: Uuid = required(row, "provenance_request_id")?;
    let attempt: Uuid = required(row, "provenance_attempt_id")?;
    let generation_value: Decimal = required(row, "provenance_dispatch_generation")?;
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
    mapping::positive_u64_from_numeric(value)
        .map_err(|_| SessionPlanCorruption::InvalidPositiveInteger(field))
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, SessionPlanRepositoryError>
where
    for<'decode> T: sqlx::Decode<'decode, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or(SessionPlanCorruption::Missing(field).into())
}

fn required_projected_authority(
    row: &PgRow,
    field: &'static str,
) -> Result<ProjectedAuthority, SessionPlanRepositoryError> {
    optional_projected_authority(row, field)?.ok_or(SessionPlanCorruption::Missing(field).into())
}

fn optional_projected_authority(
    row: &PgRow,
    field: &'static str,
) -> Result<Option<ProjectedAuthority>, sqlx::Error> {
    row.try_get::<Option<bool>, _>(field).map(|authority| {
        authority.map(|authority| {
            if authority {
                ProjectedAuthority::Trusted
            } else {
                ProjectedAuthority::Untrusted
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_PLAN_DEPENDENCIES_PER_ENTRY, RELEVANT_DEPENDENCY_GRAPH_SQL,
        RELEVANT_DEPENDENCY_VALIDATION_SQL,
    };

    const SESSION_PLAN_MIGRATION: &str =
        include_str!("../migrations/202608020011_session_plan.sql");

    #[test]
    fn dependency_validation_deduplicates_transitive_origin_node_pairs() {
        assert!(RELEVANT_DEPENDENCY_VALIDATION_SQL.contains("walk(origin, node) AS"));
        assert!(
            RELEVANT_DEPENDENCY_VALIDATION_SQL
                .contains("    UNION\n    SELECT walk.origin, edge.dependency_ordinal")
        );
        assert!(!RELEVANT_DEPENDENCY_VALIDATION_SQL.contains("UNION ALL"));
    }

    #[test]
    fn dependency_queries_seed_edge_ranking_from_reachable_roots() {
        const ROOT_SEEDED_EDGE_JOIN: &str = "FROM session_plan_event AS candidate
      JOIN reachable_node
        ON reachable_node.node = candidate.entry_ordinal";

        assert!(RELEVANT_DEPENDENCY_GRAPH_SQL.contains(ROOT_SEEDED_EDGE_JOIN));
        assert!(RELEVANT_DEPENDENCY_VALIDATION_SQL.contains(ROOT_SEEDED_EDGE_JOIN));
    }

    #[test]
    fn migration_dependency_limit_matches_the_tools_plan_constant() {
        let expected_guard =
            format!("IF dependency_count >= {MAX_PLAN_DEPENDENCIES_PER_ENTRY} THEN");

        assert!(SESSION_PLAN_MIGRATION.contains(&expected_guard));
    }
}
