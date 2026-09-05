//! Atomic PostgreSQL activation of the earliest eligible accepted-input turn.

use std::{error::Error, fmt, num::NonZeroU64};

use signalbox_application::{
    ClassifyOperatorFailure, OperatorFailureClass, StartEligibleTurnOutcome,
    StartEligibleTurnTransaction,
};
use signalbox_domain::{
    AcceptedInputEligibilityFailure, AcceptedInputSchedulingProjection,
    AcceptedInputStartingLineage, AcceptedInputTurnActivationIdentities, ActivatedTurn,
    ActiveTurnPhase, CurrentTurnAttemptState, DelegatedTurnActivationInput,
    DelegatedWakeTurnActivationInput, DelegationContent, PreparedAcceptedInputTurnActivation,
    PreparedDelegatedTurnActivation, PreparedTurnActivation, SemanticTranscriptEntryId,
    SemanticTranscriptEntryPayload as InitialSemanticTranscriptEntryPayload,
    SemanticTranscriptEntryReconstitutionInput, SemanticTranscriptEntryRef, SessionId,
    ToolRequestId, TurnId,
};
use sqlx::{
    PgConnection, PgPool, Row,
    types::{Decimal, Uuid},
};

use crate::{
    commit_failure_is_ambiguous,
    mapping::{
        defaults_version_to_numeric, input_position_to_numeric, positive_u64_from_numeric,
        session_id_to_uuid, turn_id_to_uuid,
    },
    model_execution::{
        SnapshotAppend, SnapshotAppendError, insert_snapshot_append,
        lock_delegated_child_endpoint_sessions,
    },
    outbox::{self, OutboxEvent},
    session::{SessionCorruption, SessionRepositoryError, load_session_from_connection},
    submit_input::{
        SubmitInputCorruption, SubmitInputRepositoryError, decode_goal_origin_configuration,
        load_scheduling_projection,
    },
    workspace_instructions::{
        CountedActivationInstructionEvidence, WorkspaceInstructionRepository,
        WorkspaceInstructionRepositoryError,
    },
};

/// Which fresh activation identity collided with an existing durable identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartEligibleTurnIdentityCollision {
    /// The proposed model-identity boundary semantic entry already exists.
    ModelIdentityEntry,
    /// The proposed semantic origin-entry identity already exists.
    OriginEntry,
    /// The proposed starting context-frontier identity already exists.
    StartingFrontier,
    /// The proposed initial turn-attempt identity already exists.
    InitialAttempt,
}

impl fmt::Display for StartEligibleTurnIdentityCollision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let identity = match self {
            Self::ModelIdentityEntry => "model-identity semantic-entry",
            Self::OriginEntry => "origin semantic-entry",
            Self::StartingFrontier => "starting context-frontier",
            Self::InitialAttempt => "initial turn-attempt",
        };
        write!(formatter, "{identity} identity already exists")
    }
}

impl Error for StartEligibleTurnIdentityCollision {}

/// A durable shape that cannot reconstruct or commit one eligibility pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartEligibleTurnCorruption {
    /// One required durable record is absent.
    Missing(&'static str),
    /// Correlated durable records disagree.
    Inconsistent(&'static str),
    /// The current session projection is invalid.
    CurrentSession(SessionCorruption),
    /// Complete scheduling records fail their checked persistence mapping.
    Scheduling(SubmitInputCorruption),
}

impl fmt::Display for StartEligibleTurnCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(record) => write!(formatter, "missing StartEligibleTurn {record}"),
            Self::Inconsistent(relationship) => {
                write!(formatter, "inconsistent StartEligibleTurn {relationship}")
            }
            Self::CurrentSession(error) => {
                write!(
                    formatter,
                    "StartEligibleTurn current Session is invalid: {error}"
                )
            }
            Self::Scheduling(error) => {
                write!(
                    formatter,
                    "StartEligibleTurn scheduling projection is invalid: {error}"
                )
            }
        }
    }
}

impl Error for StartEligibleTurnCorruption {}

/// A database, integrity, or identity-collision failure during eligibility.
#[derive(Debug)]
pub enum StartEligibleTurnRepositoryError {
    /// PostgreSQL could not complete the transaction.
    Database {
        /// The underlying SQLx failure.
        source: sqlx::Error,
        /// Whether the failure occurred while awaiting commit.
        commit_ambiguous: bool,
    },
    /// Durable records cannot reconstruct or commit the accepted domain shape.
    Corruption(StartEligibleTurnCorruption),
    /// A supplied fresh identity already names a durable record.
    IdentityCollision(StartEligibleTurnIdentityCollision),
    /// Checked activation output violated an internal hub invariant.
    HubInvariant(&'static str),
}

impl fmt::Display for StartEligibleTurnRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database { source, .. } => {
                write!(formatter, "StartEligibleTurn database failure: {source}")
            }
            Self::Corruption(error) => error.fmt(formatter),
            Self::IdentityCollision(error) => error.fmt(formatter),
            Self::HubInvariant(invariant) => {
                write!(
                    formatter,
                    "StartEligibleTurn hub invariant failed: {invariant}"
                )
            }
        }
    }
}

impl Error for StartEligibleTurnRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::Corruption(error) => Some(error),
            Self::IdentityCollision(error) => Some(error),
            Self::HubInvariant(_) => None,
        }
    }
}

impl ClassifyOperatorFailure for StartEligibleTurnRepositoryError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Database {
                commit_ambiguous, ..
            } => OperatorFailureClass::Infrastructure {
                commit_ambiguous: *commit_ambiguous,
            },
            Self::Corruption(_) => OperatorFailureClass::FailClosedCorruption,
            Self::IdentityCollision(_) => OperatorFailureClass::IdentityCollision,
            Self::HubInvariant(_) => OperatorFailureClass::CallerOrHubBug,
        }
    }
}

impl From<StartEligibleTurnCorruption> for StartEligibleTurnRepositoryError {
    fn from(error: StartEligibleTurnCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl From<sqlx::Error> for StartEligibleTurnRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::from_database(error, false)
    }
}

impl StartEligibleTurnRepositoryError {
    fn from_database(error: sqlx::Error, commit_ambiguous: bool) -> Self {
        if let Some(collision) = identity_collision(&error) {
            Self::IdentityCollision(collision)
        } else {
            Self::Database {
                source: error,
                commit_ambiguous,
            }
        }
    }
}

/// Failure while atomically binding a counted activation to its Prepared call.
#[derive(Debug)]
pub enum CommitActivationPreviewError {
    /// Activation revalidation, persistence, or commit failed.
    Activation(StartEligibleTurnRepositoryError),
    /// The exact initial model-call checkpoint could not be persisted.
    ModelCall(crate::model_execution::ModelCallRepositoryError),
    /// Complete instruction evidence could not join the activation commit.
    WorkspaceInstructions(WorkspaceInstructionRepositoryError),
}

impl fmt::Display for CommitActivationPreviewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Activation(error) => error.fmt(formatter),
            Self::ModelCall(error) => error.fmt(formatter),
            Self::WorkspaceInstructions(error) => error.fmt(formatter),
        }
    }
}

impl Error for CommitActivationPreviewError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Activation(error) => Some(error),
            Self::ModelCall(error) => Some(error),
            Self::WorkspaceInstructions(error) => Some(error),
        }
    }
}

impl ClassifyOperatorFailure for CommitActivationPreviewError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Activation(error) => error.operator_failure_class(),
            Self::ModelCall(error) => error.operator_failure_class(),
            Self::WorkspaceInstructions(error) => error.operator_failure_class(),
        }
    }
}

/// Read-only exact activation candidate retained for guarded commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedActivationPreview {
    identities: AcceptedInputTurnActivationIdentities,
    prepared: PreparedTurnActivation,
}

impl PreparedActivationPreview {
    /// Borrows the exact checked candidate used for prospective model rendering.
    pub const fn prepared(&self) -> &PreparedTurnActivation {
        &self.prepared
    }
}

/// Outcome of committing a previously counted activation preview.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommitActivationPreviewOutcome {
    /// The exact preview still matched and was atomically activated.
    Activated(Box<ActivatedTurn>),
    /// Authoritative state changed after preview; the caller must restart the pass.
    Stale,
}

/// Outcome of atomically activating and failing a turn whose required
/// automatic context compaction could not complete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitCompactionFailurePreviewOutcome {
    /// The exact preview activated and terminalized as failed without a call.
    Failed(TurnId),
    /// Authoritative state changed after preview; the caller must restart the pass.
    Stale,
}

/// Outcome of atomically activating and closing the exact prospective call
/// after definitive attachment failure during provider-native counting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitCountedAttachmentFailurePreviewOutcome {
    /// The exact preview activated and its Prepared call terminalized as failed.
    Failed(TurnId),
    /// Authoritative state changed after preview; the caller must restart the pass.
    Stale,
}

enum TransactionDecision {
    Commit(StartEligibleTurnOutcome),
    Rollback(StartEligibleTurnOutcome),
}

