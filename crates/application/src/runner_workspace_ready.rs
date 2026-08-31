//! Atomic durable admission of one replacement workspace-ready receipt.

use std::{error::Error, fmt, future::Future};

use signalbox_domain::{
    CanonicalCloneUrlDigest, CredentialProfileName, RunnerGeneration, RunnerId,
    RunnerSandboxProfile, RunnerWorkingDirectory, SessionId, WorkspaceManifestId,
    WorkspaceProvisioningAuthorizationId, WorkspaceRecovery, WorkspaceRelativePath,
    WorkspaceRepositoryKey,
};

/// One canonical lowercase SHA-256 digest of a ready workspace manifest.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunnerReadyManifestDigest(String);

impl RunnerReadyManifestDigest {
    /// Checks the complete lowercase digest text.
    pub fn try_new(value: String) -> Result<Self, InvalidRunnerReadyManifestDigest> {
        if value.len() == 64
            && value
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            Ok(Self(value))
        } else {
            Err(InvalidRunnerReadyManifestDigest)
        }
    }

    /// Returns the canonical hexadecimal text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A ready-manifest digest violated the canonical lowercase SHA-256 shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidRunnerReadyManifestDigest;

impl fmt::Display for InvalidRunnerReadyManifestDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .write_str("ready workspace manifest digest must be 64 lowercase hexadecimal bytes")
    }
}

impl Error for InvalidRunnerReadyManifestDigest {}

/// Complete validated receipt supplied by one replacement `workspace_ready` frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerWorkspaceReadyReceipt {
    authorization: WorkspaceProvisioningAuthorizationId,
    session: SessionId,
    placement_revision: RunnerGeneration,
    runner: RunnerId,
    manifest: WorkspaceManifestId,
    manifest_digest: RunnerReadyManifestDigest,
    repository: WorkspaceRepositoryKey,
    canonical_clone_url_digest: CanonicalCloneUrlDigest,
    credential_profile: Option<CredentialProfileName>,
    sandbox: RunnerSandboxProfile,
    relative_path: WorkspaceRelativePath,
    execution_directory: RunnerWorkingDirectory,
    recovery: WorkspaceRecovery,
}

/// A ready receipt did not carry an absolute runner-authored execution directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidRunnerWorkspaceExecutionDirectory;

impl fmt::Display for InvalidRunnerWorkspaceExecutionDirectory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ready workspace execution directory must be absolute")
    }
}

impl Error for InvalidRunnerWorkspaceExecutionDirectory {}

impl RunnerWorkspaceReadyReceipt {
    /// Retains the exact checked wire receipt without deriving execution-directory facts.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        authorization: WorkspaceProvisioningAuthorizationId,
        session: SessionId,
        placement_revision: RunnerGeneration,
        runner: RunnerId,
        manifest: WorkspaceManifestId,
        manifest_digest: RunnerReadyManifestDigest,
        repository: WorkspaceRepositoryKey,
        canonical_clone_url_digest: CanonicalCloneUrlDigest,
        credential_profile: Option<CredentialProfileName>,
        sandbox: RunnerSandboxProfile,
        relative_path: WorkspaceRelativePath,
        execution_directory: RunnerWorkingDirectory,
        recovery: WorkspaceRecovery,
    ) -> Result<Self, InvalidRunnerWorkspaceExecutionDirectory> {
        if RunnerWorkingDirectory::try_new_absolute(execution_directory.as_str().to_owned())
            .is_err()
        {
            return Err(InvalidRunnerWorkspaceExecutionDirectory);
        }
        Ok(Self {
            authorization,
            session,
            placement_revision,
            runner,
            manifest,
            manifest_digest,
            repository,
            canonical_clone_url_digest,
            credential_profile,
            sandbox,
            relative_path,
            execution_directory,
            recovery,
        })
    }

    /// Returns the single-use provisioning authorization.
    pub const fn authorization(&self) -> WorkspaceProvisioningAuthorizationId {
        self.authorization
    }

    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the successor placement revision.
    pub const fn placement_revision(&self) -> RunnerGeneration {
        self.placement_revision
    }

    /// Returns the runner that provisioned the workspace.
    pub const fn runner(&self) -> RunnerId {
        self.runner
    }

    /// Returns the stable workspace-manifest identity.
    pub const fn manifest_id(&self) -> WorkspaceManifestId {
        self.manifest
    }

    /// Returns the exact ready-manifest digest.
    pub const fn manifest_digest(&self) -> &RunnerReadyManifestDigest {
        &self.manifest_digest
    }

    /// Returns the authorized repository key.
    pub const fn repository(&self) -> &WorkspaceRepositoryKey {
        &self.repository
    }

    /// Returns the canonical clone-URL digest reported by the runner.
    pub const fn canonical_clone_url_digest(&self) -> &CanonicalCloneUrlDigest {
        &self.canonical_clone_url_digest
    }

    /// Returns the exact optional credential profile used for the clone.
    pub const fn credential_profile(&self) -> Option<&CredentialProfileName> {
        self.credential_profile.as_ref()
    }

    /// Returns the sandbox profile bound by the authorization.
    pub const fn sandbox(&self) -> RunnerSandboxProfile {
        self.sandbox
    }

    /// Returns the runner-root-relative manifest path.
    pub const fn relative_path(&self) -> &WorkspaceRelativePath {
        &self.relative_path
    }

    /// Returns the absolute execution directory stated by the runner.
    pub const fn execution_directory(&self) -> &RunnerWorkingDirectory {
        &self.execution_directory
    }

    /// Returns the exact repository recovery facts.
    pub const fn recovery(&self) -> &WorkspaceRecovery {
        &self.recovery
    }
}

