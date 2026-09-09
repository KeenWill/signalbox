//! Repository-watch successor admission after a terminal tool continuation.

use std::{error::Error, fmt};

use sha2::{Digest, Sha256};

use signalbox_application::{
    ClassifyOperatorFailure, EligibilityNudge, InProcessEligibilityNudge,
    InProcessToolDispatchGate, OperatorFailureClass, SubmitInputOutcome, SubmitInputRequest,
    SubmitInputService, UuidV7SubmitInputIdGenerator,
};
use signalbox_domain::{
    AcceptedInputId, CommandPrincipal, DurableCommandId, ModelSelectionOverride, ModuleDispatch,
    ParentTerminationKind, PerInputConfigurationChoices, SessionConfigurationDefaultsVersion,
    SessionCreationCause, SessionId, SubmitInputRejectedResult, SubmitInputResult, TurnId,
    UserContent,
};
use signalbox_persistence::{
    context_compaction_continuation::{CompactionContinuationRepository, CompactionSuccessor},
    goal::{GoalRepository, GoalRepositoryError},
    goal_turn::{GoalTurnCandidates, GoalTurnContinuationOutcome},
    session::{SessionRepository, SessionRepositoryError},
    submit_input::{SubmitInputRepository, SubmitInputRepositoryError},
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{HubModelConfiguration, process_runtime::ConfiguredSubmitInputTransaction};

// Fixed continuation input from docs/spec/model-call-execution.md.
const CONTINUATION_INPUT: &str =
    "Continue the unfinished repository-watch task from the compacted context.";
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
    /// Goal continuation admission could not commit.
    Goal(GoalRepositoryError),
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
            Self::Goal(error) => Some(error),
            Self::Rejected => None,
        }
    }
}

impl ClassifyOperatorFailure for ContinuationCompactionError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Goal(GoalRepositoryError::Database(_)) | Self::Database(_) => {
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                }
            }
            Self::Session(SessionRepositoryError::Database(_))
            | Self::Submit(SubmitInputRepositoryError::Database(_))
            | Self::Submit(SubmitInputRepositoryError::CheckoutProvisioningPending) => {
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                }
            }
            Self::Goal(GoalRepositoryError::CommitAmbiguous(_))
            | Self::Submit(SubmitInputRepositoryError::CommitAmbiguous(_)) => {
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: true,
                }
            }
            Self::Submit(SubmitInputRepositoryError::ModelExecution(error)) => {
                error.operator_failure_class()
            }
            Self::Goal(GoalRepositoryError::Corruption(_))
            | Self::Session(SessionRepositoryError::Corruption(_))
            | Self::Submit(SubmitInputRepositoryError::Corruption(_)) => {
                OperatorFailureClass::FailClosedCorruption
            }
            Self::Submit(
                SubmitInputRepositoryError::DifferentCommandKind { .. }
                | SubmitInputRepositoryError::AcceptedInputIdentityCollision { .. }
                | SubmitInputRepositoryError::UnsupportedModelSetting(_),
            ) => OperatorFailureClass::CallerOrHubBug,
            Self::Goal(GoalRepositoryError::DifferentCommandKind { .. }) | Self::Rejected => {
                OperatorFailureClass::CallerOrHubBug
            }
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
        let repository = CompactionContinuationRepository::new(pool.clone());
        let Some(terminal) = repository
            .terminal_candidate(session)
            .await
            .map_err(ContinuationCompactionError::Database)?
        else {
            return Ok(());
        };
        match GoalRepository::new(pool.clone())
            .continue_after_context_exhaustion(
                session,
                terminal,
                GoalTurnCandidates::new(
                    AcceptedInputId::from_uuid(Uuid::now_v7()),
                    TurnId::from_uuid(Uuid::now_v7()),
                ),
                CONTINUATION_INPUT,
                |alias| configuration.resolve_alias(alias),
            )
            .await
            .map_err(ContinuationCompactionError::Goal)?
        {
            GoalTurnContinuationOutcome::Scheduled { .. } => {
                let _ = self.nudge.nudge(session);
                return Ok(());
            }
            GoalTurnContinuationOutcome::NotCurrentGoalTurn => {}
            GoalTurnContinuationOutcome::AlreadyScheduled
            | GoalTurnContinuationOutcome::NotPursuing => return Ok(()),
            _ => return Err(ContinuationCompactionError::Rejected),
        }
        let inputs = SubmitInputRepository::new(pool.clone());
        let (command, recorded) =
            admission_after_configuration_rejections(&inputs, session, terminal)
                .await
                .map_err(ContinuationCompactionError::Submit)?;
        match recorded {
            Some(SubmitInputResult::Applied(_)) => return Ok(()),
            Some(SubmitInputResult::Rejected(_)) => {
                return Err(ContinuationCompactionError::Rejected);
            }
            None => {}
        }
        let request = continuation_request(
            command,
            session,
            loaded.current_configuration_defaults().version(),
        )?;
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
    match CompactionContinuationRepository::new(pool.clone())
        .uncompacted_successor(session, turn)
        .await
        .map_err(ContinuationCompactionError::Database)?
    {
        Some(CompactionSuccessor::Goal) => Ok(true),
        Some(CompactionSuccessor::Submitted {
            predecessor,
            command,
        }) => {
            let (admission, result) = admission_after_configuration_rejections(
                &SubmitInputRepository::new(pool.clone()),
                session,
                predecessor,
            )
            .await
            .map_err(ContinuationCompactionError::Submit)?;
            Ok(command == admission && matches!(result, Some(SubmitInputResult::Applied(_))))
        }
        None => Ok(false),
    }
}