/// PostgreSQL implementation of one authoritative session eligibility pass.
#[derive(Clone, Debug)]
pub struct StartEligibleTurnRepository {
    pool: PgPool,
}

impl StartEligibleTurnRepository {
    /// Uses the supplied pool for serialized, atomic eligibility handling.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Derives one exact activation candidate without committing it.
    pub async fn preview(
        &self,
        session: SessionId,
        identities: AcceptedInputTurnActivationIdentities,
    ) -> Result<Option<PreparedActivationPreview>, StartEligibleTurnRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let prepared = prepare_preview(&mut transaction, session, identities).await?;
        transaction.rollback().await?;
        Ok(prepared.map(|prepared| PreparedActivationPreview {
            identities,
            prepared,
        }))
    }

    /// Revalidates and atomically commits one previously counted preview.
    pub async fn commit_preview(
        &self,
        preview: PreparedActivationPreview,
    ) -> Result<CommitActivationPreviewOutcome, StartEligibleTurnRepositoryError> {
        let session = preview.prepared.turn().session();
        let mut transaction = self.pool.begin().await?;
        let session_uuid = session_id_to_uuid(session);
        let (session_exists, scheduler_session) =
            sqlx::query_as::<_, (bool, Option<Uuid>)>(crate::lock_inventory::START_ELIGIBLE_TURN)
                .bind(session_uuid)
                .fetch_one(&mut *transaction)
                .await?;
        if !session_exists
            || scheduler_session.is_none()
            || session_refuses_new_work(&mut transaction, session).await?
        {
            transaction.rollback().await?;
            return Ok(CommitActivationPreviewOutcome::Stale);
        }
        if dispatch_start_lease_is_expired(&mut transaction, session).await? {
            transaction.rollback().await?;
            return Ok(CommitActivationPreviewOutcome::Stale);
        }
        let Some(current) = prepare_preview(&mut transaction, session, preview.identities).await?
        else {
            transaction.rollback().await?;
            return Ok(CommitActivationPreviewOutcome::Stale);
        };
        if current != preview.prepared {
            transaction.rollback().await?;
            return Ok(CommitActivationPreviewOutcome::Stale);
        }
        let activated = insert_prepared_activation(&mut transaction, current).await?;
        transaction.commit().await.map_err(|error| {
            let commit_ambiguous = commit_failure_is_ambiguous(&error);
            StartEligibleTurnRepositoryError::from_database(error, commit_ambiguous)
        })?;
        Ok(CommitActivationPreviewOutcome::Activated(Box::new(
            activated,
        )))
    }

    /// Revalidates one counted preview and atomically commits both its
    /// activation and exact no-steering Prepared initial call.
    pub async fn commit_counted_preview(
        &self,
        preview: PreparedActivationPreview,
        prospective: crate::model_execution::ProspectiveModelCall,
        model_calls: &crate::model_execution::PostgresModelCallRepository,
        instruction_evidence: Option<CountedActivationInstructionEvidence<'_>>,
    ) -> Result<CommitActivationPreviewOutcome, CommitActivationPreviewError> {
        let session = preview.prepared.turn().session();
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(StartEligibleTurnRepositoryError::from)
            .map_err(CommitActivationPreviewError::Activation)?;
        let session_uuid = session_id_to_uuid(session);
        let (session_exists, scheduler_session) =
            sqlx::query_as::<_, (bool, Option<Uuid>)>(crate::lock_inventory::START_ELIGIBLE_TURN)
                .bind(session_uuid)
                .fetch_one(&mut *transaction)
                .await
                .map_err(StartEligibleTurnRepositoryError::from)
                .map_err(CommitActivationPreviewError::Activation)?;
        if !session_exists
            || scheduler_session.is_none()
            || session_refuses_new_work(&mut transaction, session)
                .await
                .map_err(CommitActivationPreviewError::Activation)?
        {
            transaction
                .rollback()
                .await
                .map_err(StartEligibleTurnRepositoryError::from)
                .map_err(CommitActivationPreviewError::Activation)?;
            return Ok(CommitActivationPreviewOutcome::Stale);
        }
        if dispatch_start_lease_is_expired(&mut transaction, session)
            .await
            .map_err(CommitActivationPreviewError::Activation)?
        {
            transaction
                .rollback()
                .await
                .map_err(StartEligibleTurnRepositoryError::from)
                .map_err(CommitActivationPreviewError::Activation)?;
            return Ok(CommitActivationPreviewOutcome::Stale);
        }
        let current = prepare_preview(&mut transaction, session, preview.identities)
            .await
            .map_err(CommitActivationPreviewError::Activation)?;
        let Some(current) = current else {
            transaction
                .rollback()
                .await
                .map_err(StartEligibleTurnRepositoryError::from)
                .map_err(CommitActivationPreviewError::Activation)?;
            return Ok(CommitActivationPreviewOutcome::Stale);
        };
        if current != preview.prepared {
            transaction
                .rollback()
                .await
                .map_err(StartEligibleTurnRepositoryError::from)
                .map_err(CommitActivationPreviewError::Activation)?;
            return Ok(CommitActivationPreviewOutcome::Stale);
        }
        let outbox_order_guard =
            crate::model_execution::acquire_model_call_outbox_order_guard(&mut transaction)
                .await
                .map_err(CommitActivationPreviewError::ModelCall)?;
        let activated = insert_prepared_activation(&mut transaction, current)
            .await
            .map_err(CommitActivationPreviewError::Activation)?;
        if let Some(evidence) = instruction_evidence {
            WorkspaceInstructionRepository::record_counted_activation_in_transaction(
                &mut transaction,
                evidence,
            )
            .await
            .map_err(CommitActivationPreviewError::WorkspaceInstructions)?;
        }
        let _ = model_calls
            .checkpoint_counted_activation_in_transaction(
                &mut transaction,
                &activated,
                &prospective,
                outbox_order_guard,
            )
            .await
            .map_err(CommitActivationPreviewError::ModelCall)?;
        transaction.commit().await.map_err(|error| {
            let commit_ambiguous = commit_failure_is_ambiguous(&error);
            CommitActivationPreviewError::Activation(
                StartEligibleTurnRepositoryError::from_database(error, commit_ambiguous),
            )
        })?;
        Ok(CommitActivationPreviewOutcome::Activated(Box::new(
            activated,
        )))
    }

    /// Revalidates one counted preview and atomically commits its activation,
    /// exact Prepared call, and definitive attachment-failure closure.
    pub async fn commit_counted_attachment_failure_preview(
        &self,
        preview: PreparedActivationPreview,
        prospective: crate::model_execution::ProspectiveModelCall,
        model_calls: &crate::model_execution::PostgresModelCallRepository,
        failure: signalbox_application::AttachmentPreparationFailure,
        identities: signalbox_domain::FailedModelCallTurnIdentities,
        instruction_evidence: Option<CountedActivationInstructionEvidence<'_>>,
    ) -> Result<CommitCountedAttachmentFailurePreviewOutcome, CommitActivationPreviewError> {
        let session = preview.prepared.turn().session();
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(StartEligibleTurnRepositoryError::from)
            .map_err(CommitActivationPreviewError::Activation)?;
        lock_delegated_child_endpoint_sessions(&mut transaction, session)
            .await
            .map_err(CommitActivationPreviewError::ModelCall)?;
        let session_uuid = session_id_to_uuid(session);
        let (session_exists, scheduler_session) =
            sqlx::query_as::<_, (bool, Option<Uuid>)>(crate::lock_inventory::START_ELIGIBLE_TURN)
                .bind(session_uuid)
                .fetch_one(&mut *transaction)
                .await
                .map_err(StartEligibleTurnRepositoryError::from)
                .map_err(CommitActivationPreviewError::Activation)?;
        if !session_exists
            || scheduler_session.is_none()
            || session_refuses_new_work(&mut transaction, session)
                .await
                .map_err(CommitActivationPreviewError::Activation)?
        {
            transaction
                .rollback()
                .await
                .map_err(StartEligibleTurnRepositoryError::from)
                .map_err(CommitActivationPreviewError::Activation)?;
            return Ok(CommitCountedAttachmentFailurePreviewOutcome::Stale);
        }
        if dispatch_start_lease_is_expired(&mut transaction, session)
            .await
            .map_err(CommitActivationPreviewError::Activation)?
        {
            transaction
                .rollback()
                .await
                .map_err(StartEligibleTurnRepositoryError::from)
                .map_err(CommitActivationPreviewError::Activation)?;
            return Ok(CommitCountedAttachmentFailurePreviewOutcome::Stale);
        }
        let Some(current) = prepare_preview(&mut transaction, session, preview.identities)
            .await
            .map_err(CommitActivationPreviewError::Activation)?
        else {
            transaction
                .rollback()
                .await
                .map_err(StartEligibleTurnRepositoryError::from)
                .map_err(CommitActivationPreviewError::Activation)?;
            return Ok(CommitCountedAttachmentFailurePreviewOutcome::Stale);
        };
        if current != preview.prepared {
            transaction
                .rollback()
                .await
                .map_err(StartEligibleTurnRepositoryError::from)
                .map_err(CommitActivationPreviewError::Activation)?;
            return Ok(CommitCountedAttachmentFailurePreviewOutcome::Stale);
        }
        let outbox_order_guard =
            crate::model_execution::acquire_model_call_outbox_order_guard(&mut transaction)
                .await
                .map_err(CommitActivationPreviewError::ModelCall)?;
        let activated = insert_prepared_activation(&mut transaction, current)
            .await
            .map_err(CommitActivationPreviewError::Activation)?;
        if let Some(evidence) = instruction_evidence {
            WorkspaceInstructionRepository::record_counted_activation_in_transaction(
                &mut transaction,
                evidence,
            )
            .await
            .map_err(CommitActivationPreviewError::WorkspaceInstructions)?;
        }
        let turn = activated.turn();
        model_calls
            .fail_counted_attachment_in_transaction(
                &mut transaction,
                &activated,
                &prospective,
                failure,
                identities,
                outbox_order_guard,
            )
            .await
            .map_err(CommitActivationPreviewError::ModelCall)?;
        transaction.commit().await.map_err(|error| {
            let commit_ambiguous = commit_failure_is_ambiguous(&error);
            CommitActivationPreviewError::Activation(
                StartEligibleTurnRepositoryError::from_database(error, commit_ambiguous),
            )
        })?;
        Ok(CommitCountedAttachmentFailurePreviewOutcome::Failed(turn))
    }

    /// Revalidates one preview and atomically closes it as a call-free failed
    /// turn after required automatic context compaction failed.
    pub async fn commit_compaction_failure_preview(
        &self,
        preview: PreparedActivationPreview,
        model_calls: &crate::model_execution::PostgresModelCallRepository,
        identities: signalbox_domain::FailedModelCallTurnIdentities,
        terminal_cause: signalbox_domain::TurnTerminalCause,
        recovery_cause: Option<crate::goal::GoalExecutionFailureRecoveryCause>,
    ) -> Result<CommitCompactionFailurePreviewOutcome, CommitActivationPreviewError> {
        let session = preview.prepared.turn().session();
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(StartEligibleTurnRepositoryError::from)
            .map_err(CommitActivationPreviewError::Activation)?;
        // A delegated-child failure publishes into its parent session. Keep
        // that endpoint pair ahead of the child scheduler in the global lock
        // order, matching every other delegated terminalization path.
        lock_delegated_child_endpoint_sessions(&mut transaction, session)
            .await
            .map_err(CommitActivationPreviewError::ModelCall)?;
        let session_uuid = session_id_to_uuid(session);
        let (session_exists, scheduler_session) =
            sqlx::query_as::<_, (bool, Option<Uuid>)>(crate::lock_inventory::START_ELIGIBLE_TURN)
                .bind(session_uuid)
                .fetch_one(&mut *transaction)
                .await
                .map_err(StartEligibleTurnRepositoryError::from)
                .map_err(CommitActivationPreviewError::Activation)?;
        if !session_exists
            || scheduler_session.is_none()
            || session_refuses_new_work(&mut transaction, session)
                .await
                .map_err(CommitActivationPreviewError::Activation)?
        {
            transaction
                .rollback()
                .await
                .map_err(StartEligibleTurnRepositoryError::from)
                .map_err(CommitActivationPreviewError::Activation)?;
            return Ok(CommitCompactionFailurePreviewOutcome::Stale);
        }
        let current = prepare_preview(&mut transaction, session, preview.identities)
            .await
            .map_err(CommitActivationPreviewError::Activation)?;
        let Some(current) = current else {
            transaction
                .rollback()
                .await
                .map_err(StartEligibleTurnRepositoryError::from)
                .map_err(CommitActivationPreviewError::Activation)?;
            return Ok(CommitCompactionFailurePreviewOutcome::Stale);
        };
        if current != preview.prepared {
            transaction
                .rollback()
                .await
                .map_err(StartEligibleTurnRepositoryError::from)
                .map_err(CommitActivationPreviewError::Activation)?;
            return Ok(CommitCompactionFailurePreviewOutcome::Stale);
        }
        let _outbox_order_guard =
            crate::model_execution::acquire_model_call_outbox_order_guard(&mut transaction)
                .await
                .map_err(CommitActivationPreviewError::ModelCall)?;
        let activated = insert_prepared_activation(&mut transaction, current)
            .await
            .map_err(CommitActivationPreviewError::Activation)?;
        let turn = activated.turn();
        model_calls
            .fail_automatic_compaction_in_transaction(
                &mut transaction,
                session,
                turn,
                identities,
                terminal_cause,
                recovery_cause,
            )
            .await
            .map_err(CommitActivationPreviewError::ModelCall)?;
        transaction.commit().await.map_err(|error| {
            let commit_ambiguous = commit_failure_is_ambiguous(&error);
            CommitActivationPreviewError::Activation(
                StartEligibleTurnRepositoryError::from_database(error, commit_ambiguous),
            )
        })?;
        Ok(CommitCompactionFailurePreviewOutcome::Failed(turn))
    }

    /// Locks one session scheduler row, reconstitutes complete scheduling
    /// state, and atomically activates the earliest eligible queued turn.
    pub async fn handle(
        &self,
        session: SessionId,
        identities: AcceptedInputTurnActivationIdentities,
    ) -> Result<StartEligibleTurnOutcome, StartEligibleTurnRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let decision = handle_in_transaction(&mut transaction, session, identities).await;

        match decision {
            Ok(TransactionDecision::Commit(outcome)) => {
                transaction.commit().await.map_err(|error| {
                    let commit_ambiguous = commit_failure_is_ambiguous(&error);
                    StartEligibleTurnRepositoryError::from_database(error, commit_ambiguous)
                })?;
                Ok(outcome)
            }
            Ok(TransactionDecision::Rollback(outcome)) => {
                transaction.rollback().await?;
                Ok(outcome)
            }
            Err(error) => {
                if let Err(rollback_error) = transaction.rollback().await {
                    return Err(rollback_error.into());
                }
                Err(error)
            }
        }
    }
}

