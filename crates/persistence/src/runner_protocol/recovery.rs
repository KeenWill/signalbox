//! Command claim, replay, and terminal runner-recovery transactions.

use super::*;
use crate::command_registry::{self, CommandKind, RegistryInspectionError};
use signalbox_domain::{
    AbandonLostRunner, AbandonLostRunnerResult, DurableCommandId, PromotePendingRunner,
    PromotePendingRunnerResult, ReplaceLostRunner, ReplaceLostRunnerResult,
    RunnerRecoveryRejection,
};

/// Whether a recovery command settled, remains pending, or conflicts with its identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunnerRecoveryOutcome<T> {
    /// The durable terminal result, including equal replay.
    Recorded(T),
    /// The exact request remains durably pending.
    Pending,
    /// The identifier belongs to a different payload or command kind.
    ConflictingReuse,
}

#[derive(Debug, signalbox_derive::OperatorError)]
/// Failure to claim, reconstruct, or persist a runner recovery command.
pub enum RunnerRecoveryError {
    #[error(transparent)]
    /// Runner state or PostgreSQL rejected the transaction.
    Store(#[source] RunnerProtocolStoreError),
    #[error("runner recovery command registry is corrupt: {field_0}")]
    /// The retained registry envelope cannot be reconstructed.
    Registry(String),
    #[error("runner recovery command identity is reserved")]
    /// A reserved identity cannot claim a command.
    InvalidCommandId,
}

impl From<RunnerProtocolStoreError> for RunnerRecoveryError {
    fn from(value: RunnerProtocolStoreError) -> Self {
        Self::Store(value)
    }
}

impl From<RunnerProtocolCorruption> for RunnerRecoveryError {
    fn from(value: RunnerProtocolCorruption) -> Self {
        Self::Store(value.into())
    }
}

impl From<sqlx::Error> for RunnerRecoveryError {
    fn from(value: sqlx::Error) -> Self {
        Self::Store(value.into())
    }
}

enum Claim {
    Fresh,
    Replay,
    Conflict,
}

async fn inspect(
    connection: &mut PgConnection,
    command: DurableCommandId,
) -> Result<Option<CommandKind>, RunnerRecoveryError> {
    command_registry::inspect(connection, command)
        .await
        .map_err(|error| match error {
            RegistryInspectionError::Database(error) => RunnerRecoveryError::from(error),
            RegistryInspectionError::Corruption(error) => {
                RunnerRecoveryError::Registry(format!("{error:?}"))
            }
        })
}

async fn claim(
    connection: &mut PgConnection,
    command: DurableCommandId,
    kind: CommandKind,
) -> Result<Claim, RunnerRecoveryError> {
    if command.as_uuid().is_nil() || command.as_uuid().is_max() {
        return Err(RunnerRecoveryError::InvalidCommandId);
    }
    if let Some(recorded) = inspect(connection, command).await? {
        return Ok(if recorded == kind {
            Claim::Replay
        } else {
            Claim::Conflict
        });
    }
    let inserted = sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind, issuer_module)
         VALUES ($1, $2, 1, transaction_timestamp(), 'operator', NULL)
         ON CONFLICT DO NOTHING",
    )
    .bind(command.into_uuid())
    .bind(crate::mapping::durable_command_kind_to_str(kind))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    if inserted == 1 {
        return Ok(Claim::Fresh);
    }
    Ok(if inspect(connection, command).await? == Some(kind) {
        Claim::Replay
    } else {
        Claim::Conflict
    })
}

