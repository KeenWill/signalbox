//! Owner-private durable enrollment state.

use std::{
    error::Error,
    fmt, fs,
    fs::File,
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _},
    path::Path,
};

use rustix::{
    fs::{AtFlags, FlockOperation, Mode, OFlags, flock, open, openat, renameat, unlinkat},
    process::geteuid,
};
use serde::{Deserialize, Serialize};
use signalbox_runner_wire::{CanonicalUuid, Digest, PositiveU64};
use uuid::Uuid;

use crate::{active_workspaces::ActiveWorkspaces, journal::Journal};

const STATE_DOCUMENT_VERSION: u64 = 1;
const ROOT_MODE: u32 = 0o700;
pub(crate) const STATE_MODE: u32 = 0o600;
pub(crate) const PERMISSION_MASK: u32 = 0o7777;
const STATE_FILE: &str = "enrollment-state.json";
pub(crate) const MAX_STATE_BYTES: u64 = 16 * 1024;

/// Authority carried by the daemon-issued enrollment receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentAuthority {
    /// Active registration authority.
    Active,
    /// Provisioning-only replacement-candidate authority.
    ReplacementPending,
}

/// Exact daemon-issued identities and current registration fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentReceipt {
    request_id: CanonicalUuid,
    enrollment_id: CanonicalUuid,
    runner_id: CanonicalUuid,
    authentication_id: CanonicalUuid,
    registration_revision: PositiveU64,
    advertisement_digest: Digest,
    authority: EnrollmentAuthority,
}

impl EnrollmentReceipt {
    /// Constructs one correlation-checked durable receipt value.
    pub fn new(
        request_id: CanonicalUuid,
        enrollment_id: CanonicalUuid,
        runner_id: CanonicalUuid,
        authentication_id: CanonicalUuid,
        registration_revision: PositiveU64,
        advertisement_digest: Digest,
        authority: EnrollmentAuthority,
    ) -> Self {
        Self {
            request_id,
            enrollment_id,
            runner_id,
            authentication_id,
            registration_revision,
            advertisement_digest,
            authority,
        }
    }

    /// Returns the stable runner-created request identity.
    pub const fn request_id(&self) -> CanonicalUuid {
        self.request_id
    }

    /// Returns the daemon-issued enrollment identity.
    pub const fn enrollment_id(&self) -> CanonicalUuid {
        self.enrollment_id
    }

    /// Returns the daemon-issued logical runner identity.
    pub const fn runner_id(&self) -> CanonicalUuid {
        self.runner_id
    }

    /// Returns the daemon-issued authentication-reference identity.
    pub const fn authentication_id(&self) -> CanonicalUuid {
        self.authentication_id
    }

    /// Returns the current durable registration revision.
    pub const fn registration_revision(&self) -> PositiveU64 {
        self.registration_revision
    }

    /// Borrows the digest of the advertisement accepted at this revision.
    pub const fn advertisement_digest(&self) -> &Digest {
        &self.advertisement_digest
    }

    /// Returns whether this receipt is active or provisioning-only.
    pub const fn authority(&self) -> EnrollmentAuthority {
        self.authority
    }

    pub(crate) fn with_registration(
        &self,
        registration_revision: PositiveU64,
        advertisement_digest: Digest,
    ) -> Self {
        Self {
            request_id: self.request_id,
            enrollment_id: self.enrollment_id,
            runner_id: self.runner_id,
            authentication_id: self.authentication_id,
            registration_revision,
            advertisement_digest,
            authority: self.authority,
        }
    }
}

/// Durable state available before one connection starts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerState {
    /// The stable request identity exists but no receipt has been accepted.
    Pristine {
        /// Runner-created enrollment idempotency identity.
        request_id: CanonicalUuid,
    },
    /// One exact daemon-issued receipt has been fsynced.
    Enrolled {
        /// Current durable enrollment and registration facts.
        receipt: EnrollmentReceipt,
    },
}

impl RunnerState {
    /// Returns the stable request identity in either lifecycle state.
    pub const fn request_id(&self) -> CanonicalUuid {
        match self {
            Self::Pristine { request_id } => *request_id,
            Self::Enrolled { receipt } => receipt.request_id(),
        }
    }