impl StartEligibleTurnTransaction for StartEligibleTurnRepository {
    type Error = StartEligibleTurnRepositoryError;

    async fn handle(
        &mut self,
        session: SessionId,
        identities: AcceptedInputTurnActivationIdentities,
    ) -> Result<StartEligibleTurnOutcome, Self::Error> {
        StartEligibleTurnRepository::handle(self, session, identities).await
    }

    async fn handle_with_activation_observer(
        &mut self,
        session: SessionId,
        identities: AcceptedInputTurnActivationIdentities,
        observer: std::sync::Arc<dyn Fn(TurnId) + Send + Sync>,
    ) -> Result<StartEligibleTurnOutcome, Self::Error> {
        let mut transaction = self.pool.begin().await?;
        let decision = handle_in_transaction(&mut transaction, session, identities).await;

        match decision {
            Ok(TransactionDecision::Commit(outcome)) => {
                if let StartEligibleTurnOutcome::Activated(activated) = &outcome {
                    observer(activated.turn());
                }
                transaction.commit().await.map_err(|error| {
                    let commit_ambiguous = commit_failure_is_ambiguous(&error);
                    StartEligibleTurnRepositoryError::from_database(error, commit_ambiguous)
                })?;
                Ok(outcome)
            }
            Ok(TransactionDecision::Rollback(outcome)) => {
                transaction.rollback().await?;
                Ok(outcome)
            }
            Err(error) => {
                if let Err(rollback_error) = transaction.rollback().await {
                    return Err(rollback_error.into());
                }
                Err(error)
            }
        }
    }
}

