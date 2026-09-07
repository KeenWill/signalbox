//! Runner wire representations and validation.

use crate::scalars::{
    CanonicalU64, CanonicalUuid, CanonicalValueError, deserialize_required_nullable,
};
use serde::{Deserialize, Deserializer, Serialize};
use signalbox_domain::{
    CredentialProfileName as DomainCredentialProfileName,
    RunnerCapabilityClass as DomainRunnerCapabilityClass,
    RunnerWorkingDirectory as DomainRunnerWorkingDirectory,
    WorkspaceRepositoryKey as DomainWorkspaceRepositoryKey,
};

/// Closed terminal refusal of a runner recovery command.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerRecoveryRejection {
    /// The session does not exist.
    SessionNotFound,
    /// The placement is not lost.
    PlacementNotLost,
    /// An active turn needs its ordinary control flow.
    ExistingControlRequired,
    /// The pending enrollment request does not identify a pending candidate.
    PendingRunnerNotFound,
    /// The predecessor is not lost or the candidate is not connected.
    RunnerUnavailable,
    /// Another replacement command owns the placement.
    ReplacementPending,
    /// The candidate cannot satisfy the placement.
    PlacementUnavailable,
    /// A checkout revision requires a repository placement.
    RevisionWithoutRepository,
    /// Workspace provisioning failed.
    ProvisioningFailed,
}

/// Replacement's durable terminal outcome.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerReplacementOutcome {
    /// The successor placement was installed.
    Replaced {
        /// Successor runner.
        runner_id: CanonicalUuid,
        /// Positive successor placement revision.
        placement_revision: crate::PositiveCanonicalU64,
    },
    /// No successor was installed.
    Rejected {
        /// Closed terminal refusal.
        reason: RunnerRecoveryRejection,
    },
}

/// Abandonment's durable terminal outcome.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerAbandonmentOutcome {
    /// The lost placement was retired.
    Abandoned,
    /// The placement was retained.
    Rejected {
        /// Closed terminal refusal.
        reason: RunnerRecoveryRejection,
    },
}

/// Promotion's durable terminal outcome.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerPromotionOutcome {
    /// The pending enrollment was activated.
    Promoted {
        /// Promoted runner identity.
        runner_id: CanonicalUuid,
    },
    /// Enrollment authority was retained.
    Rejected {
        /// Closed terminal refusal.
        reason: RunnerRecoveryRejection,
    },
}

/// Sandbox profile selected by one runner placement.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RunnerSandboxProfile {
    /// Supervised execution with the invoking user's ambient filesystem and network access.
    #[serde(rename = "ambient")]
    Ambient,
    /// Execution restricted to the placement-owned writable root.
    #[serde(rename = "workspace-restricted")]
    WorkspaceRestricted,
}

#[derive(signalbox_derive::Accessors)]
/// Checked runner capability-class name carried by a session projection.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunnerCapabilityClass(
    /// Borrows the validated capability-class name.
    #[get(str, as = "as_str")]
    String,
);

impl RunnerCapabilityClass {
    /// Applies the runner domain's portable catalog-name validation.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        DomainRunnerCapabilityClass::try_new(value.clone())
            .map(|_| Self(value))
            .map_err(|_| CanonicalValueError::RunnerCatalogName)
    }
}

impl TryFrom<String> for RunnerCapabilityClass {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<RunnerCapabilityClass> for String {
    fn from(value: RunnerCapabilityClass) -> Self {
        value.0
    }
}

#[derive(signalbox_derive::Accessors)]
/// Checked runner credential-profile name carried by a session projection.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunnerCredentialProfileName(
    /// Borrows the validated credential-profile name.
    #[get(str, as = "as_str")]
    String,
);

impl RunnerCredentialProfileName {
    /// Applies the runner domain's portable catalog-name validation.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        DomainCredentialProfileName::try_new(value.clone())
            .map(|_| Self(value))
            .map_err(|_| CanonicalValueError::RunnerCatalogName)
    }
}

impl TryFrom<String> for RunnerCredentialProfileName {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<RunnerCredentialProfileName> for String {
    fn from(value: RunnerCredentialProfileName) -> Self {
        value.0
    }
}

#[derive(signalbox_derive::Accessors)]
/// Checked runner repository key carried by a session projection.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunnerRepositoryKey(
    /// Borrows the validated repository key.
    #[get(str, as = "as_str")]
    String,
);

impl RunnerRepositoryKey {
    /// Applies the runner domain's portable repository-key validation.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        DomainWorkspaceRepositoryKey::try_new(value.clone())
            .map(|_| Self(value))
            .map_err(|_| CanonicalValueError::RunnerCatalogName)
    }
}

