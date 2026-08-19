//! Durable evidence and terminalization for boundedly stale active turns.
//!
//! The inventory query is the whole in-flight predicate: a turn reaches the
//! result only when every durable shape that could mean live work is absent.
//! Terminalization then re-runs that same query for the one session under the
//! scheduler lock and commits the shared failed-turn transition
//! ([`crate::startup`]) rather than editing lifecycle rows here, so a stale
//! turn ends exactly as a recovered one does and every trigger that watches
//! for a terminal turn — repository-watch dispatch release included — fires
//! without this module naming any of them.

use std::{error::Error, fmt};

use signalbox_application::{
    ClassifyOperatorFailure, OperatorFailureClass, StaleTurnCandidate, StaleTurnOutcome,
    TurnLivenessEvidence,
};
use signalbox_domain::{
    AcceptedInputTurnFailureFailure, AcceptedInputTurnFailureIdentities, ModelCallId,
    SemanticTranscriptEntryId, SessionId, TurnAttemptId,
};
use sqlx::{PgConnection, PgPool, Row, types::Uuid};

use crate::mapping::{
    session_id_from_uuid, session_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid,
};
use crate::session::{SessionRepositoryError, load_session_from_connection};
use crate::startup::{
    StartupScanCorruption, StartupScanIdentityCollision, StartupScanRepositoryError,
    insert_prepared_failure, map_scheduling_error,
};
use crate::submit_input::load_scheduling_projection;

/// How many quiescent turns one inventory read returns.
///
/// This bounds the size of one statement's result, not how much of the
/// population a scan reaches: the caller drains its whole rotation before
/// deciding anything, so every turn is observed on every scan and the staleness
/// bound holds whatever the population is.
const QUIESCENT_INVENTORY_PAGE_SIZE: i64 = 256;

/// Infrastructure or integrity failure while supervising turn liveness.
///
/// The two database arms are separate because they mean different things to an
/// operator: a failed inventory read is a pass that made no decision at all,
/// while a failed terminalization is a decision that could not be carried out
/// on a turn already judged stale. No blanket `From<sqlx::Error>` exists, so
/// neither path can silently borrow the other's cause code.
#[derive(Debug)]
pub enum TurnLivenessRepositoryError {
    /// Reading the quiescent active-turn inventory failed.
    Inventory(sqlx::Error),
    /// A database operation on the terminalization path failed.
    TerminalizationDatabase {
        /// Whether the failure leaves the commit's outcome unknown.
        commit_ambiguous: bool,
        /// The originating driver failure.
        source: sqlx::Error,
    },
    /// The shared failed-turn transition could not be committed.
    Terminalization(StartupScanRepositoryError),
}

impl TurnLivenessRepositoryError {
    /// Classifies an unambiguous driver failure on the terminalization path.
    fn terminalization(error: sqlx::Error) -> Self {
        Self::TerminalizationDatabase {
            commit_ambiguous: false,
            source: error,
        }
    }
}

impl fmt::Display for TurnLivenessRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inventory(source) => {
                write!(
                    formatter,
                    "quiescent active-turn inventory failed: {source}"
                )
            }
            Self::TerminalizationDatabase { source, .. } => {
                write!(formatter, "stale-turn terminalization failed: {source}")
            }
            Self::Terminalization(source) => source.fmt(formatter),
        }
    }
}

impl Error for TurnLivenessRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Inventory(source) | Self::TerminalizationDatabase { source, .. } => Some(source),
            Self::Terminalization(source) => Some(source),
        }
    }
}

impl ClassifyOperatorFailure for TurnLivenessRepositoryError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Inventory(_) => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::TerminalizationDatabase {
                commit_ambiguous, ..
            } => OperatorFailureClass::Infrastructure {
                commit_ambiguous: *commit_ambiguous,
            },
            Self::Terminalization(source) => source.operator_failure_class(),
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Inventory(_) => "turn_liveness_inventory_failed",
            Self::TerminalizationDatabase { .. } => "turn_liveness_terminalization_failed",
            Self::Terminalization(source) => source.operator_failure_cause_code(),
        }
    }
}