    /// Borrows the receipt after enrollment.
    pub const fn receipt(&self) -> Option<&EnrollmentReceipt> {
        match self {
            Self::Pristine { .. } => None,
            Self::Enrolled { receipt } => Some(receipt),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateDocument {
    version: u64,
    state: RunnerState,
}

/// Resource named by a durable-state I/O failure without exposing configured paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateResource {
    /// Owner-private root directory.
    Root,
    /// Directory that durably publishes a newly created state root.
    RootParent,
    /// Current durable state document.
    StateDocument,
    /// Single-use replacement document used for atomic publication.
    TemporaryDocument,
    /// Current private operation journal.
    Journal,
    /// Acknowledged active workspace receipts.
    ActiveWorkspaces,
    /// Single-use replacement journal used for atomic publication.
    TemporaryJournal,
    /// Atomic replacement of the active workspace receipts.
    TemporaryActiveWorkspaces,
}

impl fmt::Display for StateResource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Root => "runner state root",
            Self::RootParent => "runner state root parent",
            Self::StateDocument => "runner state document",
            Self::TemporaryDocument => "runner temporary state document",
            Self::Journal => "runner operation journal",
            Self::ActiveWorkspaces => "runner active workspace receipts",
            Self::TemporaryActiveWorkspaces => "runner temporary active workspace receipts",
            Self::TemporaryJournal => "runner temporary operation journal",
        })
    }
}

/// Exact durable-state operation that failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateOperation {
    Create,
    Inspect,
    ConfigurePermissions,
    Open,
    Lock,
    Read,
    Write,
    Sync,
    Rename,
}

impl fmt::Display for StateOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Create => "create",
            Self::Inspect => "inspect",
            Self::ConfigurePermissions => "configure permissions on",
            Self::Open => "open",
            Self::Lock => "lock",
            Self::Read => "read",
            Self::Write => "write",
            Self::Sync => "fsync",
            Self::Rename => "atomically publish",
        })
    }
}

/// Typed fail-closed error for documents in the durable runner state root.
#[derive(Debug)]
pub enum RunnerStateError {
    /// The configured root was not an absolute path with a final component.
    InvalidRootPath,
    /// The root was not a real owner-private directory.
    InvalidRootIdentity,
    /// Another runner process holds the root's process-lifetime lock.
    RootBusy,
    /// The durable document was absent only after a contradictory open result.
    StateDisappeared,
    /// The durable document was not a regular owner-only file.
    InvalidStateIdentity,
    /// The durable document exceeded its closed size bound.
    StateTooLarge,
    /// JSON shape, version, or typed field decoding failed.
    CorruptState,
    /// A returned receipt did not name the journaled request.
    RequestMismatch,
    /// A lifecycle update was attempted in the wrong state.
    InvalidTransition,
    /// Atomic rename completed, but durability of the published state is unknown.
    CommitAmbiguous {
        /// Directory-fsync failure after the state-document rename.
        source: io::Error,
    },
    /// One exact resource operation failed and retains its source error.
    Io {
        operation: StateOperation,
        resource: StateResource,
        source: io::Error,
    },
}

impl fmt::Display for RunnerStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRootPath => formatter.write_str("runner state root path is invalid"),
            Self::InvalidRootIdentity => {
                formatter.write_str("runner state root identity or permissions are invalid")
            }
            Self::RootBusy => formatter.write_str("runner state root is already locked"),
            Self::StateDisappeared => {
                formatter.write_str("runner state document disappeared during inspection")
            }
            Self::InvalidStateIdentity => {
                formatter.write_str("runner state document identity or permissions are invalid")
            }
            Self::StateTooLarge => formatter.write_str("runner state document exceeds its bound"),
            Self::CorruptState => formatter.write_str("runner state document is corrupt"),
            Self::RequestMismatch => {
                formatter.write_str("enrollment receipt request correlation is invalid")
            }
            Self::InvalidTransition => {
                formatter.write_str("runner durable-state transition is invalid")
            }
            Self::CommitAmbiguous { .. } => {
                formatter.write_str("runner durable-state commit outcome is ambiguous")
            }
            Self::Io {
                operation,
                resource,
                ..
            } => write!(formatter, "failed to {operation} {resource}"),
        }
    }
}

impl Error for RunnerStateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::CommitAmbiguous { source } => Some(source),
            Self::InvalidRootPath
            | Self::InvalidRootIdentity
            | Self::RootBusy
            | Self::StateDisappeared
            | Self::InvalidStateIdentity
            | Self::StateTooLarge
            | Self::CorruptState
            | Self::RequestMismatch
            | Self::InvalidTransition => None,
        }
    }
}