impl RunnerProtocolStore {
    /// Loads current active authority for an explicitly promoted pending candidate.
    pub async fn promoted_runner_receipt(
        &self,
        candidate: RunnerEnrollmentId,
    ) -> Result<Option<RunnerEnrollmentReceipt>, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        let request: Option<Uuid> = sqlx::query_scalar("SELECT receipt.request_id
            FROM runner_enrollment_request_receipt AS receipt
            JOIN runner_pending_predecessor AS pending ON pending.enrollment_id = receipt.enrollment_id
            WHERE receipt.enrollment_id = $1")
            .bind(candidate.into_uuid()).fetch_optional(&mut *transaction).await?;
        let Some(request) = request else {
            return Ok(None);
        };
        sqlx::query(RUNNER_ENROLLMENT)
            .bind(candidate.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let enrollment = load_enrollment_in(transaction.as_mut(), candidate)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        if enrollment.state() != RunnerEnrollmentState::Active {
            return Ok(None);
        }
        let revision: Decimal = sqlx::query_scalar(RUNNER_REGISTRATION_HEAD)
            .bind(candidate.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let registration = load_registration_in(
            transaction.as_mut(),
            candidate,
            decode_registration_revision(revision)?,
            Some(&enrollment),
            &self.catalog,
        )
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        Ok(Some(RunnerEnrollmentReceipt {
            request: RunnerEnrollmentRequestId::from_uuid(request),
            enrollment,
            registration,
        }))
    }
    /// Resumes the retained replacement under its original identity.
    pub async fn resume_runner_replacement(
        &self,
        command: DurableCommandId,
    ) -> Result<RunnerRecoveryOutcome<ReplaceLostRunnerResult>, RunnerRecoveryError> {
        let mut transaction = self.pool.begin().await?;
        let result = self
            .load_replacement_result(transaction.as_mut(), command)
            .await?;
        if !matches!(result, RunnerRecoveryOutcome::Pending) {
            return Ok(result);
        }
        let request =
            sqlx::query("SELECT session_id FROM replace_lost_runner_command WHERE command_id = $1")
                .bind(command.into_uuid())
                .fetch_one(&mut *transaction)
                .await?;
        let session = SessionId::from_uuid(request.decode_column("session_id")?);
        sqlx::query(RUNNER_RETRY_REPLACEMENT_SCHEDULER)
            .bind(session.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        // A concurrent installer may have settled while this transaction waited for the scheduler.
        let result = self
            .load_replacement_result(transaction.as_mut(), command)
            .await?;
        if !matches!(result, RunnerRecoveryOutcome::Pending) {
            return Ok(result);
        }
        let result = self
            .install_staged_replacement_in(&mut transaction, command, session)
            .await?;
        if let Some(result) = result {
            insert_replacement_result(&mut transaction, command, result).await?;
            sqlx::query("DELETE FROM runner_replacement_stage WHERE command_id = $1")
                .bind(command.into_uuid())
                .execute(&mut *transaction)
                .await?;
        }
        commit_mutation(transaction).await?;
        Ok(match result {
            Some(result) => RunnerRecoveryOutcome::Recorded(result),
            None => RunnerRecoveryOutcome::Pending,
        })
    }

    /// Resumes every claimed, unterminated replacement before process clients are admitted.
    pub async fn resume_runner_replacements(&self) -> Result<(), RunnerRecoveryError> {
        let commands: Vec<Uuid> = sqlx::query_scalar("SELECT request.command_id FROM replace_lost_runner_command AS request LEFT JOIN replace_lost_runner_result AS result USING (command_id) WHERE result.command_id IS NULL ORDER BY request.command_id")
            .fetch_all(&self.pool).await?;
        for command in commands {
            self.resume_runner_replacement(DurableCommandId::from_uuid(command))
                .await?;
        }
        Ok(())
    }

    async fn install_staged_replacement_in(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        command: DurableCommandId,
        session: SessionId,
    ) -> Result<Option<ReplaceLostRunnerResult>, RunnerProtocolStoreError> {
        let rejected = |reason| Some(ReplaceLostRunnerResult::Rejected(reason));
        let stage = sqlx::query("SELECT * FROM runner_replacement_stage WHERE command_id = $1")
            .bind(command.into_uuid())
            .fetch_one(&mut **transaction)
            .await?;
        let row = sqlx::query("SELECT record.* FROM runner_current_session_placement AS head JOIN runner_session_placement_record AS record USING (session_id, event_ordinal) WHERE head.session_id = $1")
            .bind(session.into_uuid()).fetch_one(&mut **transaction).await?;
        if row.decode_column::<Decimal>("event_ordinal")?
            != stage.decode_column::<Decimal>("source_event_ordinal")?
        {
            return Ok(rejected(RunnerRecoveryRejection::PlacementNotLost));
        }
        let stored = self.decode_stored_placement_in(transaction, &row).await?;
        let SessionRunnerPlacementState::RunnerLost(lost) = stored.placement().state() else {
            return Ok(rejected(RunnerRecoveryRejection::PlacementNotLost));
        };
        let predecessor: Uuid =
            sqlx::query_scalar("SELECT enrollment_id FROM runner_enrollment WHERE runner_id = $1")
                .bind(lost.pinned().runner.into_uuid())
                .fetch_one(&mut **transaction)
                .await?;
        let candidate = runner_enrollment_id(stage.decode_column("successor_enrollment_id")?);
        lock_replacement_enrollments(transaction, predecessor, candidate.into_uuid()).await?;
        let connection = load_connection_head_in(transaction.as_mut(), candidate).await?;
        let mut enrollment = load_enrollment_in(transaction.as_mut(), candidate)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        if connection.is_some_and(|head| head.state() == RunnerConnectionState::Suspect) {
            return Ok(None);
        }
        if enrollment.state() == RunnerEnrollmentState::Revoked
            || !connection.is_some_and(|head| head.state() == RunnerConnectionState::Connected)
        {
            return Ok(rejected(RunnerRecoveryRejection::RunnerUnavailable));
        }
        let revision: Decimal = sqlx::query_scalar(RUNNER_REGISTRATION_HEAD)
            .bind(candidate.into_uuid())
            .fetch_one(&mut **transaction)
            .await?;
        if revision != stage.decode_column::<Decimal>("successor_registration_revision")? {
            return Ok(rejected(RunnerRecoveryRejection::PlacementUnavailable));
        }
        let registration = load_registration_in(
            transaction.as_mut(),
            candidate,
            decode_registration_revision(revision)?,
            Some(&enrollment),
            &self.catalog,
        )
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        let pending = enrollment.state() == RunnerEnrollmentState::Pending;
        if pending {
            enrollment
                .promote_pending_in_place()
                .map_err(RunnerProtocolStoreError::Domain)?;
        }
        let active: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM turn_lifecycle WHERE session_id = $1 AND state_kind = 'active' AND NOT delegation_runtime_terminal)")
            .bind(session.into_uuid()).fetch_one(&mut **transaction).await?;
        if active {
            return Ok(None);
        }
        let authorization = sqlx::query(
            "SELECT * FROM runner_replacement_provisioning_authorization WHERE command_id = $1",
        )
        .bind(command.into_uuid())
        .fetch_optional(&mut **transaction)
        .await?
        .as_ref()
        .map(super::provisioning::decode_authorization)
        .transpose()?;
        let workspace = if let Some(authorization) = &authorization {
            let Some(workspace) =
                super::provisioning::load_ready(transaction.as_mut(), authorization).await?
            else {
                return Ok(None);
            };
            Some(workspace)
        } else {
            None
        };
        let mut request = stored.placement().request().clone();
        request.selector = RunnerSelector::Identity(enrollment.runner());
        let directory = match (&request.working_directory, &workspace) {
            (WorkingDirectorySelection::Exact(directory), _) => directory.clone(),
            (WorkingDirectorySelection::RunnerDefault, Some(workspace)) => {
                workspace.working_directory.clone()
            }
            (WorkingDirectorySelection::RunnerDefault, None) => {
                let Some(directory) = registration.registration().default_working_directory()
                else {
                    return Ok(rejected(RunnerRecoveryRejection::PlacementUnavailable));
                };
                directory.clone()
            }
        };
        let ordinal = stored
            .event_ordinal()
            .checked_add(1)
            .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
        let (_, placement, _, grant, _) = stored.into_parts();
        let replacement = match placement.replace_lost_runner(
            request,
            registration.registration(),
            directory,
            workspace,
            grant,
        ) {
            Ok(replacement) => replacement,
            Err(_) => return Ok(rejected(RunnerRecoveryRejection::PlacementUnavailable)),
        };
        if pending {
            let request: Uuid = sqlx::query_scalar(
                "SELECT request_id FROM runner_enrollment_request_receipt WHERE enrollment_id = $1",
            )
            .bind(candidate.into_uuid())
            .fetch_one(&mut **transaction)
            .await?;
            match self
                .promote_in(transaction, RunnerEnrollmentRequestId::from_uuid(request))
                .await?
            {
                PromotePendingRunnerResult::Promoted { .. } => {}
                PromotePendingRunnerResult::Rejected(reason) => return Ok(rejected(reason)),
            }
        }
        sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(session.into_uuid())
            .fetch_one(&mut **transaction)
            .await?;
        let grant_origin = placement_grant_origin(Some(&row), ordinal, &replacement.placement)?;
        insert_placement_record(
            transaction,
            ordinal,
            "runner_replaced",
            &replacement.placement,
            stored_registration_identity(Some(&registration)),
            grant_origin,
            None,
        )
        .await?;
        if let Some(grant) = &replacement.grant {
            insert_grant_if_new(
                transaction,
                Some(&row),
                ordinal,
                &replacement.placement,
                grant,
                RegistrationAuthority {
                    stored: &registration,
                    catalog: &self.catalog,
                },
                grant_origin.ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?,
            )
            .await?;
        }
        sqlx::query(
            "UPDATE runner_current_session_placement SET event_ordinal = $2 WHERE session_id = $1",
        )
        .bind(session.into_uuid())
        .bind(Decimal::from(ordinal))
        .execute(&mut **transaction)
        .await?;
        append_placement_boundary(transaction, command, ordinal, &replacement).await?;
        let directory = match replacement.placement.state() {
            SessionRunnerPlacementState::Pinned(pinned) => &pinned.working_directory,
            _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
        };
        let state = if row.decode_column::<Option<Uuid>>("lost_runner_id")?
            == Some(enrollment.runner().into_uuid())
            && row
                .decode_column::<Option<String>>("pinned_working_directory")?
                .as_deref()
                != Some(directory.as_str())
        {
            DispatchedRunnerState::WorkingDirectoryChanged
        } else {
            DispatchedRunnerState::Replaced
        };
        append_recovery_placement_event(
            transaction,
            &replacement.placement,
            enrollment.runner(),
            ordinal,
            state,
        )
        .await?;
        if let Some(authorization) = authorization {
            sqlx::query("INSERT INTO runner_replacement_workspace_consumption (authorization_id, command_id) VALUES ($1, $2)")
                .bind(authorization.authorization.into_uuid()).bind(command.into_uuid()).execute(&mut **transaction).await?;
        }
        Ok(Some(ReplaceLostRunnerResult::Replaced {
            runner: enrollment.runner(),
            placement_revision: replacement.placement.revision(),
        }))
    }

    /// Claims a replacement immediately and either installs it or retains its exact staging facts.
    pub async fn replace_lost_runner(
        &self,
        command: ReplaceLostRunner,
    ) -> Result<RunnerRecoveryOutcome<ReplaceLostRunnerResult>, RunnerRecoveryError> {
        let mut transaction = self.pool.begin().await?;
        match claim(
            transaction.as_mut(),
            command.command_id,
            CommandKind::ReplaceLostRunner,
        )
        .await?
        {
            Claim::Conflict => return Ok(RunnerRecoveryOutcome::ConflictingReuse),
            Claim::Replay => {
                let row = sqlx::query("SELECT session_id, revision FROM replace_lost_runner_command WHERE command_id = $1")
                    .bind(command.command_id.into_uuid()).fetch_one(&mut *transaction).await?;
                if row.decode_column::<Uuid>("session_id")? != command.session.into_uuid()
                    || row.decode_column::<Option<String>>("revision")?.as_deref()
                        != command.revision.as_ref().map(WorkspaceRevision::as_str)
                {
                    return Ok(RunnerRecoveryOutcome::ConflictingReuse);
                }
                return self
                    .load_replacement_result(transaction.as_mut(), command.command_id)
                    .await;
            }
            Claim::Fresh => {}
        }
        sqlx::query("INSERT INTO replace_lost_runner_command (command_id, command_kind, storage_version, session_id, revision) VALUES ($1, 'replace_lost_runner', 1, $2, $3)")
            .bind(command.command_id.into_uuid()).bind(command.session.into_uuid())
            .bind(command.revision.as_ref().map(WorkspaceRevision::as_str))
            .execute(&mut *transaction).await?;
        let mut result = self
            .stage_replacement_in(&mut transaction, &command)
            .await?;
        if result.is_none() {
            result = self
                .install_staged_replacement_in(
                    &mut transaction,
                    command.command_id,
                    command.session,
                )
                .await?;
        }
        if let Some(result) = result {
            insert_replacement_result(&mut transaction, command.command_id, result).await?;
            sqlx::query("DELETE FROM runner_replacement_stage WHERE command_id = $1")
                .bind(command.command_id.into_uuid())
                .execute(&mut *transaction)
                .await?;
        }
        commit_mutation(transaction).await?;
        Ok(match result {
            Some(result) => RunnerRecoveryOutcome::Recorded(result),
            None => RunnerRecoveryOutcome::Pending,
        })
    }

    async fn load_replacement_result(
        &self,
        connection: &mut PgConnection,
        command: DurableCommandId,
    ) -> Result<RunnerRecoveryOutcome<ReplaceLostRunnerResult>, RunnerRecoveryError> {
        let row = sqlx::query("SELECT result_kind, rejection_kind, runner_id, placement_revision FROM replace_lost_runner_result WHERE command_id = $1")
            .bind(command.into_uuid()).fetch_optional(connection).await?;
        let Some(row) = row else {
            return Ok(RunnerRecoveryOutcome::Pending);
        };
        let result = match row.decode_column::<&str>("result_kind")? {
            "applied" => ReplaceLostRunnerResult::Replaced {
                runner: runner_id(row.decode_column("runner_id")?),
                placement_revision: decode_generation(row.decode_column("placement_revision")?)?,
            },
            "rejected" => ReplaceLostRunnerResult::Rejected(decode_rejection(
                row.decode_column("rejection_kind")?,
            )?),
            _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
        };
        Ok(RunnerRecoveryOutcome::Recorded(result))
    }

    async fn stage_replacement_in(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        command: &ReplaceLostRunner,
    ) -> Result<Option<ReplaceLostRunnerResult>, RunnerProtocolStoreError> {
        use RunnerRecoveryRejection as Rejection;
        let rejected = |reason| Some(ReplaceLostRunnerResult::Rejected(reason));
        let exists = sqlx::query_scalar::<_, Uuid>(RUNNER_RETRY_REPLACEMENT_SCHEDULER)
            .bind(command.session.into_uuid())
            .fetch_optional(&mut **transaction)
            .await?
            .is_some();
        if !exists {
            return Ok(rejected(Rejection::SessionNotFound));
        }
        let row = sqlx::query("SELECT record.* FROM runner_current_session_placement AS head JOIN runner_session_placement_record AS record USING (session_id, event_ordinal) WHERE head.session_id = $1")
            .bind(command.session.into_uuid()).fetch_optional(&mut **transaction).await?;
        let Some(row) = row else {
            return Ok(rejected(Rejection::PlacementNotLost));
        };
        let stored = self.decode_stored_placement_in(transaction, &row).await?;
        let (lost_runner, before_pin, registration_loss) = match stored.placement().state() {
            SessionRunnerPlacementState::RunnerLostBeforePin(lost) => (lost.runner(), true, false),
            SessionRunnerPlacementState::RunnerLost(lost) => (
                lost.pinned().runner,
                false,
                lost.source() == RunnerPlacementLossSource::Registration,
            ),
            _ => return Ok(rejected(Rejection::PlacementNotLost)),
        };
        let active: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM turn_lifecycle WHERE session_id = $1 AND state_kind = 'active' AND NOT delegation_runtime_terminal)")
            .bind(command.session.into_uuid()).fetch_one(&mut **transaction).await?;
        if active {
            return Ok(rejected(Rejection::ExistingControlRequired));
        }
        let staging: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM runner_replacement_stage WHERE session_id = $1)",
        )
        .bind(command.session.into_uuid())
        .fetch_one(&mut **transaction)
        .await?;
        if staging {
            return Ok(rejected(Rejection::ReplacementPending));
        }
        if command.revision.is_some()
            && stored.placement().request().workspace == WorkspaceRequirement::None
        {
            return Ok(rejected(Rejection::RevisionWithoutRepository));
        }
        let candidate = sqlx::query(
            "WITH RECURSIVE successors(enrollment_id) AS (
                 SELECT pending.enrollment_id
                 FROM runner_pending_predecessor AS pending
                 JOIN runner_enrollment AS predecessor ON predecessor.enrollment_id = pending.predecessor_enrollment_id
                 WHERE NOT $2 AND predecessor.runner_id = $1
                 UNION
                 SELECT pending.enrollment_id FROM runner_pending_predecessor AS pending
                 JOIN successors ON successors.enrollment_id = pending.predecessor_enrollment_id
             )
             SELECT successor.enrollment_id, successor.runner_id, receipt.request_id
             FROM successors JOIN runner_enrollment AS successor USING (enrollment_id)
             JOIN runner_enrollment_request_receipt AS receipt USING (enrollment_id)
             WHERE successor.state_kind IN ('pending', 'active') AND NOT EXISTS (
                 SELECT 1 FROM runner_pending_predecessor AS pending
                 JOIN runner_enrollment AS descendant ON descendant.enrollment_id = pending.enrollment_id
                 WHERE pending.predecessor_enrollment_id = successor.enrollment_id
                   AND descendant.state_kind IN ('pending', 'active'))
             UNION ALL
             SELECT enrollment.enrollment_id, enrollment.runner_id, receipt.request_id
             FROM runner_enrollment AS enrollment JOIN runner_enrollment_request_receipt AS receipt USING (enrollment_id)
             WHERE $2 AND enrollment.runner_id = $1 AND enrollment.state_kind = 'active'",
        ).bind(lost_runner.into_uuid()).bind(registration_loss).fetch_optional(&mut **transaction).await?;
        let Some(candidate) = candidate else {
            return Ok(rejected(Rejection::PendingRunnerNotFound));
        };
        let candidate_id = runner_enrollment_id(candidate.decode_column("enrollment_id")?);
        let request_id =
            RunnerEnrollmentRequestId::from_uuid(candidate.decode_column("request_id")?);
        // Enrollment locks precede connection and placement locks.
        let predecessor_id: Uuid =
            sqlx::query_scalar("SELECT enrollment_id FROM runner_enrollment WHERE runner_id = $1")
                .bind(lost_runner.into_uuid())
                .fetch_one(&mut **transaction)
                .await?;
        lock_replacement_enrollments(transaction, predecessor_id, candidate_id.into_uuid()).await?;
        let connection = load_connection_head_in(transaction.as_mut(), candidate_id).await?;
        if !connection
            .is_some_and(|connection| connection.state() == RunnerConnectionState::Connected)
        {
            return Ok(rejected(Rejection::RunnerUnavailable));
        }
        let mut enrollment = load_enrollment_in(transaction.as_mut(), candidate_id)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        let pending = enrollment.state() == RunnerEnrollmentState::Pending;
        let revision: Decimal = sqlx::query_scalar(RUNNER_REGISTRATION_HEAD)
            .bind(candidate_id.into_uuid())
            .fetch_one(&mut **transaction)
            .await?;
        let registration = load_registration_in(
            transaction.as_mut(),
            candidate_id,
            decode_registration_revision(revision)?,
            Some(&enrollment),
            &self.catalog,
        )
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        if pending {
            enrollment
                .promote_pending_in_place()
                .map_err(RunnerProtocolStoreError::Domain)?;
        }
        let mut request = stored.placement().request().clone();
        request.selector = RunnerSelector::Identity(enrollment.runner());
        if before_pin {
            let replacement = match stored
                .placement
                .replace_lost_runner_before_pin(request, registration.registration())
            {
                Ok(replacement) => replacement,
                Err(_) => {
                    return Ok(rejected(Rejection::PlacementUnavailable));
                }
            };
            if pending {
                match self.promote_in(transaction, request_id).await? {
                    PromotePendingRunnerResult::Promoted { .. } => {}
                    PromotePendingRunnerResult::Rejected(reason) => return Ok(rejected(reason)),
                }
            }
            lock_runner_placement_loss_baseline(transaction, &replacement.placement).await?;
            sqlx::query(RUNNER_PLACEMENT_HEAD)
                .bind(command.session.into_uuid())
                .fetch_one(&mut **transaction)
                .await?;
            let ordinal = stored
                .event_ordinal
                .checked_add(1)
                .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
            insert_placement_record(
                transaction,
                ordinal,
                "pre_pin_replaced",
                &replacement.placement,
                (None, None),
                None,
                None,
            )
            .await?;
            sqlx::query("UPDATE runner_current_session_placement SET event_ordinal = $2 WHERE session_id = $1")
                .bind(command.session.into_uuid()).bind(Decimal::from(ordinal)).execute(&mut **transaction).await?;
            append_recovery_placement_event(
                transaction,
                &replacement.placement,
                enrollment.runner(),
                ordinal,
                DispatchedRunnerState::Replaced,
            )
            .await?;
            return Ok(Some(ReplaceLostRunnerResult::Replaced {
                runner: enrollment.runner(),
                placement_revision: replacement.placement.revision(),
            }));
        }
        if !registration
            .registration()
            .supports_sandbox(request.sandbox)
            || matches!(&request.workspace, WorkspaceRequirement::RepositoryWorktree { repository }
                if !registration.registration().supports_workspace(WorkspaceCapability::WorktreePerSession)
                    || registration.registration().repository(repository).is_none())
        {
            return Ok(rejected(Rejection::PlacementUnavailable));
        }
        let repository = match &request.workspace {
            WorkspaceRequirement::RepositoryWorktree { repository } => Some(repository.as_str()),
            WorkspaceRequirement::None => None,
        };
        let recovery = command
            .revision
            .clone()
            .map(|revision| WorkspaceRecovery::Commit { revision })
            .or_else(|| match stored.placement().state() {
                SessionRunnerPlacementState::RunnerLost(lost) => lost
                    .pinned()
                    .workspace
                    .as_ref()
                    .and_then(|workspace| workspace.recovery.clone()),
                _ => None,
            });
        let (_, branch, revision) = recovery
            .as_ref()
            .map(encode_workspace_recovery)
            .unwrap_or((None, None, None));
        if repository.is_some() != revision.is_some() {
            return Ok(rejected(Rejection::PlacementUnavailable));
        }
        sqlx::query("INSERT INTO runner_replacement_stage (command_id, session_id, source_event_ordinal, successor_enrollment_id, successor_registration_revision) VALUES ($1, $2, $3, $4, $5)")
            .bind(command.command_id.into_uuid()).bind(command.session.into_uuid())
            .bind(Decimal::from(stored.event_ordinal())).bind(candidate_id.into_uuid())
            .bind(Decimal::from(registration.revision().get())).execute(&mut **transaction).await?;
        let private_root = request.sandbox == RunnerSandboxProfile::WorkspaceRestricted
            && request.working_directory == WorkingDirectorySelection::RunnerDefault;
        if repository.is_some() || private_root {
            let successor_revision = stored
                .placement()
                .revision()
                .checked_next()
                .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
            sqlx::query("INSERT INTO runner_replacement_provisioning_authorization (authorization_id, command_id, session_id, placement_revision, runner_id, registration_enrollment_id, registration_revision, repository_key, sandbox_profile, credential_profile_name, checkout_revision, checkout_branch) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)")
                .bind(Uuid::now_v7()).bind(command.command_id.into_uuid()).bind(command.session.into_uuid())
                .bind(Decimal::from(successor_revision.get())).bind(enrollment.runner().into_uuid())
                .bind(candidate_id.into_uuid()).bind(Decimal::from(registration.revision().get()))
                .bind(repository).bind(runner_sandbox_to_str(request.sandbox))
                .bind(repository.and(request.credential_profile.as_ref().map(CredentialProfileName::as_str)))
                .bind(revision).bind(branch)
                .execute(&mut **transaction).await?;
        }
        Ok(None)
    }

    /// Claims and settles abandonment in one transaction, without inventing turn control.
    pub async fn abandon_lost_runner(
        &self,
        command: AbandonLostRunner,
    ) -> Result<RunnerRecoveryOutcome<AbandonLostRunnerResult>, RunnerRecoveryError> {
        let mut transaction = self.pool.begin().await?;
        match claim(
            transaction.as_mut(),
            command.command_id,
            CommandKind::AbandonLostRunner,
        )
        .await?
        {
            Claim::Conflict => return Ok(RunnerRecoveryOutcome::ConflictingReuse),
            Claim::Replay => {
                let row = sqlx::query(
                    "SELECT request.session_id, result.result_kind, result.rejection_kind
                     FROM abandon_lost_runner_command AS request
                     JOIN abandon_lost_runner_result AS result USING (command_id)
                     WHERE request.command_id = $1",
                )
                .bind(command.command_id.into_uuid())
                .fetch_one(&mut *transaction)
                .await?;
                if row.decode_column::<Uuid>("session_id")? != command.session.into_uuid() {
                    return Ok(RunnerRecoveryOutcome::ConflictingReuse);
                }
                let result = match row.decode_column::<&str>("result_kind")? {
                    "applied" => AbandonLostRunnerResult::Abandoned,
                    "rejected" => AbandonLostRunnerResult::Rejected(decode_rejection(
                        row.decode_column("rejection_kind")?,
                    )?),
                    _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
                };
                return Ok(RunnerRecoveryOutcome::Recorded(result));
            }
            Claim::Fresh => {}
        }
        sqlx::query(
            "INSERT INTO abandon_lost_runner_command (command_id, command_kind, storage_version, session_id)
             VALUES ($1, 'abandon_lost_runner', 1, $2)",
        ).bind(command.command_id.into_uuid()).bind(command.session.into_uuid())
            .execute(&mut *transaction).await?;
        let result = self.abandon_in(&mut transaction, command.session).await?;
        let rejection = match result {
            AbandonLostRunnerResult::Abandoned => None,
            AbandonLostRunnerResult::Rejected(reason) => Some(encode_rejection(reason)),
        };
        sqlx::query("INSERT INTO abandon_lost_runner_result (command_id, result_kind, rejection_kind) VALUES ($1, $2, $3)")
            .bind(command.command_id.into_uuid())
            .bind(if rejection.is_some() { "rejected" } else { "applied" })
            .bind(rejection).execute(&mut *transaction).await?;
        if result == AbandonLostRunnerResult::Abandoned {
            let staged: Option<Uuid> = sqlx::query_scalar(
                "SELECT command_id FROM runner_replacement_stage WHERE session_id = $1",
            )
            .bind(command.session.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
            if let Some(staged) = staged {
                insert_replacement_result(
                    &mut transaction,
                    DurableCommandId::from_uuid(staged),
                    ReplaceLostRunnerResult::Rejected(RunnerRecoveryRejection::PlacementNotLost),
                )
                .await?;
                sqlx::query("DELETE FROM runner_replacement_stage WHERE command_id = $1")
                    .bind(staged)
                    .execute(&mut *transaction)
                    .await?;
            }
        }
        commit_mutation(transaction).await?;
        Ok(RunnerRecoveryOutcome::Recorded(result))
    }

    async fn abandon_in(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        session: SessionId,
    ) -> Result<AbandonLostRunnerResult, RunnerProtocolStoreError> {
        let exists = sqlx::query_scalar::<_, Uuid>(RUNNER_RETRY_REPLACEMENT_SCHEDULER)
            .bind(session.into_uuid())
            .fetch_optional(&mut **transaction)
            .await?
            .is_some();
        if !exists {
            return Ok(AbandonLostRunnerResult::Rejected(
                RunnerRecoveryRejection::SessionNotFound,
            ));
        }
        let active: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM turn_lifecycle WHERE session_id = $1 AND state_kind = 'active' AND NOT delegation_runtime_terminal)")
            .bind(session.into_uuid()).fetch_one(&mut **transaction).await?;
        if active {
            return Ok(AbandonLostRunnerResult::Rejected(
                RunnerRecoveryRejection::ExistingControlRequired,
            ));
        }
        let row = sqlx::query(
            "SELECT record.* FROM runner_current_session_placement AS head
             JOIN runner_session_placement_record AS record USING (session_id, event_ordinal)
             WHERE head.session_id = $1",
        )
        .bind(session.into_uuid())
        .fetch_optional(&mut **transaction)
        .await?;
        let Some(row) = row else {
            return Ok(AbandonLostRunnerResult::Rejected(
                RunnerRecoveryRejection::PlacementNotLost,
            ));
        };
        let stored = self.decode_stored_placement_in(transaction, &row).await?;
        if !matches!(
            stored.placement().state(),
            SessionRunnerPlacementState::RunnerLost(_)
                | SessionRunnerPlacementState::RunnerLostBeforePin(_)
        ) {
            return Ok(AbandonLostRunnerResult::Rejected(
                RunnerRecoveryRejection::PlacementNotLost,
            ));
        }
        lock_runner_placement_loss_baseline(transaction, stored.placement()).await?;
        sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(session.into_uuid())
            .fetch_one(&mut **transaction)
            .await?;
        let ordinal = stored
            .event_ordinal()
            .checked_add(1)
            .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
        let (_, placement, registration, grant, _) = stored.into_parts();
        let placement = placement
            .abandon_lost_runner()
            .map_err(RunnerProtocolStoreError::Domain)?;
        let history = prospective_placement_reconstitution_history(
            transaction.as_mut(),
            Some(&row),
            "abandoned",
            &placement,
        )
        .await?;
        validate_placement_snapshot(&placement, registration.as_ref(), grant.as_ref(), history)?;
        let grant_origin = placement_grant_origin(Some(&row), ordinal, &placement)?;
        insert_placement_record(
            transaction,
            ordinal,
            "abandoned",
            &placement,
            stored_registration_identity(registration.as_ref()),
            grant_origin,
            None,
        )
        .await?;
        sqlx::query(
            "UPDATE runner_current_session_placement SET event_ordinal = $2 WHERE session_id = $1",
        )
        .bind(session.into_uuid())
        .bind(Decimal::from(ordinal))
        .execute(&mut **transaction)
        .await?;
        append_recovery_placement_event(
            transaction,
            &placement,
            placement_loss_fence_runner(&placement)
                .ok_or(RunnerProtocolCorruption::CrossWiredReference)?,
            ordinal,
            DispatchedRunnerState::Abandoned,
        )
        .await?;
        Ok(AbandonLostRunnerResult::Abandoned)
    }

    /// Claims and settles promotion without changing any session placement.
    pub async fn promote_pending_runner(
        &self,
        command: PromotePendingRunner,
    ) -> Result<RunnerRecoveryOutcome<PromotePendingRunnerResult>, RunnerRecoveryError> {
        let mut transaction = self.pool.begin().await?;
        match claim(
            transaction.as_mut(),
            command.command_id,
            CommandKind::PromotePendingRunner,
        )
        .await?
        {
            Claim::Conflict => return Ok(RunnerRecoveryOutcome::ConflictingReuse),
            Claim::Replay => {
                let row = sqlx::query(
                    "SELECT request.enrollment_request_id, result.result_kind, result.rejection_kind, result.runner_id
                     FROM promote_pending_runner_command AS request
                     JOIN promote_pending_runner_result AS result USING (command_id)
                     WHERE request.command_id = $1",
                ).bind(command.command_id.into_uuid()).fetch_one(&mut *transaction).await?;
                if row.decode_column::<Uuid>("enrollment_request_id")?
                    != command.enrollment_request.into_uuid()
                {
                    return Ok(RunnerRecoveryOutcome::ConflictingReuse);
                }
                let result = match row.decode_column::<&str>("result_kind")? {
                    "applied" => PromotePendingRunnerResult::Promoted {
                        runner: runner_id(row.decode_column("runner_id")?),
                    },
                    "rejected" => PromotePendingRunnerResult::Rejected(decode_rejection(
                        row.decode_column("rejection_kind")?,
                    )?),
                    _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
                };
                return Ok(RunnerRecoveryOutcome::Recorded(result));
            }
            Claim::Fresh => {}
        }
        sqlx::query("INSERT INTO promote_pending_runner_command (command_id, command_kind, storage_version, enrollment_request_id) VALUES ($1, 'promote_pending_runner', 1, $2)")
            .bind(command.command_id.into_uuid()).bind(command.enrollment_request.into_uuid())
            .execute(&mut *transaction).await?;
        let result = self
            .promote_in(&mut transaction, command.enrollment_request)
            .await?;
        let (runner, rejection) = match result {
            PromotePendingRunnerResult::Promoted { runner } => (Some(runner.into_uuid()), None),
            PromotePendingRunnerResult::Rejected(reason) => (None, Some(encode_rejection(reason))),
        };
        sqlx::query("INSERT INTO promote_pending_runner_result (command_id, result_kind, rejection_kind, runner_id) VALUES ($1, $2, $3, $4)")
            .bind(command.command_id.into_uuid()).bind(if rejection.is_some() { "rejected" } else { "applied" })
            .bind(rejection).bind(runner).execute(&mut *transaction).await?;
        commit_mutation(transaction).await?;
        Ok(RunnerRecoveryOutcome::Recorded(result))
    }

    async fn promote_in(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        request: RunnerEnrollmentRequestId,
    ) -> Result<PromotePendingRunnerResult, RunnerProtocolStoreError> {
        let pending = sqlx::query(
            "SELECT pending.enrollment_id, pending.predecessor_enrollment_id
             FROM runner_pending_predecessor AS pending
             JOIN runner_enrollment_request_receipt AS receipt USING (enrollment_id)
             WHERE receipt.request_id = $1",
        )
        .bind(request.into_uuid())
        .fetch_optional(&mut **transaction)
        .await?;
        let Some(pending) = pending else {
            return Ok(PromotePendingRunnerResult::Rejected(
                RunnerRecoveryRejection::PendingRunnerNotFound,
            ));
        };
        let candidate: Uuid = pending.decode_column("enrollment_id")?;
        let predecessor: Uuid = pending.decode_column("predecessor_enrollment_id")?;
        lock_recovery_identities(transaction, &[candidate, predecessor]).await?;
        sqlx::query(crate::lock_inventory::RUNNER_RECOVERY_ENROLLMENTS)
            .bind(vec![candidate, predecessor])
            .fetch_all(&mut **transaction)
            .await?;
        let mut candidate =
            load_enrollment_in(transaction.as_mut(), runner_enrollment_id(candidate))
                .await?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        let predecessor =
            load_enrollment_in(transaction.as_mut(), runner_enrollment_id(predecessor))
                .await?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        if candidate.state() != RunnerEnrollmentState::Pending {
            return Ok(PromotePendingRunnerResult::Rejected(
                RunnerRecoveryRejection::PendingRunnerNotFound,
            ));
        }
        for enrollment in [candidate.enrollment(), predecessor.enrollment()] {
            sqlx::query_scalar::<_, Decimal>(RUNNER_PLACEMENT_CONNECTION_AUTHORITY)
                .bind(enrollment.into_uuid())
                .fetch_optional(&mut **transaction)
                .await?;
        }
        let old = load_connection_head_in(transaction.as_mut(), predecessor.enrollment()).await?;
        let new = load_connection_head_in(transaction.as_mut(), candidate.enrollment()).await?;
        if predecessor.state() != RunnerEnrollmentState::Active
            || !old.is_some_and(|connection| connection.state() == RunnerConnectionState::Lost)
            || !new.is_some_and(|connection| connection.state() == RunnerConnectionState::Connected)
        {
            return Ok(PromotePendingRunnerResult::Rejected(
                RunnerRecoveryRejection::RunnerUnavailable,
            ));
        }
        candidate
            .promote_pending_in_place()
            .map_err(RunnerProtocolStoreError::Domain)?;
        // The stored registration stays the same immutable record; only its enrollment fence changes.
        advance_enrollment_state(transaction, predecessor.enrollment(), "revoked").await?;
        advance_enrollment_state(transaction, candidate.enrollment(), "active").await?;
        Ok(PromotePendingRunnerResult::Promoted {
            runner: candidate.runner(),
        })
    }
}