impl TryFrom<String> for RunnerRepositoryKey {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<RunnerRepositoryKey> for String {
    fn from(value: RunnerRepositoryKey) -> Self {
        value.0
    }
}

/// Complete selector carried by an authoritative runner projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerProjectionSelector {
    /// Selects one exact runner identity.
    Runner { runner_id: CanonicalUuid },
    /// Selects a runner advertising one exact capability class.
    CapabilityClass { name: RunnerCapabilityClass },
}

/// Closed current connection health carried for a pinned runner.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerConnectionHealth {
    /// The runner connection is currently healthy.
    Connected,
    /// The connection missed a heartbeat and remains within its recovery window.
    Suspect,
    /// The connection closed through an orderly daemon or runner shutdown.
    Shutdown,
    /// The connection reached a terminal loss transition.
    Lost,
}

/// Closed current state carried by an authoritative runner projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerProjectionState {
    /// No runner has been pinned yet.
    Unpinned,
    /// The current placement is pinned.
    Pinned,
    /// The exact selected runner was lost before pinning.
    RunnerLostBeforePin,
    /// The pinned runner was lost.
    RunnerLost,
    /// The lost placement was explicitly abandoned.
    RunnerAbandoned,
}

#[derive(signalbox_derive::Accessors)]
/// Authoritative current runner placement projected in a transcript snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawRunnerProjection")]
pub struct RunnerProjection {
    /// Borrows the immutable requested selector.
    #[get]
    /// Immutable selector requested by this placement revision.
    selector: RunnerProjectionSelector,
    /// Current or lost exact runner when the state names one.
    runner_id: Option<CanonicalUuid>,
    /// Positive current placement revision.
    placement_revision: RunnerPlacementRevision,
    /// Explicit sandbox profile selected by the placement.
    sandbox_profile: RunnerSandboxProfile,
    /// Independently nullable requested credential profile.
    credential_profile: Option<RunnerCredentialProfileName>,
    /// Independently nullable requested repository key.
    repository: Option<RunnerRepositoryKey>,
    /// Independently nullable exact requested working directory.
    working_directory: Option<RunnerWorkingDirectory>,
    /// Current connection health, present exactly while the placement is pinned.
    connection_health: Option<RunnerConnectionHealth>,
    /// Exact current placement state.
    state: RunnerProjectionState,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRunnerProjection {
    selector: RunnerProjectionSelector,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    runner_id: Option<CanonicalUuid>,
    placement_revision: RunnerPlacementRevision,
    sandbox_profile: RunnerSandboxProfile,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    credential_profile: Option<RunnerCredentialProfileName>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    repository: Option<RunnerRepositoryKey>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    working_directory: Option<RunnerWorkingDirectory>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    connection_health: Option<RunnerConnectionHealth>,
    state: RunnerProjectionState,
}

impl RunnerProjection {
    /// Constructs one complete internally coherent current placement projection.
    #[expect(
        clippy::too_many_arguments,
        reason = "the constructor names every independent session-composition axis"
    )]
    pub fn try_new(
        selector: RunnerProjectionSelector,
        runner_id: Option<CanonicalUuid>,
        placement_revision: RunnerPlacementRevision,
        sandbox_profile: RunnerSandboxProfile,
        credential_profile: Option<RunnerCredentialProfileName>,
        repository: Option<RunnerRepositoryKey>,
        working_directory: Option<RunnerWorkingDirectory>,
        connection_health: Option<RunnerConnectionHealth>,
        state: RunnerProjectionState,
    ) -> Result<Self, CanonicalValueError> {
        let runner_shape_valid =
            matches!(state, RunnerProjectionState::Unpinned) == runner_id.is_none();
        let selector_valid = match (&selector, runner_id, state) {
            (
                RunnerProjectionSelector::Runner {
                    runner_id: selected,
                },
                Some(current),
                _,
            ) => *selected == current,
            (RunnerProjectionSelector::Runner { .. }, None, RunnerProjectionState::Unpinned)
            | (
                RunnerProjectionSelector::CapabilityClass { .. },
                _,
                RunnerProjectionState::Unpinned
                | RunnerProjectionState::Pinned
                | RunnerProjectionState::RunnerLost
                | RunnerProjectionState::RunnerAbandoned,
            ) => true,
            (RunnerProjectionSelector::Runner { .. }, None, _)
            | (
                RunnerProjectionSelector::CapabilityClass { .. },
                _,
                RunnerProjectionState::RunnerLostBeforePin,
            ) => false,
        };
        let connection_shape_valid =
            matches!(state, RunnerProjectionState::Pinned) == connection_health.is_some();
        if !runner_shape_valid || !selector_valid || !connection_shape_valid {
            return Err(CanonicalValueError::RunnerProjection);
        }
        Ok(Self {
            selector,
            runner_id,
            placement_revision,
            sandbox_profile,
            credential_profile,
            repository,
            working_directory,
            connection_health,
            state,
        })
    }

    /// Returns the current or lost exact runner when the state names one.
    pub const fn runner_id(&self) -> Option<CanonicalUuid> {
        self.runner_id
    }

    /// Returns the positive current placement revision.
    pub const fn placement_revision(&self) -> RunnerPlacementRevision {
        self.placement_revision
    }

    /// Returns the explicitly selected sandbox profile.
    pub const fn sandbox_profile(&self) -> RunnerSandboxProfile {
        self.sandbox_profile
    }

    /// Borrows the independently nullable requested credential profile.
    pub const fn credential_profile(&self) -> Option<&RunnerCredentialProfileName> {
        self.credential_profile.as_ref()
    }

    /// Borrows the independently nullable requested repository key.
    pub const fn repository(&self) -> Option<&RunnerRepositoryKey> {
        self.repository.as_ref()
    }

    /// Borrows the independently nullable exact requested working directory.
    pub const fn working_directory(&self) -> Option<&RunnerWorkingDirectory> {
        self.working_directory.as_ref()
    }

    /// Returns current connection health exactly while the placement is pinned.
    pub const fn connection_health(&self) -> Option<RunnerConnectionHealth> {
        self.connection_health
    }

    /// Returns the exact current placement state.
    pub const fn state(&self) -> RunnerProjectionState {
        self.state
    }
}

