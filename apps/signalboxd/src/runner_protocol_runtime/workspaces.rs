//! Checked workspace diagnostic and release boundaries.

use super::*;
use signalbox_persistence::runner_protocol::workspaces::{
    RunnerEvidenceDigest, RunnerWorkspaceLeak, RunnerWorkspaceLeakKind, RunnerWorkspaceLeakPage,
    RunnerWorkspaceRelease, RunnerWorkspaceReleaseState,
};
use signalbox_runner_wire::{
    FailureCategory, OperationCorrelation, ReleasePhase, WorkspaceLeakPage, WorkspaceLeakRecorded,
    WorkspaceOperation,
};

pub(super) fn release_receipt(
    correlation: &signalbox_runner_wire::ReleaseCorrelation,
) -> Result<RunnerWorkspaceRelease, RunnerProtocolStoreError> {
    Ok(RunnerWorkspaceRelease {
        session: signalbox_domain::SessionId::from_uuid(correlation.session_id.into_uuid()),
        placement_revision: signalbox_domain::RunnerGeneration::try_from_u64(
            correlation.placement_revision.get(),
        )
        .ok_or(RunnerProtocolStoreError::Domain(
            RunnerDomainError::CorrelationMismatch,
        ))?,
        runner: RunnerId::from_uuid(correlation.runner_id.into_uuid()),
        manifest: signalbox_domain::WorkspaceManifestId::from_uuid(
            correlation.manifest_id.into_uuid(),
        ),
    })
}