async fn append_recovery_placement_event(
    transaction: &mut Transaction<'_, Postgres>,
    placement: &SessionRunnerPlacement,
    runner: RunnerId,
    ordinal: u64,
    state: DispatchedRunnerState,
) -> Result<(), RunnerProtocolStoreError> {
    outbox::append(
        transaction,
        OutboxEvent::RunnerStateTransition(RunnerStateOutboxEvent {
            session: placement.session(),
            runner,
            placement_revision: placement.revision(),
            sandbox: placement.request().sandbox,
            working_directory: lost_runner_working_directory(placement),
            state,
            source: RunnerStateOutboxSource {
                placement_event_ordinal: ordinal,
                connection: None,
            },
        }),
    )
    .await?;
    Ok(())
}

async fn lock_replacement_enrollments(
    transaction: &mut Transaction<'_, Postgres>,
    lost: Uuid,
    candidate: Uuid,
) -> Result<(), RunnerProtocolStoreError> {
    let predecessor: Option<Uuid> = sqlx::query_scalar(
        "SELECT predecessor_enrollment_id FROM runner_pending_predecessor WHERE enrollment_id = $1",
    )
    .bind(candidate)
    .fetch_optional(&mut **transaction)
    .await?;
    let mut enrollments = vec![lost, candidate];
    enrollments.extend(predecessor);
    enrollments.sort_unstable();
    enrollments.dedup();
    lock_recovery_identities(transaction, &enrollments).await?;
    sqlx::query(crate::lock_inventory::RUNNER_RECOVERY_ENROLLMENTS)
        .bind(&enrollments)
        .fetch_all(&mut **transaction)
        .await?;
    for enrollment in enrollments {
        sqlx::query(RUNNER_PLACEMENT_CONNECTION_AUTHORITY)
            .bind(enrollment)
            .fetch_optional(&mut **transaction)
            .await?;
    }
    Ok(())
}