async fn prepare_preview(
    connection: &mut PgConnection,
    requested_session: SessionId,
    identities: AcceptedInputTurnActivationIdentities,
) -> Result<Option<PreparedTurnActivation>, StartEligibleTurnRepositoryError> {
    let session = match load_session_from_connection(connection, requested_session).await {
        Ok(Some(session)) => session,
        Ok(None) => return Ok(None),
        Err(SessionRepositoryError::Database(error)) => return Err(error.into()),
        Err(SessionRepositoryError::Corruption(error)) => {
            return Err(StartEligibleTurnCorruption::CurrentSession(error).into());
        }
    };
    if let Some(prepared) =
        prepare_delegated_preview(connection, requested_session, identities).await?
    {
        return Ok(Some(prepared.into()));
    }
    let scheduling = load_scheduling_projection(connection, session)
        .await
        .map_err(map_scheduling_error)?;
    if let Some(prepared) =
        prepare_delegated_wake_preview(connection, requested_session, identities, &scheduling)
            .await?
    {
        return Ok(Some(prepared.into()));
    }
    match scheduling.prepare_earliest_queued_activation(identities) {
        Ok(prepared) => Ok(Some(prepared.into())),
        Err(error) => match error.failure() {
            AcceptedInputEligibilityFailure::ActiveTurnPresent { .. }
            | AcceptedInputEligibilityFailure::ContextCompactionInProgress { .. }
            | AcceptedInputEligibilityFailure::NoQueuedTurn => Ok(None),
            AcceptedInputEligibilityFailure::OriginEntryIdentityAlreadyExists => {
                Err(StartEligibleTurnRepositoryError::IdentityCollision(
                    StartEligibleTurnIdentityCollision::OriginEntry,
                ))
            }
            AcceptedInputEligibilityFailure::ModelIdentityEntryIdentityAlreadyExists => {
                Err(StartEligibleTurnRepositoryError::IdentityCollision(
                    StartEligibleTurnIdentityCollision::ModelIdentityEntry,
                ))
            }
            AcceptedInputEligibilityFailure::StartingFrontierIdentityAlreadyExists => {
                Err(StartEligibleTurnRepositoryError::IdentityCollision(
                    StartEligibleTurnIdentityCollision::StartingFrontier,
                ))
            }
            AcceptedInputEligibilityFailure::InitialAttemptIdentityAlreadyExists => {
                Err(StartEligibleTurnRepositoryError::IdentityCollision(
                    StartEligibleTurnIdentityCollision::InitialAttempt,
                ))
            }
            AcceptedInputEligibilityFailure::InternalOriginFrontierConstructionFailed => Err(
                StartEligibleTurnCorruption::Inconsistent("origin frontier construction").into(),
            ),
            AcceptedInputEligibilityFailure::InternalPredecessorTerminalFrontierMissing {
                ..
            } => Err(
                StartEligibleTurnCorruption::Inconsistent("predecessor terminal frontier").into(),
            ),
            AcceptedInputEligibilityFailure::InternalStartingFrontierDerivationFailed => Err(
                StartEligibleTurnCorruption::Inconsistent("starting frontier derivation").into(),
            ),
        },
    }
}

