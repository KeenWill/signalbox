//! Repository-watch successor admission after a terminal tool continuation.

use std::{error::Error, fmt};

use sha2::{Digest, Sha256};
use signalbox_application::{
    ClassifyOperatorFailure, InProcessEligibilityNudge, InProcessToolDispatchGate,
    OperatorFailureClass, SubmitInputOutcome, SubmitInputRequest, SubmitInputService,
    UuidV7SubmitInputIdGenerator,
};
use signalbox_domain::{
    CommandPrincipal, DurableCommandId, ModelSelectionOverride, ModuleDispatch,
    ParentTerminationKind, PerInputConfigurationChoices, SessionCreationCause, SessionId,
    SubmitInputResult, TurnId, UserContent,
};
use signalbox_persistence::{
    session::{SessionRepository, SessionRepositoryError},
    submit_input::{SubmitInputRepository, SubmitInputRepositoryError},
};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::{HubModelConfiguration, process_runtime::ConfiguredSubmitInputTransaction};

// Fixed continuation input from docs/spec/model-call-execution.md.
const CONTINUATION_INPUT: &str =
    "Continue the unfinished repository-watch task from the compacted context.";
// Domain separation for one durable successor command per terminal turn.
const CONTINUATION_IDENTITY_DOMAIN: &[u8] = b"signalbox.repository-watch.compaction-continuation";

#[derive(Clone, Debug)]
pub(super) struct RepositoryWatchContinuation {
    pub(super) nudge: InProcessEligibilityNudge,
    pub(super) tool_gate: InProcessToolDispatchGate,
}

/// Failure admitting the bounded successor of a terminal continuation.
#[derive(Debug)]
pub enum ContinuationCompactionError {
    /// The persisted terminalization or compaction receipt could not be read.
    Database(sqlx::Error),
    /// Session provenance could not be loaded.
    Session(SessionRepositoryError),
    /// Successor admission could not commit.
    Submit(SubmitInputRepositoryError),
    /// The ordinary input admission contract rejected the successor.
    Rejected,
}

impl fmt::Display for ContinuationCompactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("repository-watch compaction continuation failed")
    }
}

impl Error for ContinuationCompactionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Session(error) => Some(error),
            Self::Submit(error) => Some(error),
            Self::Rejected => None,
        }
    }
}

impl ClassifyOperatorFailure for ContinuationCompactionError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Database(_) => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::Session(SessionRepositoryError::Database(_))
            | Self::Submit(SubmitInputRepositoryError::Database(_)) => {
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                }
            }
            Self::Submit(SubmitInputRepositoryError::CommitAmbiguous(_)) => {
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: true,
                }
            }
            Self::Submit(SubmitInputRepositoryError::ModelExecution(error)) => {
                error.operator_failure_class()
            }
            Self::Session(SessionRepositoryError::Corruption(_))
            | Self::Submit(SubmitInputRepositoryError::Corruption(_)) => {
                OperatorFailureClass::FailClosedCorruption
            }
            Self::Submit(
                SubmitInputRepositoryError::DifferentCommandKind { .. }
                | SubmitInputRepositoryError::AcceptedInputIdentityCollision { .. }
                | SubmitInputRepositoryError::UnsupportedModelSetting(_),
            ) => OperatorFailureClass::CallerOrHubBug,
            Self::Rejected => OperatorFailureClass::CallerOrHubBug,
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        "repository_watch_compaction_continuation"
    }
}