/// Locked owner-private state root and its current durable enrollment fact.
#[derive(Debug)]
pub struct RunnerStateRoot {
    directory: File,
    canonical_root: std::path::PathBuf,
    state: RunnerState,
    journal: Journal,
    active_workspaces: ActiveWorkspaces,
}

impl RunnerStateRoot {
    /// Opens or creates, validates, locks, and initializes one private root.
    pub fn open(path: &Path) -> Result<Self, RunnerStateError> {
        if !path.is_absolute() || path.file_name().is_none() {
            return Err(RunnerStateError::InvalidRootPath);
        }
        match fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                builder.mode(ROOT_MODE);
                builder
                    .create(path)
                    .map_err(|source| RunnerStateError::Io {
                        operation: StateOperation::Create,
                        resource: StateResource::Root,
                        source,
                    })?;
                fs::set_permissions(path, fs::Permissions::from_mode(ROOT_MODE)).map_err(
                    |source| RunnerStateError::Io {
                        operation: StateOperation::ConfigurePermissions,
                        resource: StateResource::Root,
                        source,
                    },
                )?;
                let parent = path.parent().ok_or(RunnerStateError::InvalidRootPath)?;
                let parent = File::from(
                    open(
                        parent,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(|error| RunnerStateError::Io {
                        operation: StateOperation::Open,
                        resource: StateResource::RootParent,
                        source: rustix_error(error),
                    })?,
                );
                parent.sync_all().map_err(|source| RunnerStateError::Io {
                    operation: StateOperation::Sync,
                    resource: StateResource::RootParent,
                    source,
                })?;
            }
            Err(source) => {
                return Err(RunnerStateError::Io {
                    operation: StateOperation::Inspect,
                    resource: StateResource::Root,
                    source,
                });
            }
        }

        let path_metadata = fs::symlink_metadata(path).map_err(|source| RunnerStateError::Io {
            operation: StateOperation::Inspect,
            resource: StateResource::Root,
            source,
        })?;
        let directory = File::from(
            open(
                path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| RunnerStateError::Io {
                operation: StateOperation::Open,
                resource: StateResource::Root,
                source: rustix_error(error),
            })?,
        );
        let descriptor_metadata = directory
            .metadata()
            .map_err(|source| RunnerStateError::Io {
                operation: StateOperation::Inspect,
                resource: StateResource::Root,
                source,
            })?;
        let effective_user = geteuid().as_raw();
        if !path_metadata.is_dir()
            || path_metadata.uid() != effective_user
            || path_metadata.mode() & PERMISSION_MASK != ROOT_MODE
            || !descriptor_metadata.is_dir()
            || descriptor_metadata.uid() != effective_user
            || descriptor_metadata.mode() & PERMISSION_MASK != ROOT_MODE
            || path_metadata.dev() != descriptor_metadata.dev()
            || path_metadata.ino() != descriptor_metadata.ino()
        {
            return Err(RunnerStateError::InvalidRootIdentity);
        }
        flock(&directory, FlockOperation::NonBlockingLockExclusive).map_err(|error| {
            if error == rustix::io::Errno::WOULDBLOCK {
                RunnerStateError::RootBusy
            } else {
                RunnerStateError::Io {
                    operation: StateOperation::Lock,
                    resource: StateResource::Root,
                    source: rustix_error(error),
                }
            }
        })?;

