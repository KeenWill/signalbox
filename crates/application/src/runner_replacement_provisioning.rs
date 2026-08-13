//! Durable preparation boundary for a pinned runner replacement workspace.

use std::future::Future;

use signalbox_domain::{
    CredentialProfileName, DurableCommandId, ReplaceLostRunner, RunnerEnrollmentId,
    RunnerGeneration, RunnerId, RunnerReplacementProvisioningRejection, RunnerReplacementTarget,
    RunnerSandboxProfile, SessionId, WorkspaceProvisioningAuthorization,
    WorkspaceProvisioningAuthorizationId, WorkspaceRepositoryKey,
};

use crate::InvalidDurableCommandId;

/// Complete admitted request to prepare one lost-placement replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunnerReplacementProvisioningRequest {
    command: DurableCommandId,
    session: SessionId,
    expected_placement_revision: RunnerGeneration,
    replacement: RunnerReplacementTarget,
}

impl RunnerReplacementProvisioningRequest {
    /// Rejects reserved durable-command identities before transaction entry.
    pub fn try_new(
        command: DurableCommandId,
        session: SessionId,
        expected_placement_revision: RunnerGeneration,
        replacement: RunnerReplacementTarget,
    ) -> Result<Self, InvalidDurableCommandId> {
        if command.as_uuid().is_nil() {
            return Err(InvalidDurableCommandId::Nil);
        }
        if command.as_uuid().is_max() {
            return Err(InvalidDurableCommandId::Max);
        }
        Ok(Self {
            command,
            session,
            expected_placement_revision,
            replacement,
        })
    }

    /// Returns the user-global durable command identity.
    pub const fn command(&self) -> DurableCommandId {
        self.command
    }

    /// Returns the exact session whose lost placement is targeted.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the placement revision observed by the caller.
    pub const fn expected_placement_revision(&self) -> RunnerGeneration {
        self.expected_placement_revision
    }

    /// Returns the exact user-selected successor target.
    pub const fn replacement(&self) -> RunnerReplacementTarget {
        self.replacement
    }
}

/// Supplies fresh single-use workspace-provisioning identities.
pub trait RunnerReplacementProvisioningIdGenerator {
    fn next_authorization_id(&mut self) -> WorkspaceProvisioningAuthorizationId;
}

/// Production UUIDv7 generator for replacement provisioning identities.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7RunnerReplacementProvisioningIdGenerator;

impl RunnerReplacementProvisioningIdGenerator for UuidV7RunnerReplacementProvisioningIdGenerator {
    fn next_authorization_id(&mut self) -> WorkspaceProvisioningAuthorizationId {
        WorkspaceProvisioningAuthorizationId::from_uuid(uuid::Uuid::now_v7())
    }
}

/// Atomic durable preparation boundary for a pinned replacement workspace.
pub trait RunnerReplacementProvisioningTransaction {
    type Error;

    fn stage(
        &mut self,
        command: ReplaceLostRunner,
        authorization: WorkspaceProvisioningAuthorizationId,
    ) -> impl Future<Output = Result<RunnerReplacementProvisioningOutcome, Self::Error>> + Send;
}

/// Immutable durable facts for one retryable workspace-provisioning stage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerReplacementProvisioningStage {
    authorization: WorkspaceProvisioningAuthorizationId,
    session: SessionId,
    placement_revision: RunnerGeneration,
    enrollment: RunnerEnrollmentId,
    runner: RunnerId,
    registration_revision: RunnerGeneration,
    repository: WorkspaceRepositoryKey,
    sandbox: RunnerSandboxProfile,
    credential_profile: Option<CredentialProfileName>,
}

impl RunnerReplacementProvisioningStage {
    /// Converts freshly checked domain authority into its durable stage receipt.
    pub fn from_authorization(authorization: &WorkspaceProvisioningAuthorization) -> Self {
        Self {
            authorization: authorization.authorization(),
            session: authorization.session(),
            placement_revision: authorization.placement_revision(),
            enrollment: authorization.enrollment(),
            runner: authorization.runner(),
            registration_revision: authorization.registration_revision(),
            repository: authorization.repository().clone(),
            sandbox: authorization.sandbox(),
            credential_profile: authorization.credential_profile().cloned(),
        }
    }

