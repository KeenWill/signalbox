//! Retained command-bound workspace authorization and exact ready receipts.

use super::*;
use signalbox_domain::{
    DurableCommandId, RunnerProvisioningAuthorizationId, RunnerReplacementProvisioning,
};

impl RunnerProtocolStore {
    /// Loads only this candidate's command-retired, manifest-backed staging workspaces.
    pub async fn replacement_workspace_releases(
        &self,
        enrollment: RunnerEnrollmentId,
    ) -> Result<Vec<ProvisionedWorkspace>, RunnerProtocolStoreError> {
        let mut connection = self.pool.acquire().await?;
        let rows = sqlx::query("SELECT operation.* FROM runner_replacement_provisioning_authorization AS operation
            JOIN replace_lost_runner_result AS result USING (command_id)
            JOIN runner_replacement_workspace_ready AS ready USING (authorization_id)
            JOIN runner_replacement_workspace_release AS cleanup USING (authorization_id)
            JOIN runner_connection_authority_head AS head ON head.enrollment_id = operation.registration_enrollment_id
            WHERE operation.registration_enrollment_id = $1 AND result.result_kind = 'rejected'
              AND cleanup.connection_epoch = head.connection_epoch
              AND (head.latest_loss_epoch IS NULL OR head.latest_loss_epoch < cleanup.connection_epoch)
              AND NOT EXISTS (SELECT 1 FROM runner_replacement_workspace_released AS released WHERE released.authorization_id = operation.authorization_id)")
            .bind(enrollment.into_uuid()).fetch_all(&mut *connection).await?;
        let mut workspaces = Vec::new();
        for row in rows {
            let authorization = decode_authorization(&row)?;
            workspaces.push(
                load_ready(&mut connection, &authorization)
                    .await?
                    .ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
            );
        }
        Ok(workspaces)
    }

    /// Records cleanup only for the exact staging workspace a rejected command retired.
    pub async fn record_replacement_workspace_released(
        &self,
        enrollment: RunnerEnrollmentId,
        session: SessionId,
        revision: RunnerGeneration,
        runner: RunnerId,
        manifest: WorkspaceManifestId,
    ) -> Result<(), RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(RUNNER_RETRY_REPLACEMENT_SCHEDULER)
            .bind(session.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let authorization: Option<Uuid> = sqlx::query_scalar("SELECT operation.authorization_id
            FROM runner_replacement_provisioning_authorization AS operation
            JOIN replace_lost_runner_result AS result USING (command_id)
            JOIN runner_replacement_workspace_ready AS ready USING (authorization_id)
            JOIN runner_replacement_workspace_release AS cleanup USING (authorization_id)
            WHERE operation.registration_enrollment_id = $1 AND operation.session_id = $2
              AND operation.placement_revision = $3 AND operation.runner_id = $4
              AND ready.manifest_id = $5 AND result.result_kind = 'rejected'
              AND NOT EXISTS (SELECT 1 FROM runner_replacement_workspace_consumption AS consumption WHERE consumption.authorization_id = operation.authorization_id)")
            .bind(enrollment.into_uuid()).bind(session.into_uuid()).bind(Decimal::from(revision.get()))
            .bind(runner.into_uuid()).bind(manifest.into_uuid()).fetch_optional(&mut *transaction).await?;
        let authorization = authorization.ok_or(RunnerProtocolStoreError::Domain(
            RunnerDomainError::CorrelationMismatch,
        ))?;
        sqlx::query("INSERT INTO runner_replacement_workspace_released (authorization_id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(authorization).execute(&mut *transaction).await?;
        commit_mutation(transaction).await
    }
    /// Records an exact provisioning refusal and settles its owning command atomically.
    pub async fn record_replacement_provisioning_failure(
        &self,
        authorization: &RunnerReplacementProvisioning,
        kind: signalbox_domain::RunnerProvisioningFailureKind,
        detail: &serde_json::Value,
    ) -> Result<(), RunnerProtocolStoreError> {
        use signalbox_domain::RunnerProvisioningFailureKind as Kind;
        let kind = match kind {
            Kind::CredentialUnavailable => "credential_unavailable",
            Kind::RepositoryUnavailable => "repository_unavailable",
            Kind::SandboxUnavailable => "sandbox_unavailable",
            Kind::WorkspaceConflict => "workspace_conflict",
        };
        let mut transaction = self.pool.begin().await?;
        sqlx::query(RUNNER_RETRY_REPLACEMENT_SCHEDULER)
            .bind(authorization.session.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let row = sqlx::query("SELECT * FROM runner_replacement_provisioning_authorization WHERE authorization_id = $1")
            .bind(authorization.authorization.into_uuid()).fetch_one(&mut *transaction).await?;
        if decode_authorization(&row)? != *authorization {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }
        if let Some(row) = sqlx::query("SELECT failure_kind, detail FROM runner_replacement_provisioning_failure WHERE authorization_id = $1")
            .bind(authorization.authorization.into_uuid()).fetch_optional(&mut *transaction).await? {
            if row.decode_column::<String>("failure_kind")? == kind && row.decode_column::<serde_json::Value>("detail")? == *detail {
                return Ok(());
            }
            return Err(RunnerProtocolStoreError::Domain(RunnerDomainError::CorrelationMismatch));
        }
        let settled: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM replace_lost_runner_result WHERE command_id = $1) OR EXISTS (SELECT 1 FROM runner_replacement_workspace_ready WHERE authorization_id = $2)")
            .bind(authorization.command.into_uuid()).bind(authorization.authorization.into_uuid()).fetch_one(&mut *transaction).await?;
        if settled {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        sqlx::query("INSERT INTO runner_replacement_provisioning_failure (authorization_id, failure_kind, detail) VALUES ($1, $2, $3)")
            .bind(authorization.authorization.into_uuid()).bind(kind).bind(detail).execute(&mut *transaction).await?;
        super::recovery::insert_replacement_result(
            &mut transaction,
            authorization.command,
            signalbox_domain::ReplaceLostRunnerResult::Rejected(
                signalbox_domain::RunnerRecoveryRejection::ProvisioningFailed,
            ),
        )
        .await?;
        sqlx::query("DELETE FROM runner_replacement_stage WHERE command_id = $1")
            .bind(authorization.command.into_uuid())
            .execute(&mut *transaction)
            .await?;
        commit_mutation(transaction).await
    }
    /// Loads the exact immutable authorization named by a ready receipt.
    pub async fn replacement_provisioning_authorization(
        &self,
        authorization: RunnerProvisioningAuthorizationId,
    ) -> Result<Option<RunnerReplacementProvisioning>, RunnerProtocolStoreError> {
        sqlx::query("SELECT * FROM runner_replacement_provisioning_authorization WHERE authorization_id = $1")
            .bind(authorization.into_uuid()).fetch_optional(&self.pool).await?.as_ref().map(decode_authorization).transpose()
    }
    /// Loads unconsumed workspace operations for one exact candidate enrollment.
    pub async fn replacement_provisioning(
        &self,
        enrollment: RunnerEnrollmentId,
    ) -> Result<Vec<RunnerReplacementProvisioning>, RunnerProtocolStoreError> {
        let rows = sqlx::query(
            "SELECT operation.* FROM runner_replacement_provisioning_authorization AS operation
             JOIN runner_replacement_stage AS stage USING (command_id)
             WHERE operation.registration_enrollment_id = $1
               AND NOT EXISTS (SELECT 1 FROM replace_lost_runner_result AS result WHERE result.command_id = operation.command_id)
               AND NOT EXISTS (SELECT 1 FROM runner_replacement_workspace_ready AS ready WHERE ready.authorization_id = operation.authorization_id)
             ORDER BY operation.authorization_id",
        ).bind(enrollment.into_uuid()).fetch_all(&self.pool).await?;
        rows.iter().map(decode_authorization).collect()
    }

    /// Retains one exact workspace receipt; conflicting replay cannot replace its facts.
    pub async fn record_replacement_workspace_ready(
        &self,
        authorization: &RunnerReplacementProvisioning,
        workspace: &ProvisionedWorkspace,
    ) -> Result<(), RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(RUNNER_RETRY_REPLACEMENT_SCHEDULER)
            .bind(authorization.session.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let row = sqlx::query("SELECT * FROM runner_replacement_provisioning_authorization WHERE authorization_id = $1")
            .bind(authorization.authorization.into_uuid()).fetch_one(&mut *transaction).await?;
        let retained = decode_authorization(&row)?;
        if retained != *authorization || !matches_authorization(authorization, workspace) {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }
        if let Some(ready) = load_ready(transaction.as_mut(), authorization).await? {
            if ready == *workspace {
                return Ok(());
            }
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }
        let terminal: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM replace_lost_runner_result WHERE command_id = $1)",
        )
        .bind(authorization.command.into_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        if terminal {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        sqlx::query(RUNNER_ENROLLMENT)
            .bind(authorization.enrollment.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        sqlx::query(RUNNER_PLACEMENT_CONNECTION_AUTHORITY)
            .bind(authorization.enrollment.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let connection =
            load_connection_head_in(transaction.as_mut(), authorization.enrollment).await?;
        let enrollment = load_enrollment_in(transaction.as_mut(), authorization.enrollment)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        if enrollment.state() == RunnerEnrollmentState::Revoked
            || !connection.is_some_and(|head| head.state() == RunnerConnectionState::Connected)
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let revision: Decimal = sqlx::query_scalar(RUNNER_REGISTRATION_HEAD)
            .bind(authorization.enrollment.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        if decode_generation(revision)? != authorization.registration_revision {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::RegistrationChanged,
            ));
        }
        let (_, branch, revision) = workspace
            .recovery
            .as_ref()
            .map(encode_workspace_recovery)
            .unwrap_or((None, None, None));
        sqlx::query("INSERT INTO runner_replacement_workspace_ready (authorization_id, manifest_id, working_directory, relative_path, clone_url_digest, recovery_revision, recovery_branch) VALUES ($1, $2, $3, $4, $5, $6, $7)")
            .bind(authorization.authorization.into_uuid()).bind(workspace.manifest_id.into_uuid())
            .bind(workspace.working_directory.as_str()).bind(workspace.relative_path.as_str())
            .bind(workspace.canonical_clone_url_digest.as_ref().map(CanonicalCloneUrlDigest::as_str))
            .bind(revision).bind(branch).execute(&mut *transaction).await?;
        sqlx::query("SELECT pg_notify('runner_recovery', '')")
            .execute(&mut *transaction)
            .await?;
        commit_mutation(transaction).await
    }
}

fn matches_authorization(
    authorization: &RunnerReplacementProvisioning,
    workspace: &ProvisionedWorkspace,
) -> bool {
    let terminal = if authorization.repository.is_some() {
        "repo"
    } else {
        "work"
    };
    workspace.session == authorization.session
        && workspace.runner == authorization.runner
        && workspace.placement_revision == authorization.placement_revision
        && workspace.repository == authorization.repository
        && workspace.sandbox == authorization.sandbox
        && workspace.credential_profile == authorization.credential_profile
        && workspace.recovery == authorization.recovery
        && workspace.canonical_clone_url_digest.is_some() == authorization.repository.is_some()
        && workspace.relative_path.as_str()
            == format!(
                "sessions/{}/{}/{terminal}",
                authorization.session.into_uuid(),
                authorization.placement_revision.get()
            )
}

pub(super) fn decode_authorization(
    row: &PgRow,
) -> Result<RunnerReplacementProvisioning, RunnerProtocolStoreError> {
    Ok(RunnerReplacementProvisioning {
        authorization: RunnerProvisioningAuthorizationId::from_uuid(
            row.decode_column("authorization_id")?,
        ),
        command: DurableCommandId::from_uuid(row.decode_column("command_id")?),
        enrollment: runner_enrollment_id(row.decode_column("registration_enrollment_id")?),
        registration_revision: decode_generation(row.decode_column("registration_revision")?)?,
        session: SessionId::from_uuid(row.decode_column("session_id")?),
        placement_revision: decode_generation(row.decode_column("placement_revision")?)?,
        runner: runner_id(row.decode_column("runner_id")?),
        repository: row
            .decode_column::<Option<String>>("repository_key")?
            .map(repository_key)
            .transpose()?,
        sandbox: decode_sandbox(row.decode_column("sandbox_profile")?)?,
        credential_profile: row
            .decode_column::<Option<String>>("credential_profile_name")?
            .map(profile_name)
            .transpose()?,
        recovery: decode_recovery(
            row.decode_column("checkout_revision")?,
            row.decode_column("checkout_branch")?,
        )?,
    })
}

pub(super) async fn load_ready(
    connection: &mut PgConnection,
    authorization: &RunnerReplacementProvisioning,
) -> Result<Option<ProvisionedWorkspace>, RunnerProtocolStoreError> {
    let row =
        sqlx::query("SELECT * FROM runner_replacement_workspace_ready WHERE authorization_id = $1")
            .bind(authorization.authorization.into_uuid())
            .fetch_optional(connection)
            .await?;
    row.map(|row| {
        let workspace = ProvisionedWorkspace {
            session: authorization.session,
            placement_revision: authorization.placement_revision,
            runner: authorization.runner,
            repository: authorization.repository.clone(),
            canonical_clone_url_digest: row
                .decode_column::<Option<String>>("clone_url_digest")?
                .map(CanonicalCloneUrlDigest::try_new)
                .transpose()
                .map_err(RunnerProtocolStoreError::Domain)?,
            credential_profile: authorization.credential_profile.clone(),
            sandbox: authorization.sandbox,
            working_directory: RunnerWorkingDirectory::try_new(
                row.decode_column("working_directory")?,
            )
            .map_err(RunnerProtocolStoreError::Domain)?,
            relative_path: WorkspaceRelativePath::try_new(row.decode_column("relative_path")?)
                .map_err(RunnerProtocolStoreError::Domain)?,
            manifest_id: WorkspaceManifestId::from_uuid(row.decode_column("manifest_id")?),
            recovery: decode_recovery(
                row.decode_column("recovery_revision")?,
                row.decode_column("recovery_branch")?,
            )?,
        };
        if !matches_authorization(authorization, &workspace) {
            return Err(RunnerProtocolCorruption::InvalidEncoding.into());
        }
        Ok(workspace)
    })
    .transpose()
}

fn decode_recovery(
    revision: Option<String>,
    branch: Option<String>,
) -> Result<Option<WorkspaceRecovery>, RunnerProtocolStoreError> {
    let Some(revision) = revision else {
        return if branch.is_none() {
            Ok(None)
        } else {
            Err(RunnerProtocolCorruption::InvalidEncoding.into())
        };
    };
    let revision =
        WorkspaceRevision::try_new(revision).map_err(RunnerProtocolStoreError::Domain)?;
    Ok(Some(match branch {
        Some(name) => WorkspaceRecovery::Branch {
            name: WorkspaceBranchName::try_new(name).map_err(RunnerProtocolStoreError::Domain)?,
            revision,
        },
        None => WorkspaceRecovery::Commit { revision },
    }))
}