impl From<StartupScanRepositoryError> for TurnLivenessRepositoryError {
    fn from(error: StartupScanRepositoryError) -> Self {
        Self::Terminalization(error)
    }
}

/// PostgreSQL inventory and terminalization adapter for turn liveness.
#[derive(Clone, Debug)]
pub struct PostgresTurnLivenessRepository {
    pool: PgPool,
}

impl PostgresTurnLivenessRepository {
    /// Uses the supplied shared pool for liveness supervision.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Reads one bounded page of active turns with no work in flight.
    ///
    /// `after` resumes a rotation: passing the last session of a saturated page
    /// continues past it, and passing `None` starts again from the first. The
    /// returned coverage says whether the page was the whole population, which
    /// is the only condition under which absence means a turn stopped being
    /// quiescent rather than merely falling outside this page.
    pub async fn quiescent_active_turns(
        &self,
        after: Option<SessionId>,
    ) -> Result<QuiescentActiveTurnPage, TurnLivenessRepositoryError> {
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(TurnLivenessRepositoryError::Inventory)?;
        let candidates = read_quiescent_active_turns(&mut connection, None, after)
            .await
            .map_err(TurnLivenessRepositoryError::Inventory)?;
        Ok(QuiescentActiveTurnPage::new(candidates))
    }

    /// Terminalizes one observed-stale turn as failed under the session locks.
    ///
    /// The observation is revalidated inside the transaction, so a turn that
    /// resumed between the scan and this call is left untouched and reported
    /// [`StaleTurnOutcome::Superseded`].
    pub async fn terminalize_stale_turn(
        &self,
        candidate: StaleTurnCandidate,
        identities: AcceptedInputTurnFailureIdentities,
    ) -> Result<StaleTurnOutcome, TurnLivenessRepositoryError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(TurnLivenessRepositoryError::terminalization)?;
        let outcome = terminalize_in_transaction(&mut transaction, candidate, identities).await;
        match outcome {
            Ok(StaleTurnOutcome::Terminalized) => {
                transaction.commit().await.map_err(|error| {
                    TurnLivenessRepositoryError::TerminalizationDatabase {
                        commit_ambiguous: crate::commit_failure_is_ambiguous(&error),
                        source: error,
                    }
                })?;
                Ok(StaleTurnOutcome::Terminalized)
            }
            Ok(StaleTurnOutcome::Superseded) => {
                transaction
                    .rollback()
                    .await
                    .map_err(TurnLivenessRepositoryError::terminalization)?;
                Ok(StaleTurnOutcome::Superseded)
            }
            Err(error) => {
                if let Err(rollback_error) = transaction.rollback().await {
                    return Err(TurnLivenessRepositoryError::terminalization(rollback_error));
                }
                Err(error)
            }
        }
    }
}

/// One page of the quiescent inventory, and where the rotation continues.
#[derive(Clone, Debug)]
pub struct QuiescentActiveTurnPage {
    candidates: Box<[StaleTurnCandidate]>,
    resume_after: Option<SessionId>,
}

impl QuiescentActiveTurnPage {
    /// Derives the continuation cursor from one page.
    ///
    /// A page that filled may have rows behind it, so it carries the cursor to
    /// resume past; a page that did not fill is the end of the rotation and
    /// carries none. Sessions in the result are distinct, because the schema
    /// admits one active turn per session, and the statement resumes strictly
    /// past the cursor — so a rotation driven by these cursors advances on
    /// every page and terminates.
    fn new(candidates: Box<[StaleTurnCandidate]>) -> Self {
        let filled =
            i64::try_from(candidates.len()).unwrap_or(i64::MAX) >= QUIESCENT_INVENTORY_PAGE_SIZE;
        let resume_after = filled
            .then(|| candidates.last().map(|last| last.session()))
            .flatten();
        Self {
            candidates,
            resume_after,
        }
    }

    /// Returns the quiescent turns this page observed.
    pub fn candidates(&self) -> &[StaleTurnCandidate] {
        &self.candidates
    }