    /// Reconstitutes already-authenticated durable stage facts.
    #[allow(clippy::too_many_arguments)]
    pub fn from_stored(
        authorization: WorkspaceProvisioningAuthorizationId,
        session: SessionId,
        placement_revision: RunnerGeneration,
        enrollment: RunnerEnrollmentId,
        runner: RunnerId,
        registration_revision: RunnerGeneration,
        repository: WorkspaceRepositoryKey,
        sandbox: RunnerSandboxProfile,
        credential_profile: Option<CredentialProfileName>,
    ) -> Self {
        Self {
            authorization,
            session,
            placement_revision,
            enrollment,
            runner,
            registration_revision,
            repository,
            sandbox,
            credential_profile,
        }
    }

    /// Returns the single-use workspace-provisioning identity.
    pub const fn authorization(&self) -> WorkspaceProvisioningAuthorizationId {
        self.authorization
    }

    /// Returns the session whose successor workspace is being prepared.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the successor placement revision reserved by this stage.
    pub const fn placement_revision(&self) -> RunnerGeneration {
        self.placement_revision
    }

    /// Returns the selected successor enrollment.
    pub const fn enrollment(&self) -> RunnerEnrollmentId {
        self.enrollment
    }

    /// Returns the selected successor runner.
    pub const fn runner(&self) -> RunnerId {
        self.runner
    }

    /// Returns the checked registration revision for the successor.
    pub const fn registration_revision(&self) -> RunnerGeneration {
        self.registration_revision
    }

    /// Returns the repository the runner is authorized to provision.
    pub const fn repository(&self) -> &WorkspaceRepositoryKey {
        &self.repository
    }

    /// Returns the exact sandbox profile retained by the lost placement.
    pub const fn sandbox(&self) -> RunnerSandboxProfile {
        self.sandbox
    }

    /// Returns the optional credential profile retained by the lost placement.
    pub const fn credential_profile(&self) -> Option<&CredentialProfileName> {
        self.credential_profile.as_ref()
    }
}

/// Durable stage, terminal refusal, inapplicable placement, or command conflict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunnerReplacementProvisioningOutcome {
    /// The exact command owns this retryable provisioning authorization.
    Staged(RunnerReplacementProvisioningStage),
    /// The exact command owns this terminal durable refusal.
    Rejected(RunnerReplacementProvisioningRejection),
    /// The current placement requires no repository-provisioning stage.
    NotApplicable,
    /// The user-global command identity names different intent.
    ConflictingReuse { command: DurableCommandId },
}

/// Coordinates one canonical pinned-replacement provisioning stage.
#[derive(Debug)]
pub struct RunnerReplacementProvisioningService<Transaction, Ids> {
    transaction: Transaction,
    ids: Ids,
}

impl<Transaction, Ids> RunnerReplacementProvisioningService<Transaction, Ids> {
    pub const fn new(transaction: Transaction, ids: Ids) -> Self {
        Self { transaction, ids }
    }
}