/// Atomic durable receipt boundary that precedes `workspace_recorded` delivery.
pub trait RunnerWorkspaceReadyTransaction {
    /// Adapter-specific transaction failure.
    type Error;

    /// Commits or exactly replays one authenticated ready-workspace receipt.
    fn record(
        &mut self,
        receipt: RunnerWorkspaceReadyReceipt,
    ) -> impl Future<Output = Result<RunnerWorkspaceReadyReceipt, Self::Error>> + Send;
}

/// Coordinates one exact replacement workspace-ready admission.
#[derive(Debug)]
pub struct RunnerWorkspaceReadyService<Transaction> {
    transaction: Transaction,
}

impl<Transaction> RunnerWorkspaceReadyService<Transaction> {
    /// Uses the supplied durable receipt boundary.
    pub const fn new(transaction: Transaction) -> Self {
        Self { transaction }
    }
}

impl<Transaction> RunnerWorkspaceReadyService<Transaction>
where
    Transaction: RunnerWorkspaceReadyTransaction,
{
    /// Commits the exact receipt before any acknowledgement is emitted.
    pub async fn execute(
        &mut self,
        receipt: RunnerWorkspaceReadyReceipt,
    ) -> Result<RunnerWorkspaceReadyReceipt, Transaction::Error> {
        self.transaction.record(receipt).await
    }
}

#[cfg(test)]
mod tests {
    use std::future::ready;

    use signalbox_domain::{
        CanonicalCloneUrlDigest, CredentialProfileName, RunnerGeneration, RunnerId,
        RunnerSandboxProfile, RunnerWorkingDirectory, SessionId, WorkspaceManifestId,
        WorkspaceProvisioningAuthorizationId, WorkspaceRecovery, WorkspaceRelativePath,
        WorkspaceRepositoryKey, WorkspaceRevision,
    };
    use uuid::Uuid;

    use super::{
        InvalidRunnerReadyManifestDigest, InvalidRunnerWorkspaceExecutionDirectory,
        RunnerReadyManifestDigest, RunnerWorkspaceReadyReceipt, RunnerWorkspaceReadyService,
        RunnerWorkspaceReadyTransaction,
    };

    const AUTHORIZATION: u128 = 1;
    const SESSION: u128 = 2;
    const RUNNER: u128 = 3;
    const MANIFEST: u128 = 4;

    struct ReceiptFixture {
        receipt: RunnerWorkspaceReadyReceipt,
        authorization: WorkspaceProvisioningAuthorizationId,
        session: SessionId,
        placement_revision: RunnerGeneration,
        runner: RunnerId,
        manifest: WorkspaceManifestId,
        manifest_digest: RunnerReadyManifestDigest,
        repository: WorkspaceRepositoryKey,
        clone_url_digest: CanonicalCloneUrlDigest,
        credential_profile: Option<CredentialProfileName>,
        sandbox: RunnerSandboxProfile,
        relative_path: WorkspaceRelativePath,
        execution_directory: RunnerWorkingDirectory,
        recovery: WorkspaceRecovery,
    }