    /// Consumes the page, yielding the turns it observed.
    pub fn into_candidates(self) -> Box<[StaleTurnCandidate]> {
        self.candidates
    }

    /// Returns the cursor the rotation continues from, `None` at its end.
    pub const fn resume_after(&self) -> Option<SessionId> {
        self.resume_after
    }
}

/// The complete no-work-in-flight predicate, in one statement.
///
/// Every conjunct is an absence this statement proves for itself. A shape it
/// cannot see is a shape it does not clear: the phase filter admits only
/// `running`, so every durable wait — approval parking included — is outside
/// the result rather than judged by it.
///
/// The attempt arm admits `prepared` as well as `running`. An attempt becomes
/// `running` only when a model call is authorized on it, so `prepared` is a
/// turn activated but never dispatched — and nothing else reaches that shape:
/// the eligibility sweep's active arm requires a live tool round, so it does
/// not re-drive a turn that has none. Excluding `prepared` left exactly that
/// wedge unowned. `stop_requested` and `ended` stay out, the first because an
/// interrupt is in flight and the second because the attempt already closed.
///
/// Newest identities are read with `ORDER BY … DESC LIMIT 1` rather than an
/// aggregate: PostgreSQL defines no `max(uuid)`, verified against 18.4, the
/// version the integration suite runs. The ordering is `uuid`'s native
/// big-endian byte comparison, which for the UUIDv7 identities this schema
/// mints is time order, and it can be answered from an index.
const QUIESCENT_ACTIVE_TURNS: &str = "SELECT active.session_id,
            active.turn_id,
            active.current_attempt_id,
            (SELECT count(*)
               FROM model_call AS counted
              WHERE counted.session_id = active.session_id) AS model_call_count,
            (SELECT newest.model_call_id
               FROM model_call AS newest
              WHERE newest.session_id = active.session_id
              ORDER BY newest.model_call_id DESC
              LIMIT 1) AS latest_model_call_id,
            (SELECT count(*)
               FROM semantic_transcript_entry AS counted
              WHERE counted.source_session_id = active.session_id) AS transcript_entry_count,
            (SELECT newest.semantic_entry_id
               FROM semantic_transcript_entry AS newest
              WHERE newest.source_session_id = active.session_id
              ORDER BY newest.semantic_entry_id DESC
              LIMIT 1) AS latest_transcript_entry_id
       FROM turn_lifecycle AS active
       JOIN turn_attempt AS tenure
         ON tenure.turn_attempt_id = active.current_attempt_id
        AND tenure.turn_id = active.turn_id
        AND tenure.session_id = active.session_id
      WHERE active.state_kind = 'active'
        AND active.origin_kind = 'accepted_input'
        AND NOT active.delegation_runtime_terminal
        AND active.active_phase_kind = 'running'
        AND active.active_tool_round_call_id IS NULL
        AND active.approval_tool_request_id IS NULL
        AND active.recovery_tool_attempt_id IS NULL
        AND tenure.state_kind IN ('prepared', 'running')
        AND tenure.end_variant IS NULL
        AND tenure.end_disposition IS NULL
        AND ($1::uuid IS NULL OR active.session_id = $1)
        AND ($2::uuid IS NULL OR active.session_id > $2)
        AND NOT EXISTS (
            SELECT 1
              FROM model_call AS live
             WHERE live.session_id = active.session_id
               AND live.state_kind <> 'terminal'
        )
        AND NOT EXISTS (
            SELECT 1
              FROM context_compaction_model_call AS live
             WHERE live.session_id = active.session_id
               AND live.state_kind <> 'terminal'
        )
        AND NOT EXISTS (
            SELECT 1
              FROM tool_attempt AS live
             WHERE live.session_id = active.session_id
               AND live.state_kind <> 'terminal'
        )
        AND NOT EXISTS (
            SELECT 1
              FROM accepted_input AS steering
             WHERE steering.session_id = active.session_id
               AND steering.disposition_kind = 'pending_steering'
        )
      ORDER BY active.session_id
      LIMIT $3";