async fn admission_after_configuration_rejections(
    repository: &SubmitInputRepository,
    session: SessionId,
    terminal: TurnId,
) -> Result<(DurableCommandId, Option<SubmitInputResult>), SubmitInputRepositoryError> {
    let mut command = continuation_command(session, terminal);
    loop {
        let recorded = repository.load(command).await?;
        match recorded.as_ref().map(|recorded| recorded.result()) {
            Some(SubmitInputResult::Rejected(
                SubmitInputRejectedResult::SessionDefaultsVersionMismatch { .. }
                | SubmitInputRejectedResult::UnknownModelAlias { .. },
            )) => {
                // An immutable configuration rejection admits another command;
                // concurrent retries traverse the same chain and stop at success.
                command = defaults_retry_command(command);
            }
            _ => return Ok((command, recorded.map(|recorded| recorded.result().clone()))),
        }
    }
}

fn continuation_request(
    command: DurableCommandId,
    session: SessionId,
    version: SessionConfigurationDefaultsVersion,
) -> Result<SubmitInputRequest, ContinuationCompactionError> {
    SubmitInputRequest::try_new_core_continuation(
        command,
        session,
        UserContent::try_text(CONTINUATION_INPUT.to_owned())
            .map_err(|_| ContinuationCompactionError::Rejected)?,
        PerInputConfigurationChoices::new(version, ModelSelectionOverride::UseSessionDefault),
    )
    .map_err(|_| ContinuationCompactionError::Rejected)
}

/// Prepares the production continuation input at a chosen defaults epoch for race tests.
#[cfg(feature = "test-support")]
pub fn repository_watch_continuation_test_request(
    session: SessionId,
    terminal: TurnId,
    version: SessionConfigurationDefaultsVersion,
) -> Result<SubmitInputRequest, ContinuationCompactionError> {
    continuation_request(continuation_command(session, terminal), session, version)
}

// Domain separation for the initial successor command per terminal turn.
const CONTINUATION_IDENTITY_DOMAIN: &[u8] = b"signalbox.repository-watch.compaction-continuation";
/// Derives the ordinary-input command identity for one failed frontier.
fn continuation_command(session: SessionId, terminal: TurnId) -> DurableCommandId {
    let mut digest = Sha256::new();
    digest.update(CONTINUATION_IDENTITY_DOMAIN);
    digest.update(session.into_uuid().as_bytes());
    digest.update(terminal.into_uuid().as_bytes());
    command_from_digest(digest)
}

fn defaults_retry_command(rejected: DurableCommandId) -> DurableCommandId {
    let mut digest = Sha256::new();
    // Separate retry identities from the initial session/terminal identity.
    digest.update(b"signalbox.repository-watch.compaction-continuation.defaults-retry");
    digest.update(rejected.into_uuid().as_bytes());
    command_from_digest(digest)
}

fn command_from_digest(digest: Sha256) -> DurableCommandId {
    let hash: [u8; 32] = digest.finalize().into();
    let mut identity = [0_u8; 16];
    identity.copy_from_slice(&hash[..16]);
    // RFC 9562 version 8 and variant bits identify a derived name.
    identity[6] = (identity[6] & 0x0f) | 0x80;
    identity[8] = (identity[8] & 0x3f) | 0x80;
    DurableCommandId::from_uuid(Uuid::from_bytes(identity))
}