        let (state, journal, active_workspaces) = match openat(
            &directory,
            STATE_FILE,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(descriptor) => (
                read_state(File::from(descriptor), effective_user)?,
                Journal::open(&directory)?,
                ActiveWorkspaces::open(&directory)?,
            ),
            Err(rustix::io::Errno::NOENT) => {
                let journal = Journal::initialize(&directory)?;
                let active_workspaces = ActiveWorkspaces::initialize(&directory)?;
                let state = RunnerState::Pristine {
                    request_id: CanonicalUuid::from_uuid(Uuid::now_v7()),
                };
                write_state(&directory, &state)?;
                (state, journal, active_workspaces)
            }
            Err(error) => {
                return Err(RunnerStateError::Io {
                    operation: StateOperation::Open,
                    resource: StateResource::StateDocument,
                    source: rustix_error(error),
                });
            }
        };
        journal.validate_owner(&state)?;
        active_workspaces.validate_owner(&state)?;
        let canonical_root = fs::canonicalize(path).map_err(|source| RunnerStateError::Io {
            operation: StateOperation::Inspect,
            resource: StateResource::Root,
            source,
        })?;
        Ok(Self {
            directory,
            canonical_root,
            state,
            journal,
            active_workspaces,
        })
    }

    /// Borrows the exact current in-memory copy of fsynced state.
    pub const fn state(&self) -> &RunnerState {
        &self.state
    }

    pub(crate) fn workspace_store(
        &self,
    ) -> Result<crate::workspace::RunnerWorkspaceStore, RunnerStateError> {
        let directory = self
            .directory
            .try_clone()
            .map_err(|source| RunnerStateError::Io {
                operation: StateOperation::Open,
                resource: StateResource::Root,
                source,
            })?;
        Ok(crate::workspace::RunnerWorkspaceStore::from_root(
            directory,
            self.canonical_root.clone(),
        ))
    }

    /// Projects the replayed durable journal into the next resume inventory.
    pub fn reconnect_inventory(&self) -> signalbox_runner_wire::ReconnectInventory {
        self.journal.reconnect_inventory()
    }

    pub(crate) fn retained_release(
        &self,
    ) -> Option<(
        &signalbox_runner_wire::ReleaseCorrelation,
        signalbox_runner_wire::ReleasePhase,
    )> {
        self.journal.release()
    }
    pub(crate) fn accepted_release(&self) -> Option<crate::journal::AcceptedWorkspaceRelease> {
        self.journal.accepted_release()
    }
    pub(crate) fn record_release(
        &mut self,
        correlation: signalbox_runner_wire::ReleaseCorrelation,
    ) -> Result<(), RunnerStateError> {
        if !self
            .state
            .receipt()
            .is_some_and(|receipt| receipt.runner_id() == correlation.runner_id)
        {
            return Err(RunnerStateError::InvalidTransition);
        }
        self.journal.record_release(&self.directory, correlation)
    }
    pub(crate) fn complete_release(
        &mut self,
        correlation: &signalbox_runner_wire::ReleaseCorrelation,
    ) -> Result<(), RunnerStateError> {
        self.journal.complete_release(&self.directory, correlation)
    }
    pub(crate) fn acknowledge_release(
        &mut self,
        correlation: &signalbox_runner_wire::ReleaseCorrelation,
    ) -> Result<(), RunnerStateError> {
        if self.journal.release()
            != Some((
                correlation,
                signalbox_runner_wire::ReleasePhase::ReleaseCompleted,
            ))
        {
            return Err(RunnerStateError::InvalidTransition);
        }
        self.active_workspaces
            .remove(&self.directory, correlation.manifest_id)?;
        self.journal
            .acknowledge_release(&self.directory, correlation)
    }

    /// Authenticates acknowledged workspace identities before reconnecting after startup.
    pub fn authenticate_active_workspaces(
        &self,
        configuration: &crate::RunnerConfiguration,
    ) -> Result<(), crate::WorkspaceProvisionError> {
        use crate::WorkspaceProvisionError;
        let store = self
            .workspace_store()
            .map_err(|_| WorkspaceProvisionError::Storage)?;
        for active in self.active_workspaces.records.values() {
            if self.journal.release().is_some_and(|(release, _)| {
                release.manifest_id == active.ready.ready.manifest.manifest_id
            }) {
                continue;
            }
            let ready = &active.ready;
            let manifest = &ready.ready.manifest;
            if !configuration
                .advertisement()
                .sandbox_profiles
                .contains(&manifest.sandbox_profile)
            {
                return Err(WorkspaceProvisionError::ManifestConflict);
            }
            if let Some(key) = &manifest.repository {
                let repository = configuration
                    .repository(key)
                    .ok_or(WorkspaceProvisionError::ManifestConflict)?;
                if repository.credential_profile() != manifest.credential_profile.as_ref()
                    || manifest.canonical_clone_url_digest.as_ref()
                        != Some(&signalbox_runner_wire::clone_url_digest(
                            repository.clone_url(),
                        ))
                {
                    return Err(WorkspaceProvisionError::ManifestConflict);
                }
            }
            let observed = store
                .authenticate_active(ready)
                .map_err(|_| WorkspaceProvisionError::ManifestConflict)?;
            if observed != active.directory_identity {
                return Err(WorkspaceProvisionError::ManifestConflict);
            }
        }
        Ok(())
    }

    pub(crate) fn retained_provision(
        &self,
    ) -> Option<(
        &signalbox_runner_wire::WorkspaceProvision,
        Option<&signalbox_runner_wire::WorkspaceReady>,
    )> {
        self.journal.provision()
    }

    pub(crate) fn record_provision(
        &mut self,
        request: signalbox_runner_wire::WorkspaceProvision,
    ) -> Result<(), RunnerStateError> {
        let receipt = self
            .state
            .receipt()
            .ok_or(RunnerStateError::InvalidTransition)?;
        if receipt.runner_id() != request.correlation.runner_id
            || (receipt.registration_revision() != request.correlation.registration_revision
                && !self
                    .journal
                    .provision()
                    .is_some_and(|(prior, _)| prior == &request))
        {
            return Err(RunnerStateError::InvalidTransition);
        }
        self.journal.record_provision(&self.directory, request)
    }

    pub(crate) fn retained_provision_failure(
        &self,
    ) -> Option<&signalbox_runner_wire::OperationFailure> {
        self.journal.provision_failure()
    }
    pub(crate) fn record_provision_failure(
        &mut self,
        failure: signalbox_runner_wire::OperationFailure,
    ) -> Result<(), RunnerStateError> {
        self.journal
            .record_provision_failure(&self.directory, failure)
    }
    pub(crate) fn acknowledge_provision_failure(
        &mut self,
        correlation: &signalbox_runner_wire::OperationCorrelation,
    ) -> Result<(), RunnerStateError> {
        self.journal
            .acknowledge_provision_failure(&self.directory, correlation)
    }

    pub(crate) fn record_workspace_ready(
        &mut self,
        ready: signalbox_runner_wire::WorkspaceReady,
    ) -> Result<(), RunnerStateError> {
        self.journal.record_workspace_ready(&self.directory, ready)
    }

    pub(crate) fn acknowledge_workspace(
        &mut self,
        recorded: &signalbox_runner_wire::WorkspaceRecorded,
    ) -> Result<(), RunnerStateError> {
        let ready = self
            .journal
            .provision()
            .and_then(|(_, ready)| ready)
            .ok_or(RunnerStateError::InvalidTransition)?;
        let identity = self
            .workspace_store()?
            .authenticate_active(ready)
            .map_err(|_| RunnerStateError::InvalidTransition)?;
        if ready.correlation != recorded.correlation
            || ready.ready.manifest.manifest_id != recorded.manifest_id
            || ready.ready.manifest_digest != recorded.manifest_digest
        {
            return Err(RunnerStateError::InvalidTransition);
        }
        self.active_workspaces
            .record(&self.directory, ready.clone(), identity)?;
        self.journal
            .acknowledge_workspace(&self.directory, recorded)
    }

    /// Fsyncs the exact claimed lease phase before its named execution step.
    pub fn record_lease_phase(
        &mut self,
        phase: signalbox_runner_wire::LeasePhase,
    ) -> Result<(), RunnerStateError> {
        match &self.state {
            RunnerState::Enrolled { receipt }
                if receipt.runner_id() == phase.correlation.runner_id
                    && (receipt.registration_revision()
                        == phase.correlation.registration_revision
                        || self
                            .journal
                            .reconnect_inventory()
                            .lease
                            .as_ref()
                            .is_some_and(|retained| retained.correlation == phase.correlation))
                    && receipt.authority() == EnrollmentAuthority::Active => {}
            _ => return Err(RunnerStateError::InvalidTransition),
        }
        self.journal.record_phase(&self.directory, phase)
    }

    /// Fsyncs one exact terminal envelope before it can be sent to the daemon.
    pub fn record_terminal_result(
        &mut self,
        result: signalbox_runner_wire::RetainedResult,
    ) -> Result<(), RunnerStateError> {
        self.journal.record_result(&self.directory, result)
    }

    /// Discards retained execution evidence only after its exact acknowledgement.
    pub fn acknowledge_terminal_result(
        &mut self,
        correlation: &signalbox_runner_wire::LeaseCorrelation,
    ) -> Result<(), RunnerStateError> {
        self.journal
            .acknowledge_result(&self.directory, correlation)
    }

    pub(crate) fn discard_reconciled_lease(
        &mut self,
        correlation: &signalbox_runner_wire::LeaseCorrelation,
    ) -> Result<(), RunnerStateError> {
        self.journal.discard_lease(&self.directory, correlation)
    }

    /// Atomically fsyncs the first exact daemon-issued receipt.
    pub fn record_receipt(&mut self, receipt: EnrollmentReceipt) -> Result<(), RunnerStateError> {
        match &self.state {
            RunnerState::Pristine { request_id } if *request_id == receipt.request_id() => {}
            RunnerState::Pristine { .. } => return Err(RunnerStateError::RequestMismatch),
            RunnerState::Enrolled { receipt: current } if current == &receipt => return Ok(()),
            RunnerState::Enrolled { .. } => return Err(RunnerStateError::InvalidTransition),
        }
        let next = RunnerState::Enrolled { receipt };
        write_state(&self.directory, &next)?;
        self.state = next;
        Ok(())
    }

    /// Fsyncs an exact promotion of the retained candidate receipt.
    pub fn record_promotion(&mut self, receipt: EnrollmentReceipt) -> Result<(), RunnerStateError> {
        let current = self
            .state
            .receipt()
            .ok_or(RunnerStateError::InvalidTransition)?;
        let mut promoted = current.clone();
        promoted.authority = EnrollmentAuthority::Active;
        if promoted != receipt {
            return Err(RunnerStateError::InvalidTransition);
        }
        if current == &receipt {
            return Ok(());
        }
        let next = RunnerState::Enrolled { receipt };
        write_state(&self.directory, &next)?;
        self.state = next;
        Ok(())
    }

    /// Atomically advances the durable receipt after accepted re-registration.
    pub fn record_registration(
        &mut self,
        registration_revision: PositiveU64,
        advertisement_digest: Digest,
    ) -> Result<EnrollmentReceipt, RunnerStateError> {
        let receipt = self
            .state
            .receipt()
            .ok_or(RunnerStateError::InvalidTransition)?;
        if registration_revision < receipt.registration_revision() {
            return Err(RunnerStateError::InvalidTransition);
        }
        let receipt = receipt.with_registration(registration_revision, advertisement_digest);
        let next = RunnerState::Enrolled {
            receipt: receipt.clone(),
        };
        write_state(&self.directory, &next)?;
        self.state = next;
        Ok(receipt)
    }
}