async fn lock_recovery_identities(
    transaction: &mut Transaction<'_, Postgres>,
    enrollments: &[Uuid],
) -> Result<(), RunnerProtocolStoreError> {
    let runners: Vec<Uuid> = sqlx::query_scalar(
        "SELECT runner_id FROM runner_enrollment WHERE enrollment_id = ANY($1) ORDER BY runner_id",
    )
    .bind(enrollments)
    .fetch_all(&mut **transaction)
    .await?;
    for runner in runners {
        sqlx::query(crate::lock_inventory::RUNNER_RECOVERY_LOSS_IDENTITY)
            .bind(runner)
            .execute(&mut **transaction)
            .await?;
    }
    Ok(())
}

async fn append_placement_boundary(
    transaction: &mut Transaction<'_, Postgres>,
    command: DurableCommandId,
    ordinal: u64,
    replacement: &signalbox_domain::RunnerPlacementReplacement,
) -> Result<(), RunnerProtocolStoreError> {
    use signalbox_domain::{
        ContextFrontierId, ResolvedContextFrontierSnapshot, RunnerPlacementBoundary,
        SemanticTranscriptEntryId,
    };
    let session = replacement.placement.session();
    let frontiers: Vec<Uuid> = sqlx::query_scalar(
        "(SELECT turn_lifecycle_effective_terminal_frontier(session_id, turn_id)
            FROM turn_lifecycle WHERE session_id = $1
              AND turn_lifecycle_effective_terminal_frontier(session_id, turn_id) IS NOT NULL ORDER BY acceptance_position DESC LIMIT 1)
         UNION SELECT boundary.context_frontier_id FROM runner_session_placement_frontier AS head
            JOIN runner_placement_boundary AS boundary USING (session_id, placement_revision) WHERE head.session_id = $1
         UNION SELECT seed_context_frontier_id FROM imported_session_seed WHERE session_id = $1
         UNION SELECT compaction.result_frontier_id FROM context_compaction AS compaction
            WHERE compaction.session_id = $1 AND NOT EXISTS (SELECT 1 FROM context_compaction AS successor WHERE successor.predecessor_compaction_id = compaction.context_compaction_id)",
    ).bind(session.into_uuid()).fetch_all(&mut **transaction).await?;
    let mut prior: Option<ResolvedContextFrontierSnapshot> = None;
    for frontier in frontiers {
        let snapshot = crate::model_execution::load_call_snapshot(
            transaction.as_mut(),
            session,
            ContextFrontierId::from_uuid(frontier),
        )
        .await
        .map_err(map_frontier_error)?
        .reconstitute()
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        prior = match prior {
            None => Some(snapshot),
            Some(before) if before.is_semantic_prefix_of(&snapshot) => Some(snapshot),
            Some(before) if snapshot.is_semantic_prefix_of(&before) => Some(before),
            Some(_) => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
        };
    }
    let boundary = RunnerPlacementBoundary::prepare(
        replacement,
        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
        ContextFrontierId::from_uuid(Uuid::now_v7()),
        prior.as_ref(),
    )
    .map_err(RunnerProtocolStoreError::Domain)?;
    sqlx::query("INSERT INTO semantic_transcript_entry (source_session_id, semantic_entry_id, payload_kind, runner_placement_revision) VALUES ($1, $2, 'runner_placement_changed', $3)")
        .bind(session.into_uuid()).bind(boundary.entry().identity().into_uuid()).bind(Decimal::from(replacement.placement.revision().get())).execute(&mut **transaction).await?;
    crate::model_execution::insert_snapshot(transaction.as_mut(), boundary.frontier())
        .await
        .map_err(map_frontier_error)?;
    sqlx::query("INSERT INTO runner_placement_boundary (session_id, placement_revision, event_ordinal, command_id, semantic_entry_id, context_frontier_id) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(session.into_uuid()).bind(Decimal::from(replacement.placement.revision().get())).bind(Decimal::from(ordinal))
        .bind(command.into_uuid()).bind(boundary.entry().identity().into_uuid()).bind(boundary.frontier().frontier().snapshot().into_uuid()).execute(&mut **transaction).await?;
    sqlx::query("INSERT INTO runner_session_placement_frontier (session_id, placement_revision) VALUES ($1, $2) ON CONFLICT (session_id) DO UPDATE SET placement_revision = EXCLUDED.placement_revision")
        .bind(session.into_uuid()).bind(Decimal::from(replacement.placement.revision().get())).execute(&mut **transaction).await?;
    Ok(())
}