impl<Transaction, Ids> RunnerReplacementProvisioningService<Transaction, Ids>
where
    Transaction: RunnerReplacementProvisioningTransaction,
    Ids: RunnerReplacementProvisioningIdGenerator,
{
    pub async fn execute(
        &mut self,
        request: RunnerReplacementProvisioningRequest,
    ) -> Result<RunnerReplacementProvisioningOutcome, Transaction::Error> {
        let authorization = self.ids.next_authorization_id();
        self.transaction
            .stage(
                ReplaceLostRunner::new(
                    request.command,
                    request.session,
                    request.expected_placement_revision,
                    request.replacement,
                ),
                authorization,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, convert::Infallible, future::ready};

    use signalbox_domain::{
        DurableCommandId, ReplaceLostRunner, RunnerGeneration, RunnerId, RunnerReplacementTarget,
        SessionId, WorkspaceProvisioningAuthorizationId,
    };
    use uuid::{Uuid, Variant, Version};

    use super::{
        RunnerReplacementProvisioningIdGenerator, RunnerReplacementProvisioningOutcome,
        RunnerReplacementProvisioningRequest, RunnerReplacementProvisioningService,
        RunnerReplacementProvisioningTransaction, UuidV7RunnerReplacementProvisioningIdGenerator,
    };
    use crate::InvalidDurableCommandId;

    const COMMAND: u128 = 1;
    const SESSION: u128 = 2;
    const RUNNER: u128 = 3;
    const AUTHORIZATION: u128 = 4;

    #[derive(Debug)]
    struct ScriptedIds {
        authorizations: VecDeque<WorkspaceProvisioningAuthorizationId>,
    }

    impl RunnerReplacementProvisioningIdGenerator for ScriptedIds {
        fn next_authorization_id(&mut self) -> WorkspaceProvisioningAuthorizationId {
            self.authorizations
                .pop_front()
                .expect("the service requests exactly one scripted authorization")
        }
    }

    #[derive(Debug)]
    struct RecordingTransaction {
        expected_command: ReplaceLostRunner,
        expected_authorization: WorkspaceProvisioningAuthorizationId,
    }

    impl RunnerReplacementProvisioningTransaction for RecordingTransaction {
        type Error = Infallible;

        fn stage(
            &mut self,
            command: ReplaceLostRunner,
            authorization: WorkspaceProvisioningAuthorizationId,
        ) -> impl Future<Output = Result<RunnerReplacementProvisioningOutcome, Self::Error>> + Send
        {
            assert_eq!(command, self.expected_command);
            assert_eq!(authorization, self.expected_authorization);
            ready(Ok(RunnerReplacementProvisioningOutcome::NotApplicable))
        }
    }

    /// INV-001: reserved durable-command identities fail before the
    /// replacement-provisioning transaction can observe a request.
    #[test]
    fn inv001_provisioning_request_rejects_reserved_command_identifiers() {
        let session = SessionId::from_uuid(Uuid::from_u128(SESSION));
        let revision = RunnerGeneration::one();
        let replacement =
            RunnerReplacementTarget::Runner(RunnerId::from_uuid(Uuid::from_u128(RUNNER)));

        assert_eq!(
            RunnerReplacementProvisioningRequest::try_new(
                DurableCommandId::from_uuid(Uuid::nil()),
                session,
                revision,
                replacement,
            ),
            Err(InvalidDurableCommandId::Nil)
        );
        assert_eq!(
            RunnerReplacementProvisioningRequest::try_new(
                DurableCommandId::from_uuid(Uuid::max()),
                session,
                revision,
                replacement,
            ),
            Err(InvalidDurableCommandId::Max)
        );
    }

    /// INV-001: production provisioning identities are fresh RFC-9562 UUIDv7
    /// values and do not derive from the replacement command.
    #[test]
    fn inv001_production_generator_supplies_fresh_uuid_v7_authorizations() {
        let mut generator = UuidV7RunnerReplacementProvisioningIdGenerator;
        let first = generator.next_authorization_id();
        let second = generator.next_authorization_id();

        assert_ne!(first, second);
        assert_eq!(first.as_uuid().get_variant(), Variant::RFC4122);
        assert_eq!(first.as_uuid().get_version(), Some(Version::SortRand));
        assert_eq!(second.as_uuid().get_variant(), Variant::RFC4122);
        assert_eq!(second.as_uuid().get_version(), Some(Version::SortRand));
    }

    #[tokio::test]
    async fn service_supplies_the_exact_command_and_one_fresh_authorization() {
        let command = DurableCommandId::from_uuid(Uuid::from_u128(COMMAND));
        let session = SessionId::from_uuid(Uuid::from_u128(SESSION));
        let revision = RunnerGeneration::one();
        let replacement =
            RunnerReplacementTarget::Runner(RunnerId::from_uuid(Uuid::from_u128(RUNNER)));
        let expected_command = ReplaceLostRunner::new(command, session, revision, replacement);
        let expected_authorization =
            WorkspaceProvisioningAuthorizationId::from_uuid(Uuid::from_u128(AUTHORIZATION));
        let transaction = RecordingTransaction {
            expected_command,
            expected_authorization,
        };
        let request =
            RunnerReplacementProvisioningRequest::try_new(command, session, revision, replacement)
                .expect("the fixture command identity is admitted");
        let ids = ScriptedIds {
            authorizations: VecDeque::from([expected_authorization]),
        };
        let mut service = RunnerReplacementProvisioningService::new(transaction, ids);

        let outcome = service
            .execute(request)
            .await
            .expect("the infallible transaction returns");

        assert_eq!(outcome, RunnerReplacementProvisioningOutcome::NotApplicable);
    }
}
