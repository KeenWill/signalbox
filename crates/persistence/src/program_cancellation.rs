//! Durable operator cancellation of retained program journals.

use crate::{
    command_registry::{self, CommandKind},
    program_journal::{ProgramJournalRepository, ProgramJournalRepositoryError},
};
use signalbox_domain::{DeliveryKind, DurableCommandId, InlineFramePayload, ProgramRunId};
use sqlx::{PgPool, Row};

/// Cancellation meaning, independent of its durable command identity.
#[derive(Clone, Debug)]
pub struct CancelProgramRun {
    pub command_id: DurableCommandId,
    pub run_id: ProgramRunId,
}
impl PartialEq for CancelProgramRun {
    fn eq(&self, other: &Self) -> bool {
        self.run_id == other.run_id
    }
}
impl Eq for CancelProgramRun {}

/// Standing terminal state of a retained journal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgramTerminalState {
    Cancelled,
    Faulted,
}

/// The immutable result a cancellation command records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgramCancellationOutcome {
    Applied,
    NotFound,
    AlreadyTerminal(ProgramTerminalState),
}

/// A recorded result or conflicting reuse of the command identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgramCancellationResult {
    Recorded(ProgramCancellationOutcome),
    ConflictingReuse,
}

/// Cancellation storage failure, preserving ambiguous commit outcomes.
#[derive(Debug, signalbox_derive::OperatorError)]
pub enum ProgramCancellationError {
    #[error("program cancellation database failure: {field_0}")]
    Database(#[source] sqlx::Error),
    #[error("program cancellation commit is ambiguous: {field_0}")]
    CommitAmbiguous(#[source] sqlx::Error),
    #[error(transparent)]
    Journal(#[source] ProgramJournalRepositoryError),
    #[error("program cancellation records are inconsistent")]
    Corruption,
}
impl From<sqlx::Error> for ProgramCancellationError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}
impl From<ProgramJournalRepositoryError> for ProgramCancellationError {
    fn from(error: ProgramJournalRepositoryError) -> Self {
        Self::Journal(error)
    }
}

/// Records one cancellation and its terminal delivery atomically, or replays its result.
pub async fn cancel(
    pool: &PgPool,
    command: CancelProgramRun,
) -> Result<ProgramCancellationResult, ProgramCancellationError> {
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query("INSERT INTO durable_command(command_id, command_kind, storage_version, claimed_at, issuer_kind)
        VALUES ($1, 'cancel_program_run', 1, transaction_timestamp(), 'operator') ON CONFLICT DO NOTHING")
        .bind(command.command_id.into_uuid()).execute(&mut *tx).await?.rows_affected() == 1;
    if !inserted {
        let kind = command_registry::inspect(&mut tx, command.command_id)
            .await
            .map_err(|error| match error {
                command_registry::RegistryInspectionError::Database(error) => {
                    ProgramCancellationError::Database(error)
                }
                command_registry::RegistryInspectionError::Corruption(_) => {
                    ProgramCancellationError::Corruption
                }
            })?;
        if kind != Some(CommandKind::CancelProgramRun) {
            return Ok(ProgramCancellationResult::ConflictingReuse);
        }
        let row = sqlx::query("SELECT run_id, outcome, terminal_state FROM cancel_program_run_command WHERE command_id = $1")
            .bind(command.command_id.into_uuid()).fetch_one(&mut *tx).await?;
        let run: uuid::Uuid = row.try_get("run_id")?;
        if run != command.run_id.into_uuid() {
            return Ok(ProgramCancellationResult::ConflictingReuse);
        }
        return Ok(ProgramCancellationResult::Recorded(decode_outcome(&row)?));
    }
    let journal =
        ProgramJournalRepository::load_locked_in_transaction(&mut tx, command.run_id).await?;
    let (outcome, terminal_position) = match journal {
        None => (ProgramCancellationOutcome::NotFound, None),
        Some(journal) => match journal.terminal_delivery().map(|delivery| delivery.kind()) {
            Some(DeliveryKind::RunCancel(_)) => (
                ProgramCancellationOutcome::AlreadyTerminal(ProgramTerminalState::Cancelled),
                None,
            ),
            Some(DeliveryKind::Fault(_)) => (
                ProgramCancellationOutcome::AlreadyTerminal(ProgramTerminalState::Faulted),
                None,
            ),
            Some(_) => return Err(ProgramCancellationError::Corruption),
            None => {
                let position = journal
                    .entries()
                    .last()
                    .map_or(0, |entry| entry.position().as_u64())
                    .checked_add(1)
                    .ok_or(ProgramCancellationError::Corruption)?;
                ProgramJournalRepository::append_delivery_in_transaction(
                    &mut tx,
                    command.run_id,
                    DeliveryKind::RunCancel(InlineFramePayload::new(
                        command.command_id.into_uuid().to_string().into_bytes(),
                    )),
                )
                .await?;
                (
                    ProgramCancellationOutcome::Applied,
                    Some(rust_decimal::Decimal::from(position)),
                )
            }
        },
    };
    let (kind, state) = match outcome {
        ProgramCancellationOutcome::Applied => ("applied", Some("cancelled")),
        ProgramCancellationOutcome::NotFound => ("not_found", None),
        ProgramCancellationOutcome::AlreadyTerminal(ProgramTerminalState::Cancelled) => {
            ("already_terminal", Some("cancelled"))
        }
        ProgramCancellationOutcome::AlreadyTerminal(ProgramTerminalState::Faulted) => {
            ("already_terminal", Some("faulted"))
        }
    };
    sqlx::query("INSERT INTO cancel_program_run_command(command_id, run_id, outcome, terminal_state, cancellation_position) VALUES ($1, $2, $3, $4, $5)")
        .bind(command.command_id.into_uuid()).bind(command.run_id.into_uuid()).bind(kind).bind(state).bind(terminal_position)
        .execute(&mut *tx).await?;
    tx.commit().await.map_err(|error| {
        if crate::commit_failure_is_ambiguous(&error) {
            ProgramCancellationError::CommitAmbiguous(error)
        } else {
            ProgramCancellationError::Database(error)
        }
    })?;
    Ok(ProgramCancellationResult::Recorded(outcome))
}

fn decode_outcome(
    row: &sqlx::postgres::PgRow,
) -> Result<ProgramCancellationOutcome, ProgramCancellationError> {
    let outcome: String = row.try_get("outcome")?;
    let state: Option<String> = row.try_get("terminal_state")?;
    match (outcome.as_str(), state.as_deref()) {
        ("applied", Some("cancelled")) => Ok(ProgramCancellationOutcome::Applied),
        ("not_found", None) => Ok(ProgramCancellationOutcome::NotFound),
        ("already_terminal", Some("cancelled")) => Ok(ProgramCancellationOutcome::AlreadyTerminal(
            ProgramTerminalState::Cancelled,
        )),
        ("already_terminal", Some("faulted")) => Ok(ProgramCancellationOutcome::AlreadyTerminal(
            ProgramTerminalState::Faulted,
        )),
        _ => Err(ProgramCancellationError::Corruption),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_equality_ignores_command_identity() {
        let run_id = ProgramRunId::from_uuid(uuid::Uuid::now_v7());
        assert_eq!(
            CancelProgramRun {
                command_id: DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
                run_id
            },
            CancelProgramRun {
                command_id: DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
                run_id
            }
        );
    }
}
