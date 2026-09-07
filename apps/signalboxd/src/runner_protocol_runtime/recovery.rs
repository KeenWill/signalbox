//! Command-bound workspace operations on the runner connection.

use super::*;
use signalbox_domain::{
    ProvisionedWorkspace, RunnerProvisioningAuthorizationId, RunnerReplacementProvisioning,
    RunnerWorkingDirectory, WorkspaceManifestId, WorkspaceRelativePath,
};
use signalbox_persistence::runner_protocol::{RunnerRecoveryError, RunnerRecoveryOutcome};
use signalbox_runner_wire::{
    ProvisionCorrelation, Recovery, SandboxProfile, WorkspaceProvision, WorkspaceReady,
    WorkspaceRecorded,
};

pub(super) fn store_error(error: RunnerRecoveryError) -> RunnerProtocolStoreError {
    match error {
        RunnerRecoveryError::Store(error) => error,
        RunnerRecoveryError::InvalidCommandId | RunnerRecoveryError::Registry(_) => {
            RunnerProtocolStoreError::Domain(RunnerDomainError::CorruptStoredFacts)
        }
    }
}

impl PostgresRunnerRegistrationService {
    pub(super) async fn promotion_receipt_durably(
        &self,
        enrollment: CanonicalUuid,
    ) -> Result<Option<Enrolled>, RunnerRegistrationFailure> {
        let failure = |error| {
            store_failure(
                RunnerInboundFrameKind::Enrolled,
                AvailableCorrelation::None,
                error,
            )
        };
        let Some(receipt) = self
            .store
            .promoted_runner_receipt(RunnerEnrollmentId::from_uuid(enrollment.into_uuid()))
            .await
            .map_err(failure)?
        else {
            return Ok(None);
        };
        let Some(connection) = self
            .store
            .load_connection(receipt.identities().enrollment())
            .await
            .map_err(failure)?
        else {
            return Ok(None);
        };
        let advertisement = signalbox_runner_wire::Advertisement::try_from(
            &receipt.advertisement(),
        )
        .map_err(|_| {
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Enrolled,
                AvailableCorrelation::None,
                RejectionCode::Unavailable,
            )
        })?;
        let digest = signalbox_runner_wire::advertisement_digest(&advertisement).map_err(|_| {
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Enrolled,
                AvailableCorrelation::None,
                RejectionCode::Unavailable,
            )
        })?;
        Ok(Some(Enrolled {
            request_id: CanonicalUuid::from_uuid(receipt.request().into_uuid()),
            enrollment_id: enrollment,
            runner_id: CanonicalUuid::from_uuid(receipt.identities().runner().into_uuid()),
            authentication_id: CanonicalUuid::from_uuid(
                receipt.identities().authentication().into_uuid(),
            ),
            registration_revision: positive_revision(receipt.registration().revision())?,
            connection_epoch: positive_epoch(connection.epoch())?,
            advertisement_digest: digest,
        }))
    }
    pub(super) async fn replacement_releases_durably(
        &self,
        enrollment: CanonicalUuid,
    ) -> Result<Vec<signalbox_runner_wire::WorkspaceRelease>, RunnerRegistrationFailure> {
        let workspaces = self
            .store
            .replacement_workspace_releases(RunnerEnrollmentId::from_uuid(enrollment.into_uuid()))
            .await
            .map_err(|error| {
                store_failure(
                    RunnerInboundFrameKind::WorkspaceRelease,
                    AvailableCorrelation::None,
                    error,
                )
            })?;
        workspaces
            .into_iter()
            .map(|workspace| {
                Ok(signalbox_runner_wire::WorkspaceRelease {
                    correlation: signalbox_runner_wire::ReleaseCorrelation {
                        session_id: CanonicalUuid::from_uuid(workspace.session.into_uuid()),
                        placement_revision: PositiveU64::try_new(
                            workspace.placement_revision.get(),
                        )
                        .map_err(|_| {
                            RunnerRegistrationFailure::new(
                                RunnerInboundFrameKind::WorkspaceRelease,
                                AvailableCorrelation::None,
                                RejectionCode::Unavailable,
                            )
                        })?,
                        runner_id: CanonicalUuid::from_uuid(workspace.runner.into_uuid()),
                        manifest_id: CanonicalUuid::from_uuid(workspace.manifest_id.into_uuid()),
                    },
                })
            })
            .collect()
    }

    pub(super) async fn workspace_released_durably(
        &self,
        enrollment: CanonicalUuid,
        receipt: signalbox_runner_wire::WorkspaceReleased,
    ) -> Result<signalbox_runner_wire::WorkspaceReleaseRecorded, RunnerRegistrationFailure> {
        let correlation = receipt.correlation;
        let failure = |error| {
            store_failure(
                RunnerInboundFrameKind::WorkspaceReleased,
                AvailableCorrelation::Release(correlation.clone()),
                error,
            )
        };
        let revision =
            signalbox_domain::RunnerGeneration::try_from_u64(correlation.placement_revision.get())
                .ok_or_else(|| {
                    failure(RunnerProtocolStoreError::Domain(
                        RunnerDomainError::CorrelationMismatch,
                    ))
                })?;
        self.store
            .record_replacement_workspace_released(
                RunnerEnrollmentId::from_uuid(enrollment.into_uuid()),
                signalbox_domain::SessionId::from_uuid(correlation.session_id.into_uuid()),
                revision,
                RunnerId::from_uuid(correlation.runner_id.into_uuid()),
                WorkspaceManifestId::from_uuid(correlation.manifest_id.into_uuid()),
            )
            .await
            .map_err(failure)?;
        Ok(signalbox_runner_wire::WorkspaceReleaseRecorded { correlation })
    }
    pub(super) async fn provisioning_failed_durably(
        &self,
        enrollment: CanonicalUuid,
        message: signalbox_runner_wire::OperationFailed,
    ) -> Result<signalbox_runner_wire::OperationFailureRecorded, RunnerRegistrationFailure> {
        use signalbox_domain::RunnerProvisioningFailureKind as Kind;
        use signalbox_runner_wire::{FailureCategory, OperationCorrelation};
        let failure = message.failure;
        let rejected = || {
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::OperationFailed,
                AvailableCorrelation::OperationFailure(failure.correlation.clone()),
                RejectionCode::CorrelationMismatch,
            )
        };
        failure.validate().map_err(|_| rejected())?;
        let OperationCorrelation::Provision(correlation) = &failure.correlation else {
            return Err(rejected());
        };
        let authorization = self
            .store
            .replacement_provisioning_authorization(RunnerProvisioningAuthorizationId::from_uuid(
                correlation.authorization_id.into_uuid(),
            ))
            .await
            .map_err(|error| {
                store_failure(
                    RunnerInboundFrameKind::OperationFailed,
                    AvailableCorrelation::OperationFailure(failure.correlation.clone()),
                    error,
                )
            })?
            .ok_or_else(rejected)?;
        if authorization.enrollment.into_uuid() != enrollment.into_uuid()
            || provision_message(&authorization)?.correlation != *correlation
        {
            return Err(rejected());
        }
        let kind = match failure.category {
            FailureCategory::CredentialUnavailable => Kind::CredentialUnavailable,
            FailureCategory::RepositoryUnavailable => Kind::RepositoryUnavailable,
            FailureCategory::SandboxUnavailable => Kind::SandboxUnavailable,
            FailureCategory::WorkspaceConflict => Kind::WorkspaceConflict,
            FailureCategory::WorkspaceCleanupFailed | FailureCategory::LeaseAdmissionRefused => {
                return Err(rejected());
            }
        };
        let detail = serde_json::to_value(&failure.detail).map_err(|_| rejected())?;
        self.store
            .record_replacement_provisioning_failure(&authorization, kind, &detail)
            .await
            .map_err(|error| {
                store_failure(
                    RunnerInboundFrameKind::OperationFailed,
                    AvailableCorrelation::OperationFailure(failure.correlation.clone()),
                    error,
                )
            })?;
        Ok(signalbox_runner_wire::OperationFailureRecorded {
            correlation: failure.correlation,
        })
    }
    pub(super) async fn replacement_operations_durably(
        &self,
        enrollment: CanonicalUuid,
    ) -> Result<Vec<WorkspaceProvision>, RunnerRegistrationFailure> {
        let operations = self
            .store
            .replacement_provisioning(RunnerEnrollmentId::from_uuid(enrollment.into_uuid()))
            .await
            .map_err(|error| {
                store_failure(
                    RunnerInboundFrameKind::WorkspaceProvision,
                    AvailableCorrelation::None,
                    error,
                )
            })?;
        operations.iter().map(provision_message).collect()
    }

    pub(super) async fn workspace_ready_durably(
        &self,
        enrollment: CanonicalUuid,
        receipt: WorkspaceReady,
    ) -> Result<Option<WorkspaceRecorded>, RunnerRegistrationFailure> {
        let correlation = AvailableCorrelation::Provision(receipt.correlation.clone());
        let failure = |error| {
            store_failure(
                RunnerInboundFrameKind::WorkspaceReady,
                correlation.clone(),
                error,
            )
        };
        let authorization = self
            .store
            .replacement_provisioning_authorization(RunnerProvisioningAuthorizationId::from_uuid(
                receipt.correlation.authorization_id.into_uuid(),
            ))
            .await
            .map_err(failure)?
            .ok_or_else(|| {
                RunnerRegistrationFailure::new(
                    RunnerInboundFrameKind::WorkspaceReady,
                    correlation.clone(),
                    RejectionCode::CorrelationMismatch,
                )
            })?;
        if authorization.enrollment.into_uuid() != enrollment.into_uuid()
            || provision_message(&authorization)?.correlation != receipt.correlation
        {
            return Err(RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::WorkspaceReady,
                correlation,
                RejectionCode::CorrelationMismatch,
            ));
        }
        let manifest = &receipt.ready.manifest;
        let workspace = ProvisionedWorkspace {
            session: authorization.session,
            runner: authorization.runner,
            placement_revision: authorization.placement_revision,
            repository: authorization.repository.clone(),
            credential_profile: authorization.credential_profile.clone(),
            sandbox: authorization.sandbox,
            canonical_clone_url_digest: manifest
                .canonical_clone_url_digest
                .as_ref()
                .map(|digest| {
                    signalbox_domain::CanonicalCloneUrlDigest::try_new(digest.as_str().to_owned())
                })
                .transpose()
                .map_err(|error| failure(RunnerProtocolStoreError::Domain(error)))?,
            working_directory: RunnerWorkingDirectory::try_new(receipt.working_directory.clone())
                .map_err(|error| failure(RunnerProtocolStoreError::Domain(error)))?,
            relative_path: WorkspaceRelativePath::try_new(manifest.relative_path.clone())
                .map_err(|error| failure(RunnerProtocolStoreError::Domain(error)))?,
            manifest_id: WorkspaceManifestId::from_uuid(manifest.manifest_id.into_uuid()),
            recovery: match &manifest.recovery {
                Some(Recovery::Commit { revision }) => {
                    Some(signalbox_domain::WorkspaceRecovery::Commit {
                        revision: signalbox_domain::WorkspaceRevision::try_new(revision.clone())
                            .map_err(|error| failure(RunnerProtocolStoreError::Domain(error)))?,
                    })
                }
                Some(Recovery::Branch { name, revision }) => {
                    Some(signalbox_domain::WorkspaceRecovery::Branch {
                        name: signalbox_domain::WorkspaceBranchName::try_new(name.clone())
                            .map_err(|error| failure(RunnerProtocolStoreError::Domain(error)))?,
                        revision: signalbox_domain::WorkspaceRevision::try_new(revision.clone())
                            .map_err(|error| failure(RunnerProtocolStoreError::Domain(error)))?,
                    })
                }
                None => None,
            },
        };
        self.store
            .record_replacement_workspace_ready(&authorization, &workspace)
            .await
            .map_err(failure)?;
        let outcome = self
            .store
            .resume_runner_replacement(authorization.command)
            .await
            .map_err(|error| failure(store_error(error)))?;
        match outcome {
            RunnerRecoveryOutcome::Recorded(_) => Ok(Some(WorkspaceRecorded {
                correlation: receipt.correlation,
                manifest_id: manifest.manifest_id,
                manifest_digest: receipt.ready.manifest_digest,
            })),
            RunnerRecoveryOutcome::Pending => Ok(None),
            RunnerRecoveryOutcome::ConflictingReuse => Err(failure(
                RunnerProtocolStoreError::Domain(RunnerDomainError::CorruptStoredFacts),
            )),
        }
    }
}