    fn receipt_fixture() -> ReceiptFixture {
        let authorization =
            WorkspaceProvisioningAuthorizationId::from_uuid(Uuid::from_u128(AUTHORIZATION));
        let session = SessionId::from_uuid(Uuid::from_u128(SESSION));
        let placement_revision = RunnerGeneration::one();
        let runner = RunnerId::from_uuid(Uuid::from_u128(RUNNER));
        let manifest = WorkspaceManifestId::from_uuid(Uuid::from_u128(MANIFEST));
        let manifest_digest = RunnerReadyManifestDigest::try_new("a".repeat(64))
            .expect("the fixture ready-manifest digest is canonical");
        let repository = WorkspaceRepositoryKey::try_new("source".to_owned())
            .expect("the fixture repository key is portable");
        let clone_url_digest = CanonicalCloneUrlDigest::try_new("b".repeat(64))
            .expect("the fixture clone-URL digest is canonical");
        let credential_profile = None;
        let sandbox = RunnerSandboxProfile::WorkspaceRestricted;
        let relative_path = WorkspaceRelativePath::try_new(format!(
            "sessions/{}/{}/repo",
            session.as_uuid(),
            placement_revision.get()
        ))
        .expect("the fixture manifest path is relative");
        let execution_directory =
            RunnerWorkingDirectory::try_new("/runner/sessions/2/1/repo".to_owned())
                .expect("the fixture execution directory is valid");
        let recovery = WorkspaceRecovery::Commit {
            revision: WorkspaceRevision::try_new("c".repeat(40))
                .expect("the fixture revision is canonical"),
        };
        let receipt = RunnerWorkspaceReadyReceipt::try_new(
            authorization,
            session,
            placement_revision,
            runner,
            manifest,
            manifest_digest.clone(),
            repository.clone(),
            clone_url_digest.clone(),
            credential_profile.clone(),
            sandbox,
            relative_path.clone(),
            execution_directory.clone(),
            recovery.clone(),
        )
        .expect("the fixture execution directory is absolute");
        ReceiptFixture {
            receipt,
            authorization,
            session,
            placement_revision,
            runner,
            manifest,
            manifest_digest,
            repository,
            clone_url_digest,
            credential_profile,
            sandbox,
            relative_path,
            execution_directory,
            recovery,
        }
    }

    #[derive(Debug)]
    struct RecordingTransaction {
        expected: RunnerWorkspaceReadyReceipt,
    }

    impl RunnerWorkspaceReadyTransaction for RecordingTransaction {
        type Error = &'static str;

        fn record(
            &mut self,
            receipt: RunnerWorkspaceReadyReceipt,
        ) -> impl Future<Output = Result<RunnerWorkspaceReadyReceipt, Self::Error>> + Send {
            assert_eq!(receipt, self.expected);
            ready(Ok(receipt))
        }
    }

    #[test]
    fn ready_manifest_digest_rejects_noncanonical_text() {
        assert_eq!(
            RunnerReadyManifestDigest::try_new("A".repeat(64)),
            Err(InvalidRunnerReadyManifestDigest)
        );
    }

    #[test]
    fn receipt_retains_every_validated_manifest_fact() {
        let fixture = receipt_fixture();

        assert_eq!(fixture.receipt.authorization(), fixture.authorization);
        assert_eq!(fixture.receipt.session(), fixture.session);
        assert_eq!(
            fixture.receipt.placement_revision(),
            fixture.placement_revision
        );
        assert_eq!(fixture.receipt.runner(), fixture.runner);
        assert_eq!(fixture.receipt.manifest_id(), fixture.manifest);
        assert_eq!(fixture.receipt.manifest_digest(), &fixture.manifest_digest);
        assert_eq!(fixture.receipt.repository(), &fixture.repository);
        assert_eq!(
            fixture.receipt.canonical_clone_url_digest(),
            &fixture.clone_url_digest
        );
        assert_eq!(
            fixture.receipt.credential_profile(),
            fixture.credential_profile.as_ref()
        );
        assert_eq!(fixture.receipt.sandbox(), fixture.sandbox);
        assert_eq!(fixture.receipt.relative_path(), &fixture.relative_path);
        assert_eq!(
            fixture.receipt.execution_directory(),
            &fixture.execution_directory
        );
        assert_eq!(fixture.receipt.recovery(), &fixture.recovery);
    }

    #[test]
    fn receipt_rejects_a_relative_execution_directory() {
        let fixture = receipt_fixture();
        let relative = RunnerWorkingDirectory::try_new("sessions/2/1/repo".to_owned())
            .expect("the relative fixture directory is exact text");

        assert_eq!(
            RunnerWorkspaceReadyReceipt::try_new(
                fixture.authorization,
                fixture.session,
                fixture.placement_revision,
                fixture.runner,
                fixture.manifest,
                fixture.manifest_digest,
                fixture.repository,
                fixture.clone_url_digest,
                fixture.credential_profile,
                fixture.sandbox,
                fixture.relative_path,
                relative,
                fixture.recovery,
            ),
            Err(InvalidRunnerWorkspaceExecutionDirectory)
        );
    }

    #[tokio::test]
    async fn service_passes_the_exact_receipt_to_one_transaction() {
        let expected = receipt_fixture().receipt;
        let transaction = RecordingTransaction {
            expected: expected.clone(),
        };
        let mut service = RunnerWorkspaceReadyService::new(transaction);

        assert_eq!(
            service
                .execute(expected.clone())
                .await
                .expect("the recording transaction accepts the receipt"),
            expected
        );
    }
}