fn read_state(mut file: File, effective_user: u32) -> Result<RunnerState, RunnerStateError> {
    let metadata = file.metadata().map_err(|source| RunnerStateError::Io {
        operation: StateOperation::Inspect,
        resource: StateResource::StateDocument,
        source,
    })?;
    if !metadata.is_file()
        || metadata.uid() != effective_user
        || metadata.mode() & PERMISSION_MASK != STATE_MODE
    {
        return Err(RunnerStateError::InvalidStateIdentity);
    }
    if metadata.len() > MAX_STATE_BYTES {
        return Err(RunnerStateError::StateTooLarge);
    }
    let mut content = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut content)
        .map_err(|source| RunnerStateError::Io {
            operation: StateOperation::Read,
            resource: StateResource::StateDocument,
            source,
        })?;
    if content.len() as u64 > MAX_STATE_BYTES {
        return Err(RunnerStateError::StateTooLarge);
    }
    let document: StateDocument =
        serde_json::from_slice(&content).map_err(|_| RunnerStateError::CorruptState)?;
    if document.version != STATE_DOCUMENT_VERSION {
        return Err(RunnerStateError::CorruptState);
    }
    Ok(document.state)
}

fn write_state(directory: &File, state: &RunnerState) -> Result<(), RunnerStateError> {
    let document = StateDocument {
        version: STATE_DOCUMENT_VERSION,
        state: state.clone(),
    };
    let mut encoded = serde_json::to_vec(&document).map_err(|_| RunnerStateError::CorruptState)?;
    encoded.push(b'\n');
    write_document(directory, DocumentKind::Enrollment, &encoded)
}

