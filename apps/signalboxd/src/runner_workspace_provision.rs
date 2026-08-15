//! Durable repository-workspace authorization delivery to one runner connection.

use std::{error::Error, fmt};

use signalbox_domain::{
    CredentialProfileName, RunnerEnrollmentId, RunnerGeneration, RunnerId, RunnerSandboxProfile,
    SessionId, WorkspaceProvisioningAuthorizationId, WorkspaceRepositoryKey,
};
use signalbox_persistence::runner_protocol::{
    RunnerConnectionEpoch, RunnerProtocolStore, RunnerProtocolStoreError,
    StoredWorkspaceProvisioningAuthorization,
};
use signalbox_runner_wire::{
    CanonicalUuid, Message, PositiveU64, ProfileName, ProvisionCorrelation, RepositoryKey,
    SandboxProfile, WorkspaceProvision,
};

use crate::{RunnerConnectionAddress, RunnerConnectionBroker, RunnerConnectionBrokerError};

/// Failure while reconstructing or routing one durable provisioning operation.
#[derive(Debug)]
pub enum PostgresRunnerWorkspaceProvisionError {
    /// Durable authorization loading failed.
    Store(RunnerProtocolStoreError),
    /// The requested authorization does not exist.
    AuthorizationNotFound,
    /// Authenticated domain facts could not construct the closed wire frame.
    InvalidWire,
    /// The exact physical connection could not accept the operation.
    Broker(RunnerConnectionBrokerError),
}

impl fmt::Display for PostgresRunnerWorkspaceProvisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Store(_) => "runner workspace provisioning authority is unavailable",
            Self::AuthorizationNotFound => "runner workspace provisioning authority is absent",
            Self::InvalidWire => "runner workspace provisioning authority is invalid",
            Self::Broker(_) => "runner workspace provisioning connection is unavailable",
        })
    }
}

impl Error for PostgresRunnerWorkspaceProvisionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Broker(error) => Some(error),
            Self::AuthorizationNotFound | Self::InvalidWire => None,
        }
    }
}

/// Exact durable authorization reconstructed for delivery.
#[derive(Clone, Debug)]
pub struct RunnerWorkspaceProvisionDispatch {
    address: RunnerConnectionAddress,
    message: Message,
}

impl RunnerWorkspaceProvisionDispatch {
    /// Returns the exact physical connection selected by durable authority.
    pub const fn address(&self) -> RunnerConnectionAddress {
        self.address
    }

    /// Borrows the complete closed operation frame.
    pub const fn message(&self) -> &Message {
        &self.message
    }
}

/// PostgreSQL-backed delivery of one already-staged repository workspace.
#[derive(Clone, Debug)]
pub struct PostgresRunnerWorkspaceProvisioner {
    store: RunnerProtocolStore,
    broker: RunnerConnectionBroker,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProvisioningAuthority {
    authorization: WorkspaceProvisioningAuthorizationId,
    session: SessionId,
    placement_revision: RunnerGeneration,
    enrollment: RunnerEnrollmentId,
    runner: RunnerId,
    registration_revision: RunnerGeneration,
    connection_epoch: RunnerConnectionEpoch,
    repository: WorkspaceRepositoryKey,
    sandbox: RunnerSandboxProfile,
    credential_profile: Option<CredentialProfileName>,
}

impl From<&StoredWorkspaceProvisioningAuthorization> for ProvisioningAuthority {
    fn from(stored: &StoredWorkspaceProvisioningAuthorization) -> Self {
        Self {
            authorization: stored.authorization(),
            session: stored.session(),
            placement_revision: stored.successor_placement_revision(),
            enrollment: stored.enrollment(),
            runner: stored.runner(),
            registration_revision: stored.registration_revision(),
            connection_epoch: stored.connection_epoch(),
            repository: stored.repository().clone(),
            sandbox: stored.sandbox(),
            credential_profile: stored.credential_profile().cloned(),
        }
    }
}

impl PostgresRunnerWorkspaceProvisioner {
    /// Shares the durable runner store and established-connection broker.
    pub const fn new(store: RunnerProtocolStore, broker: RunnerConnectionBroker) -> Self {
        Self { store, broker }
    }