impl TryFrom<RawRunnerProjection> for RunnerProjection {
    type Error = CanonicalValueError;

    fn try_from(raw: RawRunnerProjection) -> Result<Self, Self::Error> {
        Self::try_new(
            raw.selector,
            raw.runner_id,
            raw.placement_revision,
            raw.sandbox_profile,
            raw.credential_profile,
            raw.repository,
            raw.working_directory,
            raw.connection_health,
            raw.state,
        )
    }
}

#[derive(signalbox_derive::Accessors)]
/// Exact bounded runner working-directory text carried on the process wire.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunnerWorkingDirectory(
    /// Borrows the exact validated directory text.
    #[get(str, as = "as_str")]
    String,
);

impl RunnerWorkingDirectory {
    /// Maximum UTF-8 bytes admitted by the runner domain and process wire.
    pub const MAX_UTF8_BYTES: usize = DomainRunnerWorkingDirectory::MAX_BYTES;

    /// Admits nonempty, NUL-free text within the exact byte bound.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        DomainRunnerWorkingDirectory::try_new(value.clone())
            .map_err(|_| CanonicalValueError::RunnerWorkingDirectory)?;
        Ok(Self(value))
    }
}

impl TryFrom<String> for RunnerWorkingDirectory {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<RunnerWorkingDirectory> for String {
    fn from(value: RunnerWorkingDirectory) -> Self {
        value.0
    }
}

/// Positive runner placement revision carried by follower-visible wire facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RunnerPlacementRevision(CanonicalU64);

impl RunnerPlacementRevision {
    /// Admits one positive placement revision.
    pub const fn try_new(value: u64) -> Option<Self> {
        if value == 0 {
            None
        } else {
            Some(Self(CanonicalU64::new(value)))
        }
    }

    /// Returns the positive integer carried by this placement revision.
    pub const fn value(self) -> u64 {
        self.0.value()
    }
}

impl<'de> Deserialize<'de> for RunnerPlacementRevision {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let value = CanonicalU64::deserialize(deserializer)?;
        Self::try_new(value.value())
            .ok_or_else(|| serde::de::Error::custom("runner placement revision must be positive"))
    }
}

/// Closed runner state carried by one session update.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerStateTransitionState {
    /// Initial dispatch pinned the selected runner.
    Pinned,
    /// The current runner connection missed its first heartbeat.
    Suspect,
    /// A heartbeat acknowledgement recovered that same suspect connection.
    Connected,
    /// An exact runner selection was lost before initial pinning.
    RunnerLostBeforePin,
    /// A pinned runner became unavailable.
    RunnerLost,
    /// A checked successor runner replaced the prior placement.
    Replaced,
    /// Checked recovery retained the runner but changed the selected directory.
    WorkingDirectoryChanged,
    /// The user abandoned a lost runner placement.
    Abandoned,
}