#[derive(Clone, Copy)]
pub(crate) enum DocumentKind {
    Enrollment,
    Journal,
    ActiveWorkspaces,
}

impl DocumentKind {
    pub(crate) const fn file_name(self) -> &'static str {
        match self {
            Self::Enrollment => STATE_FILE,
            Self::Journal => "operation-journal.json",
            Self::ActiveWorkspaces => "active-workspaces.json",
        }
    }

    const fn resource(self) -> StateResource {
        match self {
            Self::Enrollment => StateResource::StateDocument,
            Self::Journal => StateResource::Journal,
            Self::ActiveWorkspaces => StateResource::ActiveWorkspaces,
        }
    }

    const fn temporary_resource(self) -> StateResource {
        match self {
            Self::Enrollment => StateResource::TemporaryDocument,
            Self::Journal => StateResource::TemporaryJournal,
            Self::ActiveWorkspaces => StateResource::TemporaryActiveWorkspaces,
        }
    }
}

pub(crate) fn write_document(
    directory: &File,
    kind: DocumentKind,
    encoded: &[u8],
) -> Result<(), RunnerStateError> {
    let temporary_name = format!(".{}-{}.tmp", kind.file_name(), Uuid::now_v7());
    let descriptor = openat(
        directory,
        temporary_name.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|error| RunnerStateError::Io {
        operation: StateOperation::Create,
        resource: kind.temporary_resource(),
        source: rustix_error(error),
    })?;
    rustix::fs::fchmod(&descriptor, Mode::RUSR | Mode::WUSR).map_err(|error| {
        RunnerStateError::Io {
            operation: StateOperation::ConfigurePermissions,
            resource: kind.temporary_resource(),
            source: rustix_error(error),
        }
    })?;
    let mut temporary = File::from(descriptor);
    let prepared = (|| {
        temporary
            .write_all(encoded)
            .map_err(|source| RunnerStateError::Io {
                operation: StateOperation::Write,
                resource: kind.temporary_resource(),
                source,
            })?;
        temporary.sync_all().map_err(|source| RunnerStateError::Io {
            operation: StateOperation::Sync,
            resource: kind.temporary_resource(),
            source,
        })
    })();
    if let Err(error) = prepared {
        let _ = unlinkat(directory, temporary_name.as_str(), AtFlags::empty());
        return Err(error);
    }
    if let Err(error) = renameat(
        directory,
        temporary_name.as_str(),
        directory,
        kind.file_name(),
    ) {
        let _ = unlinkat(directory, temporary_name.as_str(), AtFlags::empty());
        return Err(RunnerStateError::Io {
            operation: StateOperation::Rename,
            resource: kind.resource(),
            source: rustix_error(error),
        });
    }
    directory
        .sync_all()
        .map_err(|source| RunnerStateError::CommitAmbiguous { source })
}

