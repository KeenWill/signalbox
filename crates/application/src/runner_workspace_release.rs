//! Atomic durable admission of one completed runner workspace release.

use std::future::Future;

use signalbox_domain::{RunnerGeneration, RunnerId, SessionId, WorkspaceManifestId};

/// Complete correlation supplied by one `workspace_released` frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunnerWorkspaceReleaseAcknowledgement {
    session: SessionId,
    placement_revision: RunnerGeneration,
    runner: RunnerId,
    manifest: WorkspaceManifestId,
}

impl RunnerWorkspaceReleaseAcknowledgement {
    /// Retains the exact completed-release correlation.
    pub const fn new(
        session: SessionId,
        placement_revision: RunnerGeneration,
        runner: RunnerId,
        manifest: WorkspaceManifestId,
    ) -> Self {
        Self {
            session,
            placement_revision,
            runner,
            manifest,
        }
    }

    /// Returns the retired session.
    pub const fn session(self) -> SessionId {
        self.session
    }

    /// Returns the exact retired placement revision.
    pub const fn placement_revision(self) -> RunnerGeneration {
        self.placement_revision
    }

    /// Returns the cleanup-owning runner.
    pub const fn runner(self) -> RunnerId {
        self.runner
    }

    /// Returns the protected predecessor manifest identity.
    pub const fn manifest_id(self) -> WorkspaceManifestId {
        self.manifest
    }
}

/// Atomic durable boundary that precedes `workspace_release_recorded` delivery.
pub trait RunnerWorkspaceReleaseTransaction {
    /// Adapter-specific transaction failure.
    type Error;

    /// Commits or exactly replays one authenticated completed release.
    fn record_release(
        &mut self,
        acknowledgement: RunnerWorkspaceReleaseAcknowledgement,
    ) -> impl Future<Output = Result<RunnerWorkspaceReleaseAcknowledgement, Self::Error>> + Send;
}

/// Coordinates one exact completed workspace-release admission.
#[derive(Debug)]
pub struct RunnerWorkspaceReleaseService<Transaction> {
    transaction: Transaction,
}

impl<Transaction> RunnerWorkspaceReleaseService<Transaction> {
    /// Uses the supplied durable release boundary.
    pub const fn new(transaction: Transaction) -> Self {
        Self { transaction }
    }
}

impl<Transaction> RunnerWorkspaceReleaseService<Transaction>
where
    Transaction: RunnerWorkspaceReleaseTransaction,
{
    /// Commits the exact completed release before acknowledgement is emitted.
    pub async fn execute(
        &mut self,
        acknowledgement: RunnerWorkspaceReleaseAcknowledgement,
    ) -> Result<RunnerWorkspaceReleaseAcknowledgement, Transaction::Error> {
        self.transaction.record_release(acknowledgement).await
    }
}

#[cfg(test)]
mod tests {
    use std::future::ready;

    use signalbox_domain::{RunnerGeneration, RunnerId, SessionId, WorkspaceManifestId};
    use uuid::Uuid;

    use super::{
        RunnerWorkspaceReleaseAcknowledgement, RunnerWorkspaceReleaseService,
        RunnerWorkspaceReleaseTransaction,
    };

    const SESSION: u128 = 1;
    const RUNNER: u128 = 2;
    const MANIFEST: u128 = 3;

    struct AcknowledgementFixture {
        acknowledgement: RunnerWorkspaceReleaseAcknowledgement,
        session: SessionId,
        placement_revision: RunnerGeneration,
        runner: RunnerId,
        manifest: WorkspaceManifestId,
    }

    fn acknowledgement_fixture() -> AcknowledgementFixture {
        let session = SessionId::from_uuid(Uuid::from_u128(SESSION));
        let placement_revision = RunnerGeneration::one();
        let runner = RunnerId::from_uuid(Uuid::from_u128(RUNNER));
        let manifest = WorkspaceManifestId::from_uuid(Uuid::from_u128(MANIFEST));
        let acknowledgement = RunnerWorkspaceReleaseAcknowledgement::new(
            session,
            placement_revision,
            runner,
            manifest,
        );
        AcknowledgementFixture {
            acknowledgement,
            session,
            placement_revision,
            runner,
            manifest,
        }
    }

    struct RecordingTransaction {
        expected: RunnerWorkspaceReleaseAcknowledgement,
    }

    impl RunnerWorkspaceReleaseTransaction for RecordingTransaction {
        type Error = ();

        fn record_release(
            &mut self,
            acknowledgement: RunnerWorkspaceReleaseAcknowledgement,
        ) -> impl Future<Output = Result<RunnerWorkspaceReleaseAcknowledgement, Self::Error>> + Send
        {
            assert_eq!(acknowledgement, self.expected);
            ready(Ok(acknowledgement))
        }
    }

    #[test]
    fn acknowledgement_retains_the_complete_release_correlation() {
        let fixture = acknowledgement_fixture();

        assert_eq!(fixture.acknowledgement.session(), fixture.session);
        assert_eq!(
            fixture.acknowledgement.placement_revision(),
            fixture.placement_revision
        );
        assert_eq!(fixture.acknowledgement.runner(), fixture.runner);
        assert_eq!(fixture.acknowledgement.manifest_id(), fixture.manifest);
    }

    #[tokio::test]
    async fn service_passes_the_exact_release_to_one_transaction() {
        let expected = acknowledgement_fixture().acknowledgement;
        let transaction = RecordingTransaction { expected };
        let mut service = RunnerWorkspaceReleaseService::new(transaction);

        let recorded = service
            .execute(expected)
            .await
            .expect("the recording transaction succeeds");

        assert_eq!(recorded, expected);
    }
}