async fn prepare_delegated_preview(
    connection: &mut PgConnection,
    session: SessionId,
    identities: AcceptedInputTurnActivationIdentities,
) -> Result<Option<PreparedDelegatedTurnActivation>, StartEligibleTurnRepositoryError> {
    let row = sqlx::query(
        "SELECT
            task.spawning_tool_request_id,
            task.turn_id,
            task.semantic_entry_id,
            task.task_content,
            relation.parent_session_id,
            relation.parent_turn_id,
            defaults.session_id AS goal_defaults_session_id,
            task.defaults_version AS queued_defaults_version,
            defaults.version AS goal_defaults_version,
            defaults.model_selection_kind AS goal_defaults_model_kind,
            defaults.direct_model_selection_id AS goal_defaults_direct_id,
            defaults.model_alias_id AS goal_defaults_alias_id,
            defaults.dangerous_tool_auto_approval AS goal_defaults_tool_auto_approval,
            defaults.model_settings AS goal_defaults_model_settings,
            task.requested_model_kind,
            task.requested_direct_model_selection_id,
            task.requested_model_alias_id,
            task.frozen_model_kind,
            task.frozen_direct_model_selection_id,
            task.frozen_model_alias_id,
            task.frozen_alias_selected_direct_id
         FROM session_delegation_initial_task AS task
         JOIN session_delegation AS relation
           ON relation.spawning_tool_request_id = task.spawning_tool_request_id
          AND relation.child_session_id = task.child_session_id
         JOIN turn_lifecycle AS lifecycle
           ON lifecycle.turn_id = task.turn_id
          AND lifecycle.session_id = task.child_session_id
          AND lifecycle.acceptance_position = task.admission_position
          AND lifecycle.origin_kind = 'delegation'
          AND lifecycle.state_kind = 'queued'
         JOIN session_defaults_version AS defaults
           ON defaults.session_id = task.child_session_id
          AND defaults.version = task.defaults_version
        WHERE task.child_session_id = $1
          AND accepted_input_turn_is_first_nonterminal(
                task.child_session_id, task.turn_id
          )
          AND NOT EXISTS (
                SELECT 1 FROM turn_lifecycle AS active
                 WHERE active.session_id = task.child_session_id
                   AND active.state_kind = 'active'
                   AND NOT active.delegation_runtime_terminal
          )
          AND NOT EXISTS (
                SELECT 1 FROM context_compaction_model_call AS compaction
                 WHERE compaction.session_id = task.child_session_id
                   AND compaction.state_kind <> 'terminal'
          )",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let starting_frontier = identities.starting_frontier();
    let initial_attempt = identities.initial_attempt();
    let collisions = sqlx::query_as::<_, (bool, bool)>(
        "SELECT
            EXISTS (
                SELECT 1 FROM context_frontier
                 WHERE owning_session_id = $1 AND context_frontier_id = $2
            ),
            EXISTS (
                SELECT 1 FROM turn_attempt WHERE turn_attempt_id = $3
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(starting_frontier.into_uuid())
    .bind(initial_attempt.into_uuid())
    .fetch_one(&mut *connection)
    .await?;
    if collisions.0 {
        return Err(StartEligibleTurnRepositoryError::IdentityCollision(
            StartEligibleTurnIdentityCollision::StartingFrontier,
        ));
    }
    if collisions.1 {
        return Err(StartEligibleTurnRepositoryError::IdentityCollision(
            StartEligibleTurnIdentityCollision::InitialAttempt,
        ));
    }
    let spawning_request = ToolRequestId::from_uuid(row.try_get("spawning_tool_request_id")?);
    let task = DelegationContent::try_new(row.try_get("task_content")?)
        .map_err(|_| StartEligibleTurnCorruption::Inconsistent("delegated task content"))?;
    let configuration =
        decode_goal_origin_configuration(&row, session).map_err(map_scheduling_error)?;
    let task_entry = SemanticTranscriptEntryReconstitutionInput::new(
        SemanticTranscriptEntryId::from_uuid(row.try_get("semantic_entry_id")?),
        session,
        InitialSemanticTranscriptEntryPayload::DelegatedTask {
            spawning_request,
            parent_session: SessionId::from_uuid(row.try_get("parent_session_id")?),
            parent_turn: TurnId::from_uuid(row.try_get("parent_turn_id")?),
            content: task.clone(),
        },
    );
    PreparedDelegatedTurnActivation::prepare(DelegatedTurnActivationInput {
        session,
        turn: TurnId::from_uuid(row.try_get("turn_id")?),
        spawning_request,
        task,
        task_entry,
        configuration,
        starting_frontier,
        initial_attempt,
    })
    .map(Some)
    .ok_or_else(|| {
        StartEligibleTurnCorruption::Inconsistent("delegated activation projection").into()
    })
}

async fn prepare_delegated_wake_preview(
    connection: &mut PgConnection,
    session: SessionId,
    identities: AcceptedInputTurnActivationIdentities,
    scheduling: &AcceptedInputSchedulingProjection,
) -> Result<Option<PreparedDelegatedTurnActivation>, StartEligibleTurnRepositoryError> {
    let row = sqlx::query(
        "SELECT
            wake.turn_id,
            wake.first_delivery_sequence,
            wake.through_delivery_sequence,
            predecessor.turn_id AS predecessor_turn_id,
            turn_lifecycle_effective_terminal_frontier(
                predecessor.session_id, predecessor.turn_id
            ) AS predecessor_frontier_id,
            defaults.session_id AS goal_defaults_session_id,
            wake.defaults_version AS queued_defaults_version,
            defaults.version AS goal_defaults_version,
            defaults.model_selection_kind AS goal_defaults_model_kind,
            defaults.direct_model_selection_id AS goal_defaults_direct_id,
            defaults.model_alias_id AS goal_defaults_alias_id,
            defaults.dangerous_tool_auto_approval AS goal_defaults_tool_auto_approval,
            defaults.model_settings AS goal_defaults_model_settings,
            wake.requested_model_kind,
            wake.requested_direct_model_selection_id,
            wake.requested_model_alias_id,
            wake.frozen_model_kind,
            wake.frozen_direct_model_selection_id,
            wake.frozen_model_alias_id,
            wake.frozen_alias_selected_direct_id
         FROM session_delegation_wake_turn_origin AS wake
         JOIN turn_lifecycle AS lifecycle
           ON lifecycle.turn_id = wake.turn_id
          AND lifecycle.session_id = wake.recipient_session_id
          AND lifecycle.acceptance_position = wake.admission_position
          AND lifecycle.origin_kind = 'delegation'
          AND lifecycle.state_kind = 'queued'
         JOIN turn_lifecycle AS predecessor
           ON predecessor.turn_id = accepted_input_turn_queue_predecessor(
                wake.recipient_session_id, wake.turn_id
           )
          AND predecessor.session_id = wake.recipient_session_id
          AND (
                predecessor.delegation_runtime_terminal
                OR (
                    predecessor.state_kind = 'terminal'
                    AND predecessor.terminal_disposition_kind IN (
                        'failed', 'completed', 'refused', 'cancelled',
                        'reconciliation_required'
                    )
                )
          )
         JOIN session_defaults_version AS defaults
           ON defaults.session_id = wake.recipient_session_id
          AND defaults.version = wake.defaults_version
        WHERE wake.recipient_session_id = $1
          AND accepted_input_turn_is_first_nonterminal(
                wake.recipient_session_id, wake.turn_id
          )
          AND NOT EXISTS (
                SELECT 1 FROM turn_lifecycle AS active
                 WHERE active.session_id = wake.recipient_session_id
                   AND active.state_kind = 'active'
                   AND NOT active.delegation_runtime_terminal
          )
          AND NOT EXISTS (
                SELECT 1 FROM context_compaction_model_call AS compaction
                 WHERE compaction.session_id = wake.recipient_session_id
                   AND compaction.state_kind <> 'terminal'
          )",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let first_numeric: Decimal = row.try_get("first_delivery_sequence")?;
    let through_numeric: Decimal = row.try_get("through_delivery_sequence")?;
    let first = NonZeroU64::new(
        positive_u64_from_numeric(first_numeric)
            .map_err(|_| StartEligibleTurnCorruption::Inconsistent("wake delivery range"))?,
    )
    .ok_or(StartEligibleTurnCorruption::Inconsistent(
        "wake delivery range",
    ))?;
    let through = NonZeroU64::new(
        positive_u64_from_numeric(through_numeric)
            .map_err(|_| StartEligibleTurnCorruption::Inconsistent("wake delivery range"))?,
    )
    .ok_or(StartEligibleTurnCorruption::Inconsistent(
        "wake delivery range",
    ))?;
    let delivery_rows = sqlx::query_as::<_, (Decimal, Uuid)>(
        "SELECT pending.delivery_sequence,
                delegation_delivery_semantic_entry(
                    pending.recipient_session_id, pending.delivery_sequence
                )
           FROM session_pending_delivery AS pending
          WHERE pending.recipient_session_id = $1
            AND pending.delivery_sequence BETWEEN $2 AND $3
          ORDER BY pending.delivery_sequence",
    )
    .bind(session_id_to_uuid(session))
    .bind(first_numeric)
    .bind(through_numeric)
    .fetch_all(&mut *connection)
    .await?;
    let mut deliveries = Vec::with_capacity(delivery_rows.len());
    for (_, entry) in delivery_rows {
        let reference = SemanticTranscriptEntryRef::from_source(
            session,
            SemanticTranscriptEntryId::from_uuid(entry),
        );
        let semantic = scheduling
            .semantic_entry(reference)
            .ok_or(StartEligibleTurnCorruption::Missing("wake semantic entry"))?;
        deliveries.push(SemanticTranscriptEntryReconstitutionInput::new(
            semantic.identity(),
            semantic.source_session(),
            semantic.payload().clone(),
        ));
    }
    let predecessor = TurnId::from_uuid(row.try_get("predecessor_turn_id")?);
    let predecessor_frontier =
        signalbox_domain::ContextFrontierId::from_uuid(row.try_get("predecessor_frontier_id")?);
    let predecessor_snapshot = scheduling
        .resolved_snapshot(predecessor_frontier)
        .cloned()
        .ok_or(StartEligibleTurnCorruption::Missing(
            "wake predecessor snapshot",
        ))?;
    let configuration =
        decode_goal_origin_configuration(&row, session).map_err(map_scheduling_error)?;
    PreparedDelegatedTurnActivation::prepare_wake(DelegatedWakeTurnActivationInput {
        session,
        turn: TurnId::from_uuid(row.try_get("turn_id")?),
        first_delivery_sequence: first,
        through_delivery_sequence: through,
        deliveries,
        predecessor,
        predecessor_snapshot,
        configuration,
        starting_frontier: identities.starting_frontier(),
        initial_attempt: identities.initial_attempt(),
    })
    .map(Some)
    .ok_or_else(|| {
        StartEligibleTurnCorruption::Inconsistent("delegated wake activation projection").into()
    })
}

async fn dispatch_start_lease_is_expired(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<bool, StartEligibleTurnRepositoryError> {
    sqlx::query_scalar(crate::lock_inventory::EXPIRED_DISPATCH_START_LEASE)
        .bind(session_id_to_uuid(session))
        .fetch_one(connection)
        .await
        .map_err(Into::into)
}

/// Whether the locked session is suspended in place.
/// A held start gate keeps queued input from activating until `release_start`,
/// including across a module park and resume.
async fn session_start_gate_is_held(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<bool, StartEligibleTurnRepositoryError> {
    let held: Option<bool> =
        sqlx::query_scalar("SELECT start_gate_held FROM session_lifecycle WHERE session_id = $1")
            .bind(session_id_to_uuid(session))
            .fetch_optional(&mut *connection)
            .await?;
    Ok(held.unwrap_or(false))
}

/// Whether the session takes no new work: it is parked, or a closure already
/// committed to an outcome and is waiting only for the live turn to settle.
///
/// Activating a successor under a committed handoff is what makes the
/// settlement impossible: the terminal write would then find a live turn, and
/// the next queued turn would do it again.
async fn session_refuses_new_work(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<bool, StartEligibleTurnRepositoryError> {
    let refuses: Option<bool> = sqlx::query_scalar(
        "SELECT state_kind = 'parked' OR pending_terminal_outcome_kind IS NOT NULL
           FROM session_lifecycle WHERE session_id = $1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?;
    Ok(refuses.unwrap_or(false))
}

async fn handle_in_transaction(
    connection: &mut PgConnection,
    requested_session: SessionId,
    identities: AcceptedInputTurnActivationIdentities,
) -> Result<TransactionDecision, StartEligibleTurnRepositoryError> {
    // Lock inventory for this transaction: the `session_scheduler` row below
    // is its only explicit row lock; the session row is locked only
    // `KEY SHARE`, implicitly, by the inserts' session foreign keys; and the
    // candidate `turn_lifecycle` row is locked `NO KEY UPDATE` by the guarded
    // activation UPDATE itself (plus `KEY SHARE` from the `turn_attempt`
    // insert's foreign key). Two standing constraints: every turn-lifecycle
    // writer acquires this scheduler lock before touching `turn_lifecycle`
    // rows, and no production path may take the strongest row-lock mode on the
    // session row —
    // see the lock-mode contract beside the session-row lock in
    // `submit_input.rs::prepare_against_locked_state`.
    let session_uuid = session_id_to_uuid(requested_session);
    let (session_exists, scheduler_session) =
        sqlx::query_as::<_, (bool, Option<Uuid>)>(crate::lock_inventory::START_ELIGIBLE_TURN)
            .bind(session_uuid)
            .fetch_one(&mut *connection)
            .await?;

    if scheduler_session.is_none() {
        if session_exists {
            return Err(StartEligibleTurnCorruption::Missing("session scheduler row").into());
        }
        return Ok(TransactionDecision::Rollback(
            StartEligibleTurnOutcome::NoEligibleTurn,
        ));
    }
    // The sweep's parked exclusion is a hint filter, not an authority: a hint
    // queued before the park still reaches this transaction. The satellite row
    // is already locked by the scheduler statement above, so this reads under
    // that lock rather than racing it.
    if session_refuses_new_work(connection, requested_session).await?
        || session_start_gate_is_held(connection, requested_session).await?
    {
        return Ok(TransactionDecision::Rollback(
            StartEligibleTurnOutcome::NoEligibleTurn,
        ));
    }

    if dispatch_start_lease_is_expired(connection, requested_session).await? {
        return Ok(TransactionDecision::Rollback(
            StartEligibleTurnOutcome::NoEligibleTurn,
        ));
    }

    let session = match load_session_from_connection(connection, requested_session).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            return Err(
                StartEligibleTurnCorruption::Inconsistent("locked session disappeared").into(),
            );
        }
        Err(SessionRepositoryError::Database(error)) => return Err(error.into()),
        Err(SessionRepositoryError::Corruption(error)) => {
            return Err(StartEligibleTurnCorruption::CurrentSession(error).into());
        }
    };
    if let Some(prepared) =
        prepare_delegated_preview(connection, requested_session, identities).await?
    {
        let activated = insert_prepared_activation(connection, prepared.into()).await?;
        return Ok(TransactionDecision::Commit(
            StartEligibleTurnOutcome::Activated(Box::new(activated)),
        ));
    }
    let scheduling = load_scheduling_projection(connection, session)
        .await
        .map_err(map_scheduling_error)?;
    if let Some(prepared) =
        prepare_delegated_wake_preview(connection, requested_session, identities, &scheduling)
            .await?
    {
        let activated = insert_prepared_activation(connection, prepared.into()).await?;
        return Ok(TransactionDecision::Commit(
            StartEligibleTurnOutcome::Activated(Box::new(activated)),
        ));
    }

    let prepared = match scheduling.prepare_earliest_queued_activation(identities) {
        Ok(prepared) => prepared,
        Err(error) => {
            let outcome = match error.failure() {
                AcceptedInputEligibilityFailure::ActiveTurnPresent { .. }
                | AcceptedInputEligibilityFailure::ContextCompactionInProgress { .. }
                | AcceptedInputEligibilityFailure::NoQueuedTurn => {
                    return Ok(TransactionDecision::Rollback(
                        StartEligibleTurnOutcome::NoEligibleTurn,
                    ));
                }
                AcceptedInputEligibilityFailure::OriginEntryIdentityAlreadyExists => {
                    StartEligibleTurnIdentityCollision::OriginEntry
                }
                AcceptedInputEligibilityFailure::ModelIdentityEntryIdentityAlreadyExists => {
                    StartEligibleTurnIdentityCollision::ModelIdentityEntry
                }
                AcceptedInputEligibilityFailure::StartingFrontierIdentityAlreadyExists => {
                    StartEligibleTurnIdentityCollision::StartingFrontier
                }
                AcceptedInputEligibilityFailure::InitialAttemptIdentityAlreadyExists => {
                    StartEligibleTurnIdentityCollision::InitialAttempt
                }
                AcceptedInputEligibilityFailure::InternalOriginFrontierConstructionFailed => {
                    return Err(StartEligibleTurnCorruption::Inconsistent(
                        "origin frontier construction",
                    )
                    .into());
                }
                AcceptedInputEligibilityFailure::InternalPredecessorTerminalFrontierMissing {
                    ..
                } => {
                    return Err(StartEligibleTurnCorruption::Inconsistent(
                        "predecessor terminal frontier",
                    )
                    .into());
                }
                AcceptedInputEligibilityFailure::InternalStartingFrontierDerivationFailed => {
                    return Err(StartEligibleTurnCorruption::Inconsistent(
                        "starting frontier derivation",
                    )
                    .into());
                }
            };
            return Err(StartEligibleTurnRepositoryError::IdentityCollision(outcome));
        }
    };

    let activated = insert_prepared_activation(connection, prepared.into()).await?;
    Ok(TransactionDecision::Commit(
        StartEligibleTurnOutcome::Activated(Box::new(activated)),
    ))
}

async fn insert_prepared_activation(
    connection: &mut PgConnection,
    prepared: PreparedTurnActivation,
) -> Result<ActivatedTurn, StartEligibleTurnRepositoryError> {
    match prepared {
        PreparedTurnActivation::Accepted(prepared) => {
            insert_prepared_accepted_activation(connection, *prepared)
                .await
                .map(Into::into)
        }
        PreparedTurnActivation::Delegated(prepared) => {
            insert_prepared_delegated_activation(connection, *prepared)
                .await
                .map(Into::into)
        }
    }
}

async fn insert_prepared_accepted_activation(
    connection: &mut PgConnection,
    prepared: PreparedAcceptedInputTurnActivation,
) -> Result<signalbox_domain::ActivatedAcceptedInputTurn, StartEligibleTurnRepositoryError> {
    let (activated, starting_entries, starting_snapshot) = prepared.into_parts();
    let Some(origin_entry) = starting_entries.last() else {
        return Err(StartEligibleTurnRepositoryError::HubInvariant(
            "prepared activation entries",
        ));
    };
    let accepted_input = match origin_entry.payload() {
        InitialSemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input } => {
            *accepted_input
        }
        InitialSemanticTranscriptEntryPayload::Imported { .. }
        | InitialSemanticTranscriptEntryPayload::DelegatedTask { .. }
        | InitialSemanticTranscriptEntryPayload::DelegationMessage { .. }
        | InitialSemanticTranscriptEntryPayload::DelegationResult { .. }
        | InitialSemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
        | InitialSemanticTranscriptEntryPayload::ContextSummary { .. }
        | InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
        | InitialSemanticTranscriptEntryPayload::TurnFailed { .. }
        | InitialSemanticTranscriptEntryPayload::TurnCancelled { .. }
        | InitialSemanticTranscriptEntryPayload::AssistantText { .. }
        | InitialSemanticTranscriptEntryPayload::ProviderCompaction { .. }
        | InitialSemanticTranscriptEntryPayload::AssistantToolUse { .. }
        | InitialSemanticTranscriptEntryPayload::ToolExecutionResult { .. }
        | InitialSemanticTranscriptEntryPayload::ToolDenied { .. }
        | InitialSemanticTranscriptEntryPayload::ToolClosed { .. }
        | InitialSemanticTranscriptEntryPayload::TurnCompleted { .. } => {
            return Err(StartEligibleTurnRepositoryError::HubInvariant(
                "prepared origin-entry payload",
            ));
        }
    };
    let session = activated.session();
    if origin_entry.source_session() != session
        || starting_snapshot.frontier().owning_session() != session
    {
        return Err(StartEligibleTurnRepositoryError::HubInvariant(
            "prepared activation ownership",
        ));
    }

    for entry in &starting_entries {
        match entry.payload() {
            InitialSemanticTranscriptEntryPayload::ModelIdentityChanged {
                turn,
                defaults_version,
                selected,
            } => {
                if *turn != activated.turn() {
                    return Err(StartEligibleTurnRepositoryError::HubInvariant(
                        "prepared model-identity turn",
                    ));
                }
                sqlx::query(
                    "INSERT INTO semantic_transcript_entry
                        (source_session_id, semantic_entry_id, payload_kind,
                         model_identity_turn_id, model_identity_defaults_version,
                         model_identity_direct_selection_id)
                     VALUES ($1, $2, 'model_identity_changed', $3, $4, $5)",
                )
                .bind(session_id_to_uuid(entry.source_session()))
                .bind(entry.identity().into_uuid())
                .bind(turn_id_to_uuid(*turn))
                .bind(defaults_version_to_numeric(*defaults_version))
                .bind(selected.into_uuid())
                .execute(&mut *connection)
                .await
                .map_err(|error| {
                    semantic_entry_insert_error(
                        error,
                        StartEligibleTurnIdentityCollision::ModelIdentityEntry,
                    )
                })?;
            }
            InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: entry_accepted_input,
            } if *entry_accepted_input == accepted_input => {
                sqlx::query(
                    "INSERT INTO semantic_transcript_entry
                        (source_session_id, semantic_entry_id, payload_kind,
                         origin_accepted_input_id)
                     VALUES ($1, $2, 'origin_accepted_input', $3)",
                )
                .bind(session_id_to_uuid(entry.source_session()))
                .bind(entry.identity().into_uuid())
                .bind(entry_accepted_input.into_uuid())
                .execute(&mut *connection)
                .await
                .map_err(|error| {
                    semantic_entry_insert_error(
                        error,
                        StartEligibleTurnIdentityCollision::OriginEntry,
                    )
                })?;
            }
            InitialSemanticTranscriptEntryPayload::Imported { .. }
            | InitialSemanticTranscriptEntryPayload::DelegatedTask { .. }
            | InitialSemanticTranscriptEntryPayload::DelegationMessage { .. }
            | InitialSemanticTranscriptEntryPayload::DelegationResult { .. }
            | InitialSemanticTranscriptEntryPayload::OriginAcceptedInput { .. }
            | InitialSemanticTranscriptEntryPayload::ContextSummary { .. }
            | InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
            | InitialSemanticTranscriptEntryPayload::TurnFailed { .. }
            | InitialSemanticTranscriptEntryPayload::TurnCancelled { .. }
            | InitialSemanticTranscriptEntryPayload::AssistantText { .. }
            | InitialSemanticTranscriptEntryPayload::ProviderCompaction { .. }
            | InitialSemanticTranscriptEntryPayload::AssistantToolUse { .. }
            | InitialSemanticTranscriptEntryPayload::ToolExecutionResult { .. }
            | InitialSemanticTranscriptEntryPayload::ToolDenied { .. }
            | InitialSemanticTranscriptEntryPayload::ToolClosed { .. }
            | InitialSemanticTranscriptEntryPayload::TurnCompleted { .. } => {
                return Err(StartEligibleTurnRepositoryError::HubInvariant(
                    "prepared activation-entry payload",
                ));
            }
        }
    }

    let member_count = u64::try_from(starting_snapshot.entry_count()).map_err(|_| {
        StartEligibleTurnRepositoryError::HubInvariant("starting frontier member count")
    })?;
    let appended_entry_count = starting_snapshot.appended_entries().len();
    let prefix_member_count = starting_snapshot
        .entry_count()
        .checked_sub(appended_entry_count)
        .and_then(|count| u64::try_from(count).ok())
        .ok_or(StartEligibleTurnRepositoryError::HubInvariant(
            "starting frontier prefix member count",
        ))?;
    insert_snapshot_append(
        connection,
        SnapshotAppend {
            owning_session: session,
            frontier: starting_snapshot.frontier().snapshot(),
            prefix: starting_snapshot
                .immediate_semantic_prefix()
                .map(|prefix| prefix.snapshot()),
            member_count,
            prefix_member_count,
            appended_entries: starting_snapshot.appended_entries(),
        },
    )
    .await
    .map_err(|error| match error {
        SnapshotAppendError::FrontierInsert(error) | SnapshotAppendError::MemberInsert(error) => {
            error.into()
        }
        SnapshotAppendError::MemberPositionOverflow => {
            StartEligibleTurnRepositoryError::HubInvariant("starting frontier member position")
        }
    })?;

    let initial_attempt = match activated.phase() {
        ActiveTurnPhase::Running { current_attempt }
            if current_attempt.state() == &CurrentTurnAttemptState::Prepared =>
        {
            current_attempt.id()
        }
        ActiveTurnPhase::Running { .. }
        | ActiveTurnPhase::AwaitingApproval { .. }
        | ActiveTurnPhase::AwaitingChild { .. }
        | ActiveTurnPhase::AwaitingRecoveryDecision { .. }
        | ActiveTurnPhase::AwaitingRunnerRecovery { .. } => {
            return Err(StartEligibleTurnRepositoryError::HubInvariant(
                "prepared initial active phase",
            ));
        }
    };
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(initial_attempt.into_uuid())
    .bind(turn_id_to_uuid(activated.turn()))
    .bind(session_id_to_uuid(session))
    .execute(&mut *connection)
    .await?;

    let (lineage_kind, predecessor) = match activated.start().lineage() {
        AcceptedInputStartingLineage::FirstInSession => ("first_in_session", None),
        AcceptedInputStartingLineage::After {
            immediate_predecessor,
        } => ("after", Some(turn_id_to_uuid(immediate_predecessor))),
    };
    let updated = sqlx::query(
        "UPDATE turn_lifecycle AS candidate
            SET state_kind = 'active',
                start_lineage_kind = $1,
                immediate_predecessor_turn_id = $2,
                starting_frontier_id = $3,
                active_phase_kind = 'running',
                current_attempt_id = $4
          WHERE candidate.turn_id = $5
            AND candidate.session_id = $6
            AND candidate.origin_accepted_input_id = $7
            AND candidate.acceptance_position = $8
            AND candidate.state_kind = 'queued'
            AND goal_turn_is_runtime_relevant(
                candidate.session_id,
                candidate.turn_id
            )
            AND NOT EXISTS (
                SELECT 1
                  FROM turn_lifecycle AS active
                 WHERE active.session_id = candidate.session_id
                   AND active.state_kind = 'active'
                   AND NOT active.delegation_runtime_terminal
            )
            AND accepted_input_turn_is_first_nonterminal(
                candidate.session_id,
                candidate.turn_id
            )
            AND (
                (
                    $1 = 'first_in_session'
                    AND $2::uuid IS NULL
                    AND accepted_input_turn_queue_predecessor(
                        candidate.session_id,
                        candidate.turn_id
                    ) IS NULL
                )
                OR
                (
                    $1 = 'after'
                    AND $2::uuid = accepted_input_turn_queue_predecessor(
                        candidate.session_id,
                        candidate.turn_id
                    )
                    AND EXISTS (
                        SELECT 1
                          FROM turn_lifecycle AS predecessor
                         WHERE predecessor.turn_id = $2::uuid
                           AND predecessor.session_id = candidate.session_id
                           AND (
                                predecessor.state_kind = 'terminal'
                                OR predecessor.delegation_runtime_terminal
                           )
                    )
                )
            )",
    )
    .bind(lineage_kind)
    .bind(predecessor)
    .bind(starting_snapshot.frontier().snapshot().into_uuid())
    .bind(initial_attempt.into_uuid())
    .bind(turn_id_to_uuid(activated.turn()))
    .bind(session_id_to_uuid(session))
    .bind(activated.accepted_input().id().into_uuid())
    .bind(input_position_to_numeric(
        activated.order().acceptance_position(),
    ))
    .execute(&mut *connection)
    .await?
    .rows_affected();

    match updated {
        1 => {
            outbox::append(
                connection,
                OutboxEvent::TurnActivated {
                    session,
                    turn: activated.turn(),
                    current_attempt: initial_attempt,
                },
            )
            .await?;
            Ok(activated)
        }
        0 => Err(
            StartEligibleTurnCorruption::Inconsistent("guarded activation matched no row").into(),
        ),
        _ => Err(StartEligibleTurnRepositoryError::HubInvariant(
            "guarded activation cardinality",
        )),
    }
}

async fn insert_prepared_delegated_activation(
    connection: &mut PgConnection,
    prepared: PreparedDelegatedTurnActivation,
) -> Result<signalbox_domain::ActivatedDelegatedTurn, StartEligibleTurnRepositoryError> {
    let (activated, starting_entries, starting_snapshot) = prepared.into_parts();
    let session = activated.session();
    if starting_entries
        .iter()
        .any(|entry| entry.source_session() != session)
        || starting_snapshot.frontier().owning_session() != session
    {
        return Err(StartEligibleTurnRepositoryError::HubInvariant(
            "prepared delegated activation ownership",
        ));
    }
    if let Some(spawning_request) = activated.spawning_request() {
        let [task_entry] = starting_entries.as_slice() else {
            return Err(StartEligibleTurnRepositoryError::HubInvariant(
                "prepared delegated task entries",
            ));
        };
        let task_exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                SELECT 1
                  FROM semantic_transcript_entry AS entry
                  JOIN session_delegation_initial_task AS task
                    ON task.child_session_id = entry.source_session_id
                   AND task.semantic_entry_id = entry.semantic_entry_id
                   AND task.spawning_tool_request_id =
                        entry.delegated_task_spawning_tool_request_id
                 WHERE entry.source_session_id = $1
                   AND entry.semantic_entry_id = $2
                   AND entry.payload_kind = 'delegated_task'
                   AND task.turn_id = $3
                   AND task.spawning_tool_request_id = $4
            )",
        )
        .bind(session_id_to_uuid(session))
        .bind(task_entry.identity().into_uuid())
        .bind(turn_id_to_uuid(activated.turn()))
        .bind(spawning_request.into_uuid())
        .fetch_one(&mut *connection)
        .await?;
        if !task_exists {
            return Err(
                StartEligibleTurnCorruption::Missing("delegated task semantic entry").into(),
            );
        }
    }

    let member_count = u64::try_from(starting_snapshot.entry_count()).map_err(|_| {
        StartEligibleTurnRepositoryError::HubInvariant("delegated starting member count")
    })?;
    let prefix = starting_snapshot.immediate_semantic_prefix();
    let prefix_member_count = prefix.map_or(0, |_| {
        starting_snapshot
            .entry_count()
            .saturating_sub(starting_snapshot.appended_entries().len())
    });
    let prefix_member_count = u64::try_from(prefix_member_count).map_err(|_| {
        StartEligibleTurnRepositoryError::HubInvariant("delegated prefix member count")
    })?;
    insert_snapshot_append(
        connection,
        SnapshotAppend {
            owning_session: session,
            frontier: starting_snapshot.frontier().snapshot(),
            prefix: prefix.map(|frontier| frontier.snapshot()),
            member_count,
            prefix_member_count,
            appended_entries: starting_snapshot.appended_entries(),
        },
    )
    .await
    .map_err(|error| match error {
        SnapshotAppendError::FrontierInsert(error) | SnapshotAppendError::MemberInsert(error) => {
            error.into()
        }
        SnapshotAppendError::MemberPositionOverflow => {
            StartEligibleTurnRepositoryError::HubInvariant(
                "delegated starting frontier member position",
            )
        }
    })?;

    let initial_attempt = match activated.phase() {
        ActiveTurnPhase::Running { current_attempt }
            if current_attempt.state() == &CurrentTurnAttemptState::Prepared =>
        {
            current_attempt.id()
        }
        ActiveTurnPhase::Running { .. }
        | ActiveTurnPhase::AwaitingApproval { .. }
        | ActiveTurnPhase::AwaitingChild { .. }
        | ActiveTurnPhase::AwaitingRecoveryDecision { .. }
        | ActiveTurnPhase::AwaitingRunnerRecovery { .. } => {
            return Err(StartEligibleTurnRepositoryError::HubInvariant(
                "prepared delegated initial phase",
            ));
        }
    };
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(initial_attempt.into_uuid())
    .bind(turn_id_to_uuid(activated.turn()))
    .bind(session_id_to_uuid(session))
    .execute(&mut *connection)
    .await?;

    let updated = match (
        activated.spawning_request(),
        activated.delivery_range(),
        activated.start().lineage(),
    ) {
        (Some(spawning_request), None, AcceptedInputStartingLineage::FirstInSession) => {
            sqlx::query(
                "UPDATE turn_lifecycle AS candidate
                SET state_kind = 'active',
                    start_lineage_kind = 'first_in_session',
                    immediate_predecessor_turn_id = NULL,
                    starting_frontier_id = $1,
                    active_phase_kind = 'running',
                    current_attempt_id = $2
              WHERE candidate.turn_id = $3
                AND candidate.session_id = $4
                AND candidate.origin_kind = 'delegation'
                AND candidate.origin_accepted_input_id IS NULL
                AND candidate.acceptance_position = 1
                AND candidate.state_kind = 'queued'
                AND accepted_input_turn_is_first_nonterminal(
                    candidate.session_id, candidate.turn_id
                )
                AND accepted_input_turn_queue_predecessor(
                    candidate.session_id, candidate.turn_id
                ) IS NULL
                AND NOT EXISTS (
                    SELECT 1 FROM turn_lifecycle AS active
                     WHERE active.session_id = candidate.session_id
                       AND active.state_kind = 'active'
                       AND NOT active.delegation_runtime_terminal
                )
                AND EXISTS (
                    SELECT 1 FROM session_delegation_initial_task AS task
                     WHERE task.turn_id = candidate.turn_id
                       AND task.child_session_id = candidate.session_id
                       AND task.spawning_tool_request_id = $5
                )",
            )
            .bind(starting_snapshot.frontier().snapshot().into_uuid())
            .bind(initial_attempt.into_uuid())
            .bind(turn_id_to_uuid(activated.turn()))
            .bind(session_id_to_uuid(session))
            .bind(spawning_request.into_uuid())
            .execute(&mut *connection)
            .await?
            .rows_affected()
        }
        (
            None,
            Some((first, through)),
            AcceptedInputStartingLineage::After {
                immediate_predecessor,
            },
        ) => sqlx::query(
            "UPDATE turn_lifecycle AS candidate
                SET state_kind = 'active',
                    start_lineage_kind = 'after',
                    immediate_predecessor_turn_id = $5,
                    starting_frontier_id = $1,
                    active_phase_kind = 'running',
                    current_attempt_id = $2
              WHERE candidate.turn_id = $3
                AND candidate.session_id = $4
                AND candidate.origin_kind = 'delegation'
                AND candidate.origin_accepted_input_id IS NULL
                AND candidate.state_kind = 'queued'
                AND accepted_input_turn_is_first_nonterminal(
                    candidate.session_id, candidate.turn_id
                )
                AND accepted_input_turn_queue_predecessor(
                    candidate.session_id, candidate.turn_id
                ) = $5
                AND NOT EXISTS (
                    SELECT 1 FROM turn_lifecycle AS active
                     WHERE active.session_id = candidate.session_id
                       AND active.state_kind = 'active'
                       AND NOT active.delegation_runtime_terminal
                )
                AND EXISTS (
                    SELECT 1 FROM session_delegation_wake_turn_origin AS wake
                     WHERE wake.turn_id = candidate.turn_id
                       AND wake.recipient_session_id = candidate.session_id
                       AND wake.admission_position = candidate.acceptance_position
                       AND wake.first_delivery_sequence = $6
                       AND wake.through_delivery_sequence = $7
                )",
        )
        .bind(starting_snapshot.frontier().snapshot().into_uuid())
        .bind(initial_attempt.into_uuid())
        .bind(turn_id_to_uuid(activated.turn()))
        .bind(session_id_to_uuid(session))
        .bind(immediate_predecessor.into_uuid())
        .bind(Decimal::from(first.get()))
        .bind(Decimal::from(through.get()))
        .execute(&mut *connection)
        .await?
        .rows_affected(),
        _ => {
            return Err(StartEligibleTurnRepositoryError::HubInvariant(
                "prepared delegated origin lineage",
            ));
        }
    };
    if updated != 1 {
        return Err(StartEligibleTurnCorruption::Inconsistent(
            "guarded delegated activation cardinality",
        )
        .into());
    }
    outbox::append(
        connection,
        OutboxEvent::TurnActivated {
            session,
            turn: activated.turn(),
            current_attempt: initial_attempt,
        },
    )
    .await?;
    Ok(activated)
}

fn map_scheduling_error(error: SubmitInputRepositoryError) -> StartEligibleTurnRepositoryError {
    match error {
        SubmitInputRepositoryError::Database(error) => error.into(),
        SubmitInputRepositoryError::CommitAmbiguous(error) => {
            StartEligibleTurnRepositoryError::from_database(error, true)
        }
        SubmitInputRepositoryError::Corruption(error) => {
            StartEligibleTurnCorruption::Scheduling(error).into()
        }
        SubmitInputRepositoryError::DifferentCommandKind { .. } => {
            StartEligibleTurnCorruption::Inconsistent("origin command kind").into()
        }
        SubmitInputRepositoryError::AcceptedInputIdentityCollision { .. } => {
            StartEligibleTurnCorruption::Inconsistent("origin accepted-input identity").into()
        }
        SubmitInputRepositoryError::UnsupportedModelSetting(_) => {
            StartEligibleTurnCorruption::Inconsistent("origin model settings").into()
        }
        SubmitInputRepositoryError::ModelExecution(_) => {
            StartEligibleTurnCorruption::Inconsistent("origin command application").into()
        }
    }
}

fn identity_collision(error: &sqlx::Error) -> Option<StartEligibleTurnIdentityCollision> {
    match error
        .as_database_error()
        .and_then(|database| database.constraint())
    {
        Some("semantic_transcript_entry_pk" | "semantic_transcript_entry_id_global") => {
            Some(StartEligibleTurnIdentityCollision::OriginEntry)
        }
        Some("context_frontier_pk" | "context_frontier_id_global") => {
            Some(StartEligibleTurnIdentityCollision::StartingFrontier)
        }
        Some("turn_attempt_pkey") => Some(StartEligibleTurnIdentityCollision::InitialAttempt),
        _ => None,
    }
}

fn semantic_entry_insert_error(
    error: sqlx::Error,
    candidate: StartEligibleTurnIdentityCollision,
) -> StartEligibleTurnRepositoryError {
    match error
        .as_database_error()
        .and_then(|database| database.constraint())
    {
        Some("semantic_transcript_entry_pk" | "semantic_transcript_entry_id_global") => {
            StartEligibleTurnRepositoryError::IdentityCollision(candidate)
        }
        _ => error.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, error::Error, fmt, io};

    use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
    use sqlx::error::{DatabaseError, ErrorKind};

    use super::{
        StartEligibleTurnIdentityCollision, StartEligibleTurnRepositoryError,
        commit_failure_is_ambiguous, semantic_entry_insert_error,
    };

    #[derive(Debug)]
    struct ServerCommitFailure {
        code: &'static str,
        constraint: Option<&'static str>,
    }

    impl fmt::Display for ServerCommitFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("server reported commit failure")
        }
    }

    impl Error for ServerCommitFailure {}

    impl DatabaseError for ServerCommitFailure {
        fn message(&self) -> &str {
            "server reported commit failure"
        }

        fn as_error(&self) -> &(dyn Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn Error + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }

        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.code))
        }

        fn constraint(&self) -> Option<&str> {
            self.constraint
        }
    }

    #[test]
    fn precommit_database_failure_is_not_commit_ambiguous() {
        let error = StartEligibleTurnRepositoryError::from_database(sqlx::Error::PoolClosed, false);

        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false
            }
        );
    }

    #[test]
    fn impossible_prepared_activation_shape_is_a_hub_bug() {
        let error = StartEligibleTurnRepositoryError::HubInvariant("prepared origin-entry payload");

        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::CallerOrHubBug
        );
    }

    #[test]
    fn lost_commit_response_is_commit_ambiguous() {
        let error = sqlx::Error::Io(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "commit response was lost",
        ));
        let commit_ambiguous = commit_failure_is_ambiguous(&error);
        assert!(commit_ambiguous);
        let error = StartEligibleTurnRepositoryError::from_database(error, commit_ambiguous);

        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true
            }
        );
    }

    #[test]
    fn server_rejected_commit_is_not_ambiguous() {
        let error = sqlx::Error::Database(Box::new(ServerCommitFailure {
            code: "23514",
            constraint: None,
        }));
        let commit_ambiguous = commit_failure_is_ambiguous(&error);

        assert!(!commit_ambiguous);
        let classified = StartEligibleTurnRepositoryError::from_database(error, commit_ambiguous);
        assert_eq!(
            classified.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false
            }
        );
    }

    #[test]
    fn server_reported_unknown_commit_outcomes_are_ambiguous() {
        assert_server_reported_unknown_commit_outcome_is_ambiguous("08007");
        assert_server_reported_unknown_commit_outcome_is_ambiguous("40003");
    }

    #[test]
    fn model_identity_entry_collision_retains_its_candidate_kind() {
        let error = sqlx::Error::Database(Box::new(ServerCommitFailure {
            code: "23505",
            constraint: Some("semantic_transcript_entry_id_global"),
        }));

        assert!(matches!(
            semantic_entry_insert_error(
                error,
                StartEligibleTurnIdentityCollision::ModelIdentityEntry,
            ),
            StartEligibleTurnRepositoryError::IdentityCollision(
                StartEligibleTurnIdentityCollision::ModelIdentityEntry
            )
        ));
    }

    #[track_caller]
    fn assert_server_reported_unknown_commit_outcome_is_ambiguous(code: &'static str) {
        let error = sqlx::Error::Database(Box::new(ServerCommitFailure {
            code,
            constraint: None,
        }));
        let commit_ambiguous = commit_failure_is_ambiguous(&error);

        assert!(commit_ambiguous);
        let classified = StartEligibleTurnRepositoryError::from_database(error, commit_ambiguous);
        assert_eq!(
            classified.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true
            }
        );
    }
}