impl RepositoryWatchContinuation {
    pub(super) async fn enqueue(
        &self,
        pool: &PgPool,
        configuration: &HubModelConfiguration,
        session: SessionId,
    ) -> Result<(), ContinuationCompactionError> {
        let Some(loaded) = SessionRepository::new(pool.clone())
            .load_session(session)
            .await
            .map_err(ContinuationCompactionError::Session)?
        else {
            return Ok(());
        };
        if !matches!(
            loaded.creation_provenance().cause(),
            SessionCreationCause::ModuleDispatched {
                dispatch: ModuleDispatch::RepositoryWatch { .. }
            }
        ) {
            return Ok(());
        }
        // The headroom record distinguishes the continuation terminalization
        // from other context failures. A later turn already carries the work.
        let terminal = sqlx::query_scalar::<_, Uuid>(
            "SELECT failed.turn_id
               FROM turn_lifecycle AS failed
               JOIN tool_continuation_context_headroom AS headroom
                 ON headroom.terminal_attempt_id = failed.terminal_attempt_id
              WHERE failed.session_id = $1
                AND NOT EXISTS (
                    SELECT 1 FROM turn_lifecycle AS later
                     WHERE later.session_id = failed.session_id
                       AND later.acceptance_position > failed.acceptance_position)",
        )
        .bind(session.into_uuid())
        .fetch_optional(pool)
        .await
        .map_err(ContinuationCompactionError::Database)?;
        let Some(terminal) = terminal else {
            return Ok(());
        };
        let command = continuation_command(session, TurnId::from_uuid(terminal));
        let recorded = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM durable_command WHERE command_id = $1)",
        )
        .bind(command.into_uuid())
        .fetch_one(pool)
        .await
        .map_err(ContinuationCompactionError::Database)?;
        if recorded {
            return Ok(());
        }
        let request = SubmitInputRequest::try_new_core_continuation(
            command,
            session,
            UserContent::try_text(CONTINUATION_INPUT.to_owned())
                .map_err(|_| ContinuationCompactionError::Rejected)?,
            PerInputConfigurationChoices::new(
                loaded.current_configuration_defaults().version(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        )
        .map_err(|_| ContinuationCompactionError::Rejected)?;
        let transaction = ConfiguredSubmitInputTransaction {
            repository: SubmitInputRepository::with_model_capabilities(
                pool.clone(),
                configuration.model_capability_catalog(),
            ),
            model_configuration: configuration,
            principal: CommandPrincipal::Core,
            cascade_root_kind: ParentTerminationKind::Cancelled,
        };
        let mut service = SubmitInputService::new(
            UuidV7SubmitInputIdGenerator,
            transaction,
            self.nudge.clone(),
            self.tool_gate.clone(),
        );
        match service
            .execute(request)
            .await
            .map_err(ContinuationCompactionError::Submit)?
        {
            SubmitInputOutcome::Recorded(SubmitInputResult::Applied(_)) => Ok(()),
            SubmitInputOutcome::Recorded(SubmitInputResult::Rejected(_))
            | SubmitInputOutcome::ConflictingReuse { .. } => {
                Err(ContinuationCompactionError::Rejected)
            }
        }
    }
}

pub(super) async fn successor_requires_compaction(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
) -> Result<bool, ContinuationCompactionError> {
    let row = sqlx::query(
        "SELECT origin.accepting_command_id, previous.turn_id AS previous_turn,
                EXISTS (SELECT 1 FROM compact_session_command AS compact
                         WHERE compact.session_id = queued.session_id
                           AND compact.automatic_for_turn_id = queued.turn_id
                           AND compact.result_kind = 'applied') AS compacted
           FROM turn_lifecycle AS queued
           JOIN session AS owner ON owner.session_id = queued.session_id
            AND owner.creation_cause = 'module_dispatched'
            AND owner.dispatching_module = 'repo_watch'
           JOIN accepted_input AS origin
             ON origin.accepted_input_id = queued.origin_accepted_input_id
           JOIN LATERAL (
               SELECT failed.turn_id
                 FROM turn_lifecycle AS failed
                 JOIN tool_continuation_context_headroom AS headroom
                   ON headroom.terminal_attempt_id = failed.terminal_attempt_id
                WHERE failed.session_id = queued.session_id
                  AND failed.acceptance_position < queued.acceptance_position
                ORDER BY failed.acceptance_position DESC LIMIT 1
           ) AS previous ON TRUE
          WHERE queued.session_id = $1 AND queued.turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_optional(pool)
    .await
    .map_err(ContinuationCompactionError::Database)?;
    let Some(row) = row else {
        return Ok(false);
    };
    let command: Option<Uuid> = row
        .try_get("accepting_command_id")
        .map_err(ContinuationCompactionError::Database)?;
    let previous: Uuid = row
        .try_get("previous_turn")
        .map_err(ContinuationCompactionError::Database)?;
    let compacted: bool = row
        .try_get("compacted")
        .map_err(ContinuationCompactionError::Database)?;
    Ok(!compacted
        && command == Some(continuation_command(session, TurnId::from_uuid(previous)).into_uuid()))
}

fn continuation_command(session: SessionId, terminal: TurnId) -> DurableCommandId {
    let mut digest = Sha256::new();
    digest.update(CONTINUATION_IDENTITY_DOMAIN);
    digest.update(session.into_uuid().as_bytes());
    digest.update(terminal.into_uuid().as_bytes());
    let hash: [u8; 32] = digest.finalize().into();
    let mut identity = [0_u8; 16];
    identity
        .iter_mut()
        .zip(hash)
        .for_each(|(byte, digested)| *byte = digested);
    // RFC 9562 version 8 and variant bits identify a derived name.
    identity[6] = (identity[6] & 0x0f) | 0x80;
    identity[8] = (identity[8] & 0x3f) | 0x80;
    DurableCommandId::from_uuid(Uuid::from_bytes(identity))
}