fn rustix_error(error: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}

#[cfg(test)]
mod tests {
    use signalbox_runner_wire::{Advertisement, advertisement_digest};
    use tempfile::TempDir;

    use super::*;

    /// Arbitrary daemon-issued enrollment identity used throughout state tests.
    const ARBITRARY_ENROLLMENT_UUID: u128 = 0x100;
    /// Arbitrary daemon-issued runner identity used throughout state tests.
    const ARBITRARY_RUNNER_UUID: u128 = 0x200;
    /// Arbitrary daemon-issued authentication-reference identity used throughout state tests.
    const ARBITRARY_AUTHENTICATION_UUID: u128 = 0x300;
    const INITIAL_REGISTRATION_REVISION: u64 = 1;

    fn root_path(parent: &TempDir) -> std::path::PathBuf {
        parent.path().join("runner-state")
    }

    /// Widens a rustix mode to the `u32` that `std` permissions are built from.
    ///
    /// rustix's `RawMode` is the host's native `mode_t`: `u16` on Apple targets
    /// and `u32` on Linux, where the cast is a no-op.
    #[allow(clippy::unnecessary_cast)]
    fn permission_bits(mode: Mode) -> u32 {
        mode.bits() as u32
    }

    fn empty_advertisement() -> Advertisement {
        Advertisement {
            default_working_directory: None,
            capability_classes: Vec::new(),
            tools: Vec::new(),
            workspace_capabilities: Vec::new(),
            sandbox_profiles: Vec::new(),
            credential_profiles: Vec::new(),
            repositories: Vec::new(),
        }
    }