    /// Reloads and routes one exact durable provisioning authorization.
    pub async fn dispatch(
        &self,
        authorization: WorkspaceProvisioningAuthorizationId,
    ) -> Result<RunnerWorkspaceProvisionDispatch, PostgresRunnerWorkspaceProvisionError> {
        let stored = self
            .store
            .load_workspace_provisioning_authorization(authorization)
            .await
            .map_err(PostgresRunnerWorkspaceProvisionError::Store)?
            .ok_or(PostgresRunnerWorkspaceProvisionError::AuthorizationNotFound)?;
        dispatch_authority(&self.broker, &ProvisioningAuthority::from(&stored))
    }
}

fn dispatch_authority(
    broker: &RunnerConnectionBroker,
    authority: &ProvisioningAuthority,
) -> Result<RunnerWorkspaceProvisionDispatch, PostgresRunnerWorkspaceProvisionError> {
    let dispatch = dispatch_from_authority(authority)?;
    broker
        .send(dispatch.address, dispatch.message.clone())
        .map_err(PostgresRunnerWorkspaceProvisionError::Broker)?;
    Ok(dispatch)
}

fn dispatch_from_authority(
    authority: &ProvisioningAuthority,
) -> Result<RunnerWorkspaceProvisionDispatch, PostgresRunnerWorkspaceProvisionError> {
    let placement_revision = PositiveU64::try_new(authority.placement_revision.get())
        .map_err(|_| PostgresRunnerWorkspaceProvisionError::InvalidWire)?;
    let registration_revision = PositiveU64::try_new(authority.registration_revision.get())
        .map_err(|_| PostgresRunnerWorkspaceProvisionError::InvalidWire)?;
    let repository = RepositoryKey::try_new(authority.repository.as_str().to_owned())
        .map_err(|_| PostgresRunnerWorkspaceProvisionError::InvalidWire)?;
    let credential_profile = authority
        .credential_profile
        .as_ref()
        .map(|profile| ProfileName::try_new(profile.as_str().to_owned()))
        .transpose()
        .map_err(|_| PostgresRunnerWorkspaceProvisionError::InvalidWire)?;
    let correlation = ProvisionCorrelation {
        authorization_id: CanonicalUuid::from_uuid(authority.authorization.into_uuid()),
        session_id: CanonicalUuid::from_uuid(authority.session.into_uuid()),
        placement_revision,
        runner_id: CanonicalUuid::from_uuid(authority.runner.into_uuid()),
        registration_revision,
        repository: Some(repository),
        sandbox_profile: match authority.sandbox {
            signalbox_domain::RunnerSandboxProfile::Ambient => SandboxProfile::Ambient,
            signalbox_domain::RunnerSandboxProfile::WorkspaceRestricted => {
                SandboxProfile::WorkspaceRestricted
            }
        },
        credential_profile,
    };
    correlation
        .validate()
        .map_err(|_| PostgresRunnerWorkspaceProvisionError::InvalidWire)?;
    Ok(RunnerWorkspaceProvisionDispatch {
        address: RunnerConnectionAddress::new(
            authority.enrollment,
            authority.runner,
            authority.connection_epoch,
        ),
        message: Message::WorkspaceProvision(WorkspaceProvision { correlation }),
    })
}

#[cfg(test)]
mod tests {
    use signalbox_domain::{
        CredentialProfileName, RunnerEnrollmentId, RunnerGeneration, RunnerId,
        RunnerSandboxProfile, SessionId, WorkspaceProvisioningAuthorizationId,
        WorkspaceRepositoryKey,
    };
    use signalbox_persistence::runner_protocol::RunnerConnectionEpoch;
    use signalbox_runner_wire::{
        CanonicalUuid, Message, PositiveU64, ProfileName, ProvisionCorrelation, RepositoryKey,
        SandboxProfile, WorkspaceProvision,
    };
    use uuid::Uuid;

