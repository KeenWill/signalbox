//! Retained command-bound workspace authorization and exact ready receipts.

use super::*;
use signalbox_domain::{
    DurableCommandId, RunnerProvisioningAuthorizationId, RunnerReplacementProvisioning,
};

impl RunnerProtocolStore {
    /// Records a live provisioning refusal under its caller's current epoch.
    pub async fn record_replacement_provisioning_failure(
        &self,
        authorization: &RunnerReplacementProvisioning,
        epoch: RunnerConnectionEpoch,
        kind: signalbox_domain::RunnerProvisioningFailureKind,
        detail: &serde_json::Value,
    ) -> Result<(), RunnerProtocolStoreError> {
        self.store_replacement_provisioning_failure(authorization, Some(epoch), kind, detail)
            .await
    }

    /// Reconciles authenticated Resume failure evidence before opening its next epoch.
    pub async fn reconcile_replacement_provisioning_failure(
        &self,
        authorization: &RunnerReplacementProvisioning,
        kind: signalbox_domain::RunnerProvisioningFailureKind,
        detail: &serde_json::Value,
    ) -> Result<(), RunnerProtocolStoreError> {
        self.store_replacement_provisioning_failure(authorization, None, kind, detail)
            .await
    }

    async fn store_replacement_provisioning_failure(
        &self,
        authorization: &RunnerReplacementProvisioning,
        epoch: Option<RunnerConnectionEpoch>,
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
        sqlx::query(RUNNER_ENROLLMENT)
            .bind(authorization.enrollment.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        sqlx::query(RUNNER_PLACEMENT_CONNECTION_AUTHORITY)
            .bind(authorization.enrollment.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        if let Some(epoch) = epoch {
            let current =
                load_connection_head_in(transaction.as_mut(), authorization.enrollment).await?;
            if !current.is_some_and(|head| {
                head.epoch() == epoch
                    && matches!(
                        head.state(),
                        RunnerConnectionState::Connected | RunnerConnectionState::Suspect
                    )
            }) {
                return Err(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::CorrelationMismatch,
                ));
            }
        }
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
    /// Loads unconsumed workspace operations for the candidate’s current connection epoch.
    pub async fn replacement_provisioning(
        &self,
        enrollment: RunnerEnrollmentId,
        epoch: RunnerConnectionEpoch,
    ) -> Result<Vec<RunnerReplacementProvisioning>, RunnerProtocolStoreError> {
        let rows = sqlx::query(
            "SELECT operation.* FROM runner_replacement_provisioning_authorization AS operation
             JOIN runner_replacement_stage AS stage USING (command_id)
             JOIN runner_connection_authority_head AS head ON head.enrollment_id = operation.registration_enrollment_id
             JOIN runner_connection_event AS event ON event.enrollment_id = head.enrollment_id AND event.connection_epoch = head.connection_epoch AND event.event_ordinal = head.connection_event_ordinal
             WHERE operation.registration_enrollment_id = $1 AND head.connection_epoch = $2
               AND event.state_kind IN ('connected', 'suspect')
               AND NOT EXISTS (SELECT 1 FROM replace_lost_runner_result AS result WHERE result.command_id = operation.command_id)
               AND NOT EXISTS (SELECT 1 FROM runner_replacement_workspace_ready AS ready WHERE ready.authorization_id = operation.authorization_id)
             ORDER BY operation.authorization_id",
        ).bind(enrollment.into_uuid()).bind(Decimal::from(epoch.get())).fetch_all(&self.pool).await?;
        rows.iter().map(decode_authorization).collect()
    }

    /// Retains one exact workspace receipt; conflicting replay cannot replace its facts.
    pub async fn record_replacement_workspace_ready(
        &self,
        authorization: &RunnerReplacementProvisioning,
        epoch: RunnerConnectionEpoch,
        workspace: &ProvisionedWorkspace,
        manifest_digest: &super::workspaces::RunnerEvidenceDigest,
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
            || !connection.is_some_and(|head| {
                head.epoch() == epoch
                    && matches!(
                        head.state(),
                        RunnerConnectionState::Connected | RunnerConnectionState::Suspect
                    )
            })
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        if let Some(ready) = load_ready(transaction.as_mut(), authorization).await? {
            let prior_digest: String = sqlx::query_scalar("SELECT manifest_digest FROM runner_replacement_workspace_ready WHERE authorization_id = $1")
                .bind(authorization.authorization.into_uuid()).fetch_one(&mut *transaction).await?;
            if ready == *workspace && prior_digest == manifest_digest.as_str() {
                return Ok(());
            }
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }
        let terminal: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM replace_lost_runner_result WHERE command_id = $1 AND result_kind = 'rejected')",
        )
        .bind(authorization.command.into_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        let failed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM runner_replacement_provisioning_failure WHERE authorization_id = $1)",
        )
        .bind(authorization.authorization.into_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        if failed {
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
        sqlx::query("INSERT INTO runner_replacement_workspace_ready (authorization_id, manifest_id, working_directory, relative_path, clone_url_digest, recovery_revision, recovery_branch, manifest_digest) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
            .bind(authorization.authorization.into_uuid()).bind(workspace.manifest_id.into_uuid())
            .bind(workspace.working_directory.as_str()).bind(workspace.relative_path.as_str())
            .bind(workspace.canonical_clone_url_digest.as_ref().map(CanonicalCloneUrlDigest::as_str))
            .bind(revision).bind(branch).bind(manifest_digest.as_str()).execute(&mut *transaction).await?;
        if terminal {
            release_rejected_replacement_workspace(transaction.as_mut(), authorization.command)
                .await?;
        }
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
        return branch
            .map(|name| {
                Ok(WorkspaceRecovery::UnbornBranch {
                    name: WorkspaceBranchName::try_new(name)
                        .map_err(RunnerProtocolStoreError::Domain)?,
                })
            })
            .transpose();
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

pub(super) async fn release_rejected_replacement_workspace(
    connection: &mut PgConnection,
    command: DurableCommandId,
) -> Result<(), RunnerProtocolStoreError> {
    sqlx::query("INSERT INTO runner_workspace_release (manifest_id,session_id,placement_revision,runner_id,enrollment_id,connection_epoch,connection_event_ordinal,relative_path,authorization_id)
        SELECT ready.manifest_id,operation.session_id,operation.placement_revision,operation.runner_id,operation.registration_enrollment_id,
            head.connection_epoch,head.connection_event_ordinal,ready.relative_path,operation.authorization_id
        FROM runner_replacement_provisioning_authorization operation JOIN runner_replacement_workspace_ready ready USING (authorization_id)
        JOIN runner_enrollment enrollment ON enrollment.enrollment_id = operation.registration_enrollment_id
        JOIN runner_connection_authority_head head ON head.enrollment_id = enrollment.enrollment_id
        JOIN runner_connection_event event ON event.enrollment_id = head.enrollment_id AND event.connection_epoch = head.connection_epoch AND event.event_ordinal = head.connection_event_ordinal
        WHERE operation.command_id = $1 AND enrollment.state_kind <> 'revoked' AND event.state_kind IN ('connected','suspect')
        ON CONFLICT DO NOTHING")
        .bind(command.into_uuid()).execute(&mut *connection).await?;
    sqlx::query("INSERT INTO runner_workspace_leak (runner_id,locator,entry_digest,kind,session_id,placement_revision)
        SELECT operation.runner_id,ready.relative_path,ready.manifest_digest,'retired_present',operation.session_id,operation.placement_revision
        FROM runner_replacement_provisioning_authorization operation JOIN runner_replacement_workspace_ready ready USING (authorization_id)
        WHERE operation.command_id = $1 AND NOT EXISTS (SELECT 1 FROM runner_workspace_release release WHERE release.manifest_id = ready.manifest_id)
        ON CONFLICT (runner_id,locator,entry_digest) DO UPDATE SET report_derived = false")
        .bind(command.into_uuid()).execute(connection).await?;
    Ok(())
}