fn provision_message(
    operation: &RunnerReplacementProvisioning,
) -> Result<WorkspaceProvision, RunnerRegistrationFailure> {
    let invalid = |_| {
        RunnerRegistrationFailure::new(
            RunnerInboundFrameKind::WorkspaceProvision,
            AvailableCorrelation::None,
            RejectionCode::Unavailable,
        )
    };
    Ok(WorkspaceProvision {
        correlation: ProvisionCorrelation {
            authorization_id: CanonicalUuid::from_uuid(operation.authorization.into_uuid()),
            session_id: CanonicalUuid::from_uuid(operation.session.into_uuid()),
            placement_revision: PositiveU64::try_new(operation.placement_revision.get())
                .map_err(invalid)?,
            runner_id: CanonicalUuid::from_uuid(operation.runner.into_uuid()),
            registration_revision: PositiveU64::try_new(operation.registration_revision.get())
                .map_err(invalid)?,
            repository: operation
                .repository
                .as_ref()
                .map(|repository| {
                    signalbox_runner_wire::RepositoryKey::try_new(repository.as_str().to_owned())
                })
                .transpose()
                .map_err(invalid)?,
            sandbox_profile: match operation.sandbox {
                signalbox_domain::RunnerSandboxProfile::Ambient => SandboxProfile::Ambient,
                signalbox_domain::RunnerSandboxProfile::WorkspaceRestricted => {
                    SandboxProfile::WorkspaceRestricted
                }
            },
            credential_profile: operation
                .credential_profile
                .as_ref()
                .map(|profile| {
                    signalbox_runner_wire::ProfileName::try_new(profile.as_str().to_owned())
                })
                .transpose()
                .map_err(invalid)?,
        },
        recovery: operation.recovery.as_ref().map(|recovery| match recovery {
            signalbox_domain::WorkspaceRecovery::Commit { revision } => Recovery::Commit {
                revision: revision.as_str().to_owned(),
            },
            signalbox_domain::WorkspaceRecovery::Branch { name, revision } => Recovery::Branch {
                name: name.as_str().to_owned(),
                revision: revision.as_str().to_owned(),
            },
        }),
    })
}