    use super::{ProvisioningAuthority, dispatch_authority, dispatch_from_authority};

    const AUTHORIZATION: u128 = 1;
    const SESSION: u128 = 2;
    const ENROLLMENT: u128 = 3;
    const RUNNER: u128 = 4;

    fn authority() -> ProvisioningAuthority {
        ProvisioningAuthority {
            authorization: WorkspaceProvisioningAuthorizationId::from_uuid(Uuid::from_u128(
                AUTHORIZATION,
            )),
            session: SessionId::from_uuid(Uuid::from_u128(SESSION)),
            placement_revision: RunnerGeneration::try_from_u64(7)
                .expect("the fixture placement revision is positive"),
            enrollment: RunnerEnrollmentId::from_uuid(Uuid::from_u128(ENROLLMENT)),
            runner: RunnerId::from_uuid(Uuid::from_u128(RUNNER)),
            registration_revision: RunnerGeneration::try_from_u64(5)
                .expect("the fixture registration revision is positive"),
            connection_epoch: RunnerConnectionEpoch::try_from_u64(3)
                .expect("the fixture connection epoch is positive"),
            repository: WorkspaceRepositoryKey::try_new("signalbox".to_owned())
                .expect("the fixture repository key is valid"),
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            credential_profile: Some(
                CredentialProfileName::try_new("github-runner".to_owned())
                    .expect("the fixture profile name is valid"),
            ),
        }
    }

    fn expected_address() -> crate::RunnerConnectionAddress {
        crate::RunnerConnectionAddress::new(
            RunnerEnrollmentId::from_uuid(Uuid::from_u128(ENROLLMENT)),
            RunnerId::from_uuid(Uuid::from_u128(RUNNER)),
            RunnerConnectionEpoch::try_from_u64(3)
                .expect("the fixture connection epoch is positive"),
        )
    }

    fn expected_message() -> Message {
        Message::WorkspaceProvision(WorkspaceProvision {
            correlation: ProvisionCorrelation {
                authorization_id: CanonicalUuid::from_uuid(Uuid::from_u128(AUTHORIZATION)),
                session_id: CanonicalUuid::from_uuid(Uuid::from_u128(SESSION)),
                placement_revision: PositiveU64::try_new(7)
                    .expect("the fixture placement revision is positive"),
                runner_id: CanonicalUuid::from_uuid(Uuid::from_u128(RUNNER)),
                registration_revision: PositiveU64::try_new(5)
                    .expect("the fixture registration revision is positive"),
                repository: Some(
                    RepositoryKey::try_new("signalbox".to_owned())
                        .expect("the fixture repository key is valid"),
                ),
                sandbox_profile: SandboxProfile::WorkspaceRestricted,
                credential_profile: Some(
                    ProfileName::try_new("github-runner".to_owned())
                        .expect("the fixture profile name is valid"),
                ),
            },
        })
    }

    #[test]
    fn durable_authority_constructs_the_exact_closed_provisioning_frame() {
        let authority = authority();
        let dispatch = dispatch_from_authority(&authority)
            .expect("the authenticated fixture constructs a wire frame");

        assert_eq!(dispatch.address(), expected_address());
        assert_eq!(dispatch.message(), &expected_message());
    }

    #[tokio::test]
    async fn durable_authority_routes_only_to_its_exact_physical_connection() {
        let authority = authority();
        let broker = crate::RunnerConnectionBroker::new();
        let address = expected_address();
        let mut attachment = broker
            .attach(address)
            .expect("the fixture connection owns its broker address");
        let dispatched = dispatch_authority(&broker, &authority)
            .expect("the attached exact connection accepts the operation");
        let received = attachment
            .receive()
            .await
            .expect("the attached connection receives one operation");

        assert_eq!(dispatched.address(), address);
        assert_eq!(&received, dispatched.message());
    }
}