    fn receipt(request_id: CanonicalUuid) -> EnrollmentReceipt {
        EnrollmentReceipt::new(
            request_id,
            CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_ENROLLMENT_UUID)),
            CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_RUNNER_UUID)),
            CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_AUTHENTICATION_UUID)),
            PositiveU64::try_new(INITIAL_REGISTRATION_REVISION)
                .expect("the fixture registration revision is positive"),
            advertisement_digest(&empty_advertisement())
                .expect("the explicit empty advertisement has a digest"),
            EnrollmentAuthority::Active,
        )
    }

    #[test]
    fn pristine_request_identity_survives_reopen() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let first = RunnerStateRoot::open(&path).expect("the private root opens");
        let request_id = first.state().request_id();
        drop(first);

        let reopened = RunnerStateRoot::open(&path).expect("the private root reopens");

        assert_eq!(reopened.state().request_id(), request_id);
    }

    #[test]
    fn precreated_empty_private_root_is_initialized() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        fs::create_dir(&path).expect("the provisioned root is created");
        fs::set_permissions(&path, fs::Permissions::from_mode(ROOT_MODE))
            .expect("the provisioned root is owner-private");

        let root = RunnerStateRoot::open(&path).expect("the provisioned root is initialized");

        assert!(matches!(root.state(), RunnerState::Pristine { .. }));
        assert!(path.join(STATE_FILE).is_file());
        assert!(path.join(DocumentKind::Journal.file_name()).is_file());
    }

    #[test]
    fn exact_pending_promotion_survives_reopen_and_rejects_changed_identity() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut root = RunnerStateRoot::open(&path).expect("the private root opens");
        let active = receipt(root.state().request_id());
        let mut pending = active.clone();
        pending.authority = EnrollmentAuthority::ReplacementPending;
        root.record_receipt(pending)
            .expect("the pending receipt is recorded");
        let mut wrong = active.clone();
        wrong.runner_id = active.enrollment_id;
        assert!(root.record_promotion(wrong).is_err());

        root.record_promotion(active.clone())
            .expect("the exact promotion commits");
        root.record_promotion(active.clone())
            .expect("equal promotion replays");
        drop(root);

        let reopened = RunnerStateRoot::open(&path).expect("the promoted root reopens");
        assert_eq!(reopened.state().receipt(), Some(&active));
    }

    #[test]
    fn issued_receipt_survives_reopen_exactly() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut first = RunnerStateRoot::open(&path).expect("the private root opens");
        let issued = receipt(first.state().request_id());
        first
            .record_receipt(issued.clone())
            .expect("the issued receipt is atomically recorded");
        drop(first);

        let reopened = RunnerStateRoot::open(&path).expect("the private root reopens");

        assert_eq!(reopened.state().receipt(), Some(&issued));
    }

    #[test]
    fn second_process_cannot_share_state_root_lock() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let _held = RunnerStateRoot::open(&path).expect("the private root opens");

        let error = RunnerStateRoot::open(&path)
            .expect_err("a second process-lifetime root owner must fail closed");

        assert_eq!(error.to_string(), "runner state root is already locked");
    }

    #[test]
    fn state_document_with_open_permissions_fails_closed() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let first = RunnerStateRoot::open(&path).expect("the private root opens");
        drop(first);
        fs::set_permissions(
            path.join(STATE_FILE),
            fs::Permissions::from_mode(permission_bits(Mode::RUSR | Mode::WUSR | Mode::RGRP)),
        )
        .expect("the fixture state permissions change");

        let error = RunnerStateRoot::open(&path)
            .expect_err("a state document visible to the group fails closed");

        assert!(matches!(error, RunnerStateError::InvalidStateIdentity));
    }

    #[test]
    fn oversized_state_document_fails_closed() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let first = RunnerStateRoot::open(&path).expect("the private root opens");
        drop(first);
        fs::write(
            path.join(STATE_FILE),
            vec![b'x'; (MAX_STATE_BYTES + 1) as usize],
        )
        .expect("the oversized state fixture is written");

        let error =
            RunnerStateRoot::open(&path).expect_err("an oversized state document fails closed");

        assert!(matches!(error, RunnerStateError::StateTooLarge));
    }

    #[test]
    fn malformed_state_document_fails_closed() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let first = RunnerStateRoot::open(&path).expect("the private root opens");
        drop(first);
        fs::write(path.join(STATE_FILE), b"{").expect("the malformed state fixture is written");

        let error =
            RunnerStateRoot::open(&path).expect_err("a malformed state document fails closed");

        assert!(matches!(error, RunnerStateError::CorruptState));
    }

    #[test]
    fn durable_registration_rejects_a_revision_regression() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut state = RunnerStateRoot::open(&path).expect("the private root opens");
        let issued = receipt(state.state().request_id());
        let stale_revision = issued.registration_revision();
        state
            .record_receipt(issued)
            .expect("the issued receipt is recorded");
        let next_revision = PositiveU64::try_new(stale_revision.get() + 1)
            .expect("the successor fixture revision is positive");
        let digest = advertisement_digest(&empty_advertisement())
            .expect("the explicit empty advertisement has a digest");
        state
            .record_registration(next_revision, digest.clone())
            .expect("the successor registration is recorded");

        let error = state
            .record_registration(stale_revision, digest)
            .expect_err("the durable receipt cannot regress");

        assert!(matches!(error, RunnerStateError::InvalidTransition));
        assert_eq!(
            state
                .state()
                .receipt()
                .expect("the durable receipt remains present")
                .registration_revision(),
            next_revision
        );
    }
}