fn map_frontier_error(
    error: crate::model_execution::ModelCallRepositoryError,
) -> RunnerProtocolStoreError {
    use crate::model_execution::ModelCallRepositoryError;
    match error {
        ModelCallRepositoryError::Database { source, .. } => {
            RunnerProtocolStoreError::Database(source)
        }
        ModelCallRepositoryError::Corruption(_)
        | ModelCallRepositoryError::IdentityCollision(_)
        | ModelCallRepositoryError::NoLiveExecution
        | ModelCallRepositoryError::InvalidTransition(_) => {
            RunnerProtocolCorruption::InvalidEncoding.into()
        }
    }
}

pub(super) async fn insert_replacement_result(
    transaction: &mut Transaction<'_, Postgres>,
    command: DurableCommandId,
    result: ReplaceLostRunnerResult,
) -> Result<(), RunnerProtocolStoreError> {
    let (runner, revision, rejection) = match result {
        ReplaceLostRunnerResult::Replaced {
            runner,
            placement_revision,
        } => (
            Some(runner.into_uuid()),
            Some(Decimal::from(placement_revision.get())),
            None,
        ),
        ReplaceLostRunnerResult::Rejected(reason) => (None, None, Some(encode_rejection(reason))),
    };
    sqlx::query("INSERT INTO replace_lost_runner_result (command_id, result_kind, rejection_kind, runner_id, placement_revision) VALUES ($1, $2, $3, $4, $5)")
        .bind(command.into_uuid()).bind(if rejection.is_some() { "rejected" } else { "applied" })
        .bind(rejection).bind(runner).bind(revision).execute(&mut **transaction).await?;
    if rejection.is_some() {
        super::provisioning::release_rejected_replacement_workspace(transaction.as_mut(), command)
            .await?;
    }
    sqlx::query("SELECT pg_notify('runner_recovery', '')")
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

pub(super) async fn advance_enrollment_state(
    transaction: &mut Transaction<'_, Postgres>,
    enrollment: RunnerEnrollmentId,
    state: &str,
) -> Result<(), RunnerProtocolStoreError> {
    sqlx::query(
        "INSERT INTO runner_enrollment_audit
         (enrollment_id, revision, runner_id, authentication_reference_id, allowed_class_count, state_kind)
         SELECT enrollment_id, revision + 1, runner_id, authentication_reference_id, allowed_class_count, $2
         FROM runner_enrollment WHERE enrollment_id = $1",
    ).bind(enrollment.into_uuid()).bind(state).execute(&mut **transaction).await?;
    sqlx::query(
        "INSERT INTO runner_enrollment_audit_allowed_class (enrollment_id, revision, capability_class)
         SELECT enrollment.enrollment_id, enrollment.revision + 1, class.capability_class
         FROM runner_enrollment AS enrollment JOIN runner_enrollment_allowed_class AS class USING (enrollment_id)
         WHERE enrollment.enrollment_id = $1",
    ).bind(enrollment.into_uuid()).execute(&mut **transaction).await?;
    sqlx::query("UPDATE runner_enrollment SET revision = revision + 1, state_kind = $2 WHERE enrollment_id = $1")
        .bind(enrollment.into_uuid()).bind(state).execute(&mut **transaction).await?;
    Ok(())
}

fn encode_rejection(reason: RunnerRecoveryRejection) -> &'static str {
    match reason {
        RunnerRecoveryRejection::SessionNotFound => "session_not_found",
        RunnerRecoveryRejection::PlacementNotLost => "placement_not_lost",
        RunnerRecoveryRejection::ExistingControlRequired => "existing_control_required",
        RunnerRecoveryRejection::PendingRunnerNotFound => "pending_runner_not_found",
        RunnerRecoveryRejection::RunnerUnavailable => "runner_unavailable",
        RunnerRecoveryRejection::ReplacementPending => "replacement_pending",
        RunnerRecoveryRejection::PlacementUnavailable => "placement_unavailable",
        RunnerRecoveryRejection::RevisionWithoutRepository => "revision_without_repository",
        RunnerRecoveryRejection::ProvisioningFailed => "provisioning_failed",
    }
}

fn decode_rejection(reason: &str) -> Result<RunnerRecoveryRejection, RunnerProtocolStoreError> {
    Ok(match reason {
        "session_not_found" => RunnerRecoveryRejection::SessionNotFound,
        "placement_not_lost" => RunnerRecoveryRejection::PlacementNotLost,
        "existing_control_required" => RunnerRecoveryRejection::ExistingControlRequired,
        "pending_runner_not_found" => RunnerRecoveryRejection::PendingRunnerNotFound,
        "runner_unavailable" => RunnerRecoveryRejection::RunnerUnavailable,
        "replacement_pending" => RunnerRecoveryRejection::ReplacementPending,
        "placement_unavailable" => RunnerRecoveryRejection::PlacementUnavailable,
        "revision_without_repository" => RunnerRecoveryRejection::RevisionWithoutRepository,
        "provisioning_failed" => RunnerRecoveryRejection::ProvisioningFailed,
        _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    })
}