impl PostgresRunnerRegistrationService {
    pub(super) async fn leak_page_durably(
        &self,
        enrollment: CanonicalUuid,
        epoch: Option<PositiveU64>,
        message: WorkspaceLeakPage,
    ) -> Result<WorkspaceLeakRecorded, RunnerRegistrationFailure> {
        let page = message.page;
        let rejected = || {
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::WorkspaceLeakPage,
                AvailableCorrelation::LeakPage(page.correlation.clone()),
                RejectionCode::CorrelationMismatch,
            )
        };
        page.validate().map_err(|_| rejected())?;
        let digest = |value: &signalbox_runner_wire::Digest| {
            RunnerEvidenceDigest::try_new(value.as_str().to_owned()).ok_or_else(rejected)
        };
        let revision = |value: PositiveU64| {
            signalbox_domain::RunnerGeneration::try_from_u64(value.get()).ok_or_else(rejected)
        };
        let facts = page
            .facts
            .iter()
            .map(|fact| {
                use signalbox_runner_wire::LeakFactKind as Kind;
                Ok(RunnerWorkspaceLeak {
                    kind: match fact.kind {
                        Kind::UnknownManifest => RunnerWorkspaceLeakKind::UnknownManifest,
                        Kind::RetiredPresent => RunnerWorkspaceLeakKind::RetiredPresent,
                        Kind::ManifestConflict => RunnerWorkspaceLeakKind::ManifestConflict,
                        Kind::CleanupFailed => RunnerWorkspaceLeakKind::CleanupFailed,
                        Kind::Unreconciled => RunnerWorkspaceLeakKind::Unreconciled,
                    },
                    locator: signalbox_domain::WorkspaceRelativePath::try_new(fact.locator.clone())
                        .map_err(|_| rejected())?,
                    entry_digest: digest(&fact.entry_digest)?,
                    session: fact
                        .session
                        .map(|session| signalbox_domain::SessionId::from_uuid(session.into_uuid())),
                    placement_revision: fact.placement_revision.map(revision).transpose()?,
                })
            })
            .collect::<Result<Vec<_>, RunnerRegistrationFailure>>()?;
        let page_to_store = RunnerWorkspaceLeakPage {
            registration_revision: revision(page.correlation.registration_revision)?,
            report_digest: digest(&page.correlation.report_digest)?,
            page: revision(page.correlation.page)?,
            prior_page_digest: page.prior_page_digest.as_ref().map(digest).transpose()?,
            final_page: page.final_page,
            page_digest: digest(&page.page_digest)?,
            facts,
        };
        let enrollment = RunnerEnrollmentId::from_uuid(enrollment.into_uuid());
        match epoch {
            Some(epoch) => {
                self.store
                    .record_workspace_leak_page(
                        enrollment,
                        RunnerConnectionEpoch::try_from_u64(epoch.get()).ok_or_else(rejected)?,
                        &page_to_store,
                    )
                    .await
            }
            None => {
                self.store
                    .reconcile_workspace_leak_page(enrollment, &page_to_store)
                    .await
            }
        }
        .map_err(|error| {
            store_failure(
                RunnerInboundFrameKind::WorkspaceLeakPage,
                AvailableCorrelation::LeakPage(page.correlation.clone()),
                error,
            )
        })?;
        Ok(WorkspaceLeakRecorded {
            correlation: page.correlation,
            page_digest: page.page_digest,
        })
    }

    pub(super) async fn release_resume_directive(
        &self,
        request: &Resume,
        identities: IssuedRunnerEnrollmentIdentities,
    ) -> Result<Directive<OperationCorrelation>, RunnerRegistrationFailure> {
        let rejected = || {
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Resume,
                AvailableCorrelation::Enrollment(request.request_id),
                RejectionCode::CorrelationMismatch,
            )
        };
        let Some(WorkspaceOperation::Release { correlation, phase }) =
            &request.inventory.workspace_operation
        else {
            return Err(rejected());
        };
        if request.inventory.lease.is_some()
            || request.inventory.result.is_some()
            || correlation.runner_id.into_uuid() != identities.runner().into_uuid()
        {
            return Err(rejected());
        }
        let operation = OperationCorrelation::Release(correlation.clone());
        let detail = if let Some(failure) = &request.inventory.operation_failure {
            if failure.correlation != operation
                || failure.category != FailureCategory::WorkspaceCleanupFailed
                || *phase != ReleasePhase::ReleaseAccepted
            {
                return Err(rejected());
            }
            Some(serde_json::to_value(&failure.detail).map_err(|_| rejected())?)
        } else {
            None
        };
        let store_error = |error| {
            store_failure(
                RunnerInboundFrameKind::Resume,
                AvailableCorrelation::Release(correlation.clone()),
                error,
            )
        };
        let release = release_receipt(correlation).map_err(store_error)?;
        let state = self
            .store
            .workspace_release_state(identities.enrollment(), &release)
            .await
            .map_err(store_error)?
            .ok_or_else(rejected)?;
        let action = match (state, phase, detail.as_ref()) {
            (RunnerWorkspaceReleaseState::Pending, ReleasePhase::ReleaseAccepted, None) => {
                DirectiveAction::Await
            }
            (RunnerWorkspaceReleaseState::Pending, ReleasePhase::ReleaseCompleted, None)
            | (RunnerWorkspaceReleaseState::Pending, ReleasePhase::ReleaseAccepted, Some(_)) => {
                self.store
                    .reconcile_workspace_release_outcome(
                        identities.enrollment(),
                        &release,
                        detail.as_ref(),
                    )
                    .await
                    .map_err(store_error)?;
                DirectiveAction::DiscardAsRecorded
            }
            (RunnerWorkspaceReleaseState::Completed, ReleasePhase::ReleaseCompleted, None) => {
                DirectiveAction::DiscardAsRecorded
            }
            (
                RunnerWorkspaceReleaseState::CleanupFailed(prior),
                ReleasePhase::ReleaseAccepted,
                Some(detail),
            ) if prior == *detail => DirectiveAction::DiscardAsRecorded,
            (RunnerWorkspaceReleaseState::Unowned, _, _) => DirectiveAction::FailStale,
            _ => return Err(rejected()),
        };
        Ok(Directive {
            correlation: operation,
            action,
        })
    }
}

pub(super) fn operation_correlation(operation: &WorkspaceOperation) -> OperationCorrelation {
    match operation {
        WorkspaceOperation::Provision { correlation, .. } => {
            OperationCorrelation::Provision(correlation.clone())
        }
        WorkspaceOperation::Release { correlation, .. } => {
            OperationCorrelation::Release(correlation.clone())
        }
    }
}

pub(super) fn settle_workspace(
    pending: &mut Option<OperationCorrelation>,
    correlation: OperationCorrelation,
) {
    if pending.as_ref() == Some(&correlation) {
        *pending = None;
    }
}