/// Reads one page, leaving classification of any failure to the caller.
///
/// The same statement serves the periodic scan and the locked revalidation, so
/// it returns the driver's error unclassified: the scan reports an inventory
/// failure and the revalidation a terminalization failure.
async fn read_quiescent_active_turns(
    connection: &mut PgConnection,
    session: Option<SessionId>,
    after: Option<SessionId>,
) -> Result<Box<[StaleTurnCandidate]>, sqlx::Error> {
    let rows = sqlx::query(QUIESCENT_ACTIVE_TURNS)
        .bind(session.map(session_id_to_uuid))
        .bind(after.map(session_id_to_uuid))
        .bind(QUIESCENT_INVENTORY_PAGE_SIZE)
        .fetch_all(connection)
        .await?;
    let mut candidates = Vec::with_capacity(rows.len());
    for row in rows {
        let session: Uuid = row.try_get("session_id")?;
        let turn: Uuid = row.try_get("turn_id")?;
        let attempt: Uuid = row.try_get("current_attempt_id")?;
        let model_call_count: i64 = row.try_get("model_call_count")?;
        let latest_model_call: Option<Uuid> = row.try_get("latest_model_call_id")?;
        let transcript_entry_count: i64 = row.try_get("transcript_entry_count")?;
        let latest_transcript_entry: Option<Uuid> = row.try_get("latest_transcript_entry_id")?;
        candidates.push(StaleTurnCandidate::new(
            session_id_from_uuid(session),
            turn_id_from_uuid(turn),
            TurnLivenessEvidence::new(
                TurnAttemptId::from_uuid(attempt),
                model_call_count.unsigned_abs(),
                latest_model_call.map(ModelCallId::from_uuid),
                transcript_entry_count.unsigned_abs(),
                latest_transcript_entry.map(SemanticTranscriptEntryId::from_uuid),
            ),
        ));
    }
    Ok(candidates.into_boxed_slice())
}

async fn terminalize_in_transaction(
    connection: &mut PgConnection,
    candidate: StaleTurnCandidate,
    identities: AcceptedInputTurnFailureIdentities,
) -> Result<StaleTurnOutcome, TurnLivenessRepositoryError> {
    let locks = sqlx::query(crate::lock_inventory::STARTUP_RECOVERY)
        .bind(session_id_to_uuid(candidate.session()))
        .fetch_one(&mut *connection)
        .await
        .map_err(TurnLivenessRepositoryError::terminalization)?;
    let session_exists: bool = locks
        .try_get(0)
        .map_err(TurnLivenessRepositoryError::terminalization)?;
    let scheduler_session: Option<Uuid> = locks
        .try_get(1)
        .map_err(TurnLivenessRepositoryError::terminalization)?;
    let active_turn: Option<Uuid> = locks
        .try_get(2)
        .map_err(TurnLivenessRepositoryError::terminalization)?;
    if !session_exists
        || scheduler_session.is_none()
        || active_turn != Some(turn_id_to_uuid(candidate.turn()))
    {
        return Ok(StaleTurnOutcome::Superseded);
    }
    // The scan ran without the scheduler lock, so the whole predicate is
    // re-decided here against rows no concurrent pass can now be changing.
    let locked = read_quiescent_active_turns(connection, Some(candidate.session()), None)
        .await
        .map_err(TurnLivenessRepositoryError::terminalization)?;
    if locked.first().copied() != Some(candidate) {
        return Ok(StaleTurnOutcome::Superseded);
    }

    let session = match load_session_from_connection(connection, candidate.session()).await {
        Ok(Some(session)) => session,
        Ok(None) => return Ok(StaleTurnOutcome::Superseded),
        Err(SessionRepositoryError::Database(error)) => {
            return Err(StartupScanRepositoryError::from(error).into());
        }
        Err(SessionRepositoryError::Corruption(error)) => {
            return Err(
                StartupScanRepositoryError::from(StartupScanCorruption::CurrentSession(error))
                    .into(),
            );
        }
    };
    let scheduling = load_scheduling_projection(connection, session)
        .await
        .map_err(map_scheduling_error)?;
    let prepared =
        match scheduling.prepare_active_turn_lost_failure(identities) {
            Ok(prepared) => prepared,
            // This is the identical transition startup recovery prepares, so it is
            // classified identically: only a turn that is no longer active is a
            // concurrent departure. Every other refusal contradicts something the
            // locked predicate proved absent moments earlier, in the same
            // transaction, under the same locks — a projection that disagrees with
            // the rows it was built from is inconsistent durable state, not a race.
            // Reporting those as supersession would retry them silently forever
            // while the slot stayed wedged, which is exactly the failure this pass
            // exists to end.
            Err(error) => match error.failure() {
                AcceptedInputTurnFailureFailure::NoActiveTurn => {
                    return Ok(StaleTurnOutcome::Superseded);
                }
                AcceptedInputTurnFailureFailure::PendingSteering { .. }
                | AcceptedInputTurnFailureFailure::ActiveAttemptCannotEndLost
                | AcceptedInputTurnFailureFailure::ActiveStartMissing
                | AcceptedInputTurnFailureFailure::StartingSnapshotMissing
                | AcceptedInputTurnFailureFailure::TerminalFrontierCannotAppend => {
                    return Err(StartupScanRepositoryError::from(
                        StartupScanCorruption::Inconsistent("active failure preparation"),
                    )
                    .into());
                }
                AcceptedInputTurnFailureFailure::FailureEntryIdentityAlreadyExists => {
                    return Err(StartupScanRepositoryError::IdentityCollision(
                        StartupScanIdentityCollision::FailureEntry,
                    )
                    .into());
                }
                AcceptedInputTurnFailureFailure::TerminalFrontierIdentityAlreadyExists => {
                    return Err(StartupScanRepositoryError::IdentityCollision(
                        StartupScanIdentityCollision::TerminalFrontier,
                    )
                    .into());
                }
            },
        };
    insert_prepared_failure(connection, prepared).await?;
    Ok(StaleTurnOutcome::Terminalized)
}

#[cfg(test)]
mod tests {
    use super::{QUIESCENT_INVENTORY_PAGE_SIZE, QuiescentActiveTurnPage};
    use signalbox_application::{StaleTurnCandidate, TurnLivenessEvidence};
    use signalbox_domain::{SessionId, TurnAttemptId, TurnId};
    use sqlx::types::Uuid;

    fn candidate(session: u128) -> StaleTurnCandidate {
        StaleTurnCandidate::new(
            SessionId::from_uuid(Uuid::from_u128(session)),
            TurnId::from_uuid(Uuid::from_u128(0xa_0000 + session)),
            TurnLivenessEvidence::new(
                TurnAttemptId::from_uuid(Uuid::from_u128(0xb_0000 + session)),
                0,
                None,
                0,
                None,
            ),
        )
    }

    fn page(rows: usize) -> QuiescentActiveTurnPage {
        QuiescentActiveTurnPage::new(
            (1..=rows)
                .map(|session| candidate(session as u128))
                .collect(),
        )
    }

    /// A page that did not fill is the end of the rotation, so a scan that
    /// reads it has the whole population and needs no further read.
    #[test]
    fn an_underfilled_page_ends_the_rotation() {
        let short = page(3);
        let empty = page(0);

        assert_eq!(short.resume_after(), None);
        assert_eq!(short.candidates().len(), 3);
        assert_eq!(empty.resume_after(), None);
    }

    /// A full page may have rows behind it, so it carries the cursor the next
    /// read resumes strictly past — which is what makes the rotation advance.
    #[test]
    fn a_full_page_resumes_past_its_last_session() {
        let rows = usize::try_from(QUIESCENT_INVENTORY_PAGE_SIZE).expect("the page size is small");
        let full = page(rows);

        assert_eq!(
            full.resume_after(),
            Some(candidate(rows as u128).session()),
            "the cursor is the greatest session this page returned"
        );
    }
}
