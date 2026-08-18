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
use signalbox_runner_wire::{
    CanonicalUuid, Digest, LeaseCorrelation, LeasePhase, LeasePhaseKind, MAX_FRAME_BYTES,
    OperationCorrelation, OperationFailure, PositiveU64, ReconnectInventory, RetainedResult,
};
use uuid::Uuid;

const STATE_DOCUMENT_VERSION: u64 = 1;
const ROOT_MODE: u32 = 0o700;
const STATE_MODE: u32 = 0o600;
const PERMISSION_MASK: u32 = 0o7777;
const STATE_FILE: &str = "enrollment-state.json";
const OPERATION_JOURNAL_FILE: &str = "operation-journal.json";
const MAX_STATE_BYTES: u64 = 16 * 1024;
const MAX_OPERATION_JOURNAL_BYTES: u64 = MAX_FRAME_BYTES as u64;

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

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationJournalDocument {
    version: u64,
    inventory: ReconnectInventory,
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
    /// Current durable operation journal.
    OperationJournal,
    /// Single-use replacement journal used for atomic publication.
    TemporaryOperationJournal,
}

impl fmt::Display for StateResource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Root => "runner state root",
            Self::RootParent => "runner state root parent",
            Self::StateDocument => "runner state document",
            Self::TemporaryDocument => "runner temporary state document",
            Self::OperationJournal => "runner operation journal",
            Self::TemporaryOperationJournal => "runner temporary operation journal",
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

/// Typed fail-closed durable enrollment-state error.
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
    /// The operation journal was not a regular owner-only file.
    InvalidOperationJournalIdentity,
    /// The durable document exceeded its closed size bound.
    StateTooLarge,
    /// The operation journal exceeded the runner frame-size bound.
    OperationJournalTooLarge,
    /// JSON shape, version, or typed field decoding failed.
    CorruptState,
    /// Operation-journal shape, version, or retained item validation failed.
    CorruptOperationJournal,
    /// A returned receipt did not name the journaled request.
    RequestMismatch,
    /// A lifecycle update was attempted in the wrong state.
    InvalidTransition,
    /// A retained operation does not name the current registration or slot.
    OperationCorrelationMismatch,
    /// Atomic rename completed, but durability of the published state is unknown.
    CommitAmbiguous {
        /// Exact document whose rename preceded the directory-fsync failure.
        resource: StateResource,
        /// Directory-fsync failure after the document rename.
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
            Self::InvalidOperationJournalIdentity => {
                formatter.write_str("runner operation journal identity or permissions are invalid")
            }
            Self::StateTooLarge => formatter.write_str("runner state document exceeds its bound"),
            Self::OperationJournalTooLarge => {
                formatter.write_str("runner operation journal exceeds its bound")
            }
            Self::CorruptState => formatter.write_str("runner state document is corrupt"),
            Self::CorruptOperationJournal => {
                formatter.write_str("runner operation journal is corrupt")
            }
            Self::RequestMismatch => {
                formatter.write_str("enrollment receipt request correlation is invalid")
            }
            Self::InvalidTransition => {
                formatter.write_str("runner durable-state transition is invalid")
            }
            Self::OperationCorrelationMismatch => {
                formatter.write_str("runner operation correlation is invalid")
            }
            Self::CommitAmbiguous { resource, .. } => {
                write!(formatter, "{resource} commit outcome is ambiguous")
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
            Self::Io { source, .. } | Self::CommitAmbiguous { source, .. } => Some(source),
            Self::InvalidRootPath
            | Self::InvalidRootIdentity
            | Self::RootBusy
            | Self::StateDisappeared
            | Self::InvalidStateIdentity
            | Self::InvalidOperationJournalIdentity
            | Self::StateTooLarge
            | Self::OperationJournalTooLarge
            | Self::CorruptState
            | Self::CorruptOperationJournal
            | Self::RequestMismatch
            | Self::InvalidTransition
            | Self::OperationCorrelationMismatch => None,
        }
    }
}

/// Locked owner-private state root and its current durable enrollment fact.
#[derive(Debug)]
pub struct RunnerStateRoot {
    directory: File,
    state: RunnerState,
    inventory: ReconnectInventory,
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
        };

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

        let state = match openat(
            &directory,
            STATE_FILE,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(descriptor) => read_state(File::from(descriptor), effective_user)?,
            Err(rustix::io::Errno::NOENT) => {
                let state = RunnerState::Pristine {
                    request_id: CanonicalUuid::from_uuid(Uuid::now_v7()),
                };
                write_state(&directory, &state)?;
                state
            }
            Err(error) => {
                return Err(RunnerStateError::Io {
                    operation: StateOperation::Open,
                    resource: StateResource::StateDocument,
                    source: rustix_error(error),
                });
            }
        };
        let inventory = match openat(
            &directory,
            OPERATION_JOURNAL_FILE,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(descriptor) => read_operation_journal(File::from(descriptor), effective_user)?,
            Err(rustix::io::Errno::NOENT) => ReconnectInventory::default(),
            Err(error) => {
                return Err(RunnerStateError::Io {
                    operation: StateOperation::Open,
                    resource: StateResource::OperationJournal,
                    source: rustix_error(error),
                });
            }
        };
        validate_operation_journal_correlations(&state, &inventory)?;
        Ok(Self {
            directory,
            state,
            inventory,
        })
    }

    /// Borrows the exact current in-memory copy of fsynced state.
    pub const fn state(&self) -> &RunnerState {
        &self.state
    }

    /// Borrows the exact current in-memory copy of the fsynced operation slots.
    pub const fn reconnect_inventory(&self) -> &ReconnectInventory {
        &self.inventory
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

    /// Atomically journals one exact lease phase without opening another slot.
    pub fn record_lease_phase(&mut self, next: LeasePhase) -> Result<(), RunnerStateError> {
        self.validate_current_lease_correlation(&next.correlation)?;
        if self.inventory.operation_failure.is_some() {
            return Err(RunnerStateError::InvalidTransition);
        }
        match self.inventory.lease.as_ref() {
            None if next.phase == LeasePhaseKind::WaitingDispatch => {}
            None => return Err(RunnerStateError::InvalidTransition),
            Some(current) if current == &next => return Ok(()),
            Some(current) if current.correlation != next.correlation => {
                return Err(RunnerStateError::OperationCorrelationMismatch);
            }
            Some(current)
                if matches!(
                    (current.phase, next.phase),
                    (
                        LeasePhaseKind::WaitingDispatch,
                        LeasePhaseKind::DispatchReceived
                    ) | (
                        LeasePhaseKind::DispatchReceived,
                        LeasePhaseKind::ExecutionMayHaveStarted
                    )
                ) => {}
            Some(_) => return Err(RunnerStateError::InvalidTransition),
        }
        let mut inventory = self.inventory.clone();
        inventory.lease = Some(next);
        write_operation_journal(&self.directory, &inventory)?;
        self.inventory = inventory;
        Ok(())
    }

    /// Atomically retains terminal evidence until its exact durable acknowledgement.
    pub fn record_terminal_result(
        &mut self,
        result: RetainedResult,
    ) -> Result<(), RunnerStateError> {
        self.validate_current_lease_correlation(&result.correlation)?;
        let lease = self
            .inventory
            .lease
            .as_ref()
            .ok_or(RunnerStateError::InvalidTransition)?;
        if lease.phase != LeasePhaseKind::ExecutionMayHaveStarted {
            return Err(RunnerStateError::InvalidTransition);
        }
        if lease.correlation != result.correlation {
            return Err(RunnerStateError::OperationCorrelationMismatch);
        }
        match self.inventory.result.as_ref() {
            None => {}
            Some(current) if current == &result => return Ok(()),
            Some(_) => return Err(RunnerStateError::OperationCorrelationMismatch),
        }
        let mut inventory = self.inventory.clone();
        inventory.result = Some(result);
        write_operation_journal(&self.directory, &inventory)?;
        self.inventory = inventory;
        Ok(())
    }

    /// Atomically releases the exact lease and result after daemon recording.
    pub fn acknowledge_terminal_result(
        &mut self,
        correlation: &LeaseCorrelation,
    ) -> Result<(), RunnerStateError> {
        match (
            self.inventory.lease.as_ref(),
            self.inventory.result.as_ref(),
        ) {
            (Some(lease), Some(result))
                if lease.correlation == *correlation && result.correlation == *correlation => {}
            (Some(_), Some(_)) => {
                return Err(RunnerStateError::OperationCorrelationMismatch);
            }
            (None, None) | (Some(_), None) | (None, Some(_)) => {
                return Err(RunnerStateError::InvalidTransition);
            }
        }
        let inventory = ReconnectInventory::default();
        write_operation_journal(&self.directory, &inventory)?;
        self.inventory = inventory;
        Ok(())
    }

    /// Atomically retains one refused lease offer until its exact acknowledgement.
    pub fn record_lease_offer_failure(
        &mut self,
        failure: OperationFailure,
    ) -> Result<(), RunnerStateError> {
        let OperationCorrelation::LeaseOffer(correlation) = &failure.correlation else {
            return Err(RunnerStateError::InvalidTransition);
        };
        self.validate_current_lease_correlation(correlation)?;
        if self.inventory.lease.is_some() || self.inventory.result.is_some() {
            return Err(RunnerStateError::InvalidTransition);
        }
        match self.inventory.operation_failure.as_ref() {
            None => {}
            Some(current) if current == &failure => return Ok(()),
            Some(_) => return Err(RunnerStateError::OperationCorrelationMismatch),
        }
        let mut inventory = self.inventory.clone();
        inventory.operation_failure = Some(failure);
        write_operation_journal(&self.directory, &inventory)?;
        self.inventory = inventory;
        Ok(())
    }

    /// Atomically releases one exact retained lease-offer failure.
    pub fn acknowledge_lease_offer_failure(
        &mut self,
        correlation: &OperationCorrelation,
    ) -> Result<(), RunnerStateError> {
        if !matches!(correlation, OperationCorrelation::LeaseOffer(_)) {
            return Err(RunnerStateError::InvalidTransition);
        }
        match self.inventory.operation_failure.as_ref() {
            Some(failure) if &failure.correlation == correlation => {}
            Some(_) => return Err(RunnerStateError::OperationCorrelationMismatch),
            None => return Err(RunnerStateError::InvalidTransition),
        }
        let mut inventory = self.inventory.clone();
        inventory.operation_failure = None;
        write_operation_journal(&self.directory, &inventory)?;
        self.inventory = inventory;
        Ok(())
    }

    fn validate_current_lease_correlation(
        &self,
        correlation: &LeaseCorrelation,
    ) -> Result<(), RunnerStateError> {
        let receipt = self
            .state
            .receipt()
            .ok_or(RunnerStateError::InvalidTransition)?;
        if correlation.runner_id == receipt.runner_id()
            && correlation.registration_revision == receipt.registration_revision()
        {
            Ok(())
        } else {
            Err(RunnerStateError::OperationCorrelationMismatch)
        }
    }
}

fn validate_operation_journal_correlations(
    state: &RunnerState,
    inventory: &ReconnectInventory,
) -> Result<(), RunnerStateError> {
    let receipt = state.receipt();
    let lease_owned = match (receipt, inventory.lease.as_ref()) {
        (_, None) => true,
        (Some(receipt), Some(lease)) => lease.correlation.runner_id == receipt.runner_id(),
        (None, Some(_)) => false,
    };
    let result_owned = match (receipt, inventory.result.as_ref()) {
        (_, None) => true,
        (Some(receipt), Some(result)) => result.correlation.runner_id == receipt.runner_id(),
        (None, Some(_)) => false,
    };
    let result_matches_lease = match (inventory.lease.as_ref(), inventory.result.as_ref()) {
        (_, None) => true,
        (Some(lease), Some(result)) => {
            lease.phase == LeasePhaseKind::ExecutionMayHaveStarted
                && lease.correlation == result.correlation
        }
        (None, Some(_)) => false,
    };
    let failure_owned = match (receipt, inventory.operation_failure.as_ref()) {
        (_, None) => true,
        (Some(receipt), Some(failure)) => match &failure.correlation {
            OperationCorrelation::LeaseOffer(correlation) => {
                correlation.runner_id == receipt.runner_id()
            }
            OperationCorrelation::Provision(_) | OperationCorrelation::Release(_) => false,
        },
        (None, Some(_)) => false,
    };
    let failure_precedes_lease = inventory.operation_failure.is_none()
        || (inventory.lease.is_none() && inventory.result.is_none());
    if lease_owned
        && result_owned
        && result_matches_lease
        && failure_owned
        && failure_precedes_lease
    {
        Ok(())
    } else {
        Err(RunnerStateError::CorruptOperationJournal)
    }
}

fn operation_journal_has_only_supported_slots(inventory: &ReconnectInventory) -> bool {
    inventory.workspace_operation.is_none()
        && inventory
            .operation_failure
            .as_ref()
            .is_none_or(|failure| match &failure.correlation {
                OperationCorrelation::LeaseOffer(_) => true,
                OperationCorrelation::Provision(_) => false,
                OperationCorrelation::Release(_) => false,
            })
        && inventory.leak_page.is_none()
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

fn read_operation_journal(
    mut file: File,
    effective_user: u32,
) -> Result<ReconnectInventory, RunnerStateError> {
    let metadata = file.metadata().map_err(|source| RunnerStateError::Io {
        operation: StateOperation::Inspect,
        resource: StateResource::OperationJournal,
        source,
    })?;
    if !metadata.is_file()
        || metadata.uid() != effective_user
        || metadata.mode() & PERMISSION_MASK != STATE_MODE
    {
        return Err(RunnerStateError::InvalidOperationJournalIdentity);
    }
    if metadata.len() > MAX_OPERATION_JOURNAL_BYTES {
        return Err(RunnerStateError::OperationJournalTooLarge);
    }
    let mut content = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_OPERATION_JOURNAL_BYTES + 1)
        .read_to_end(&mut content)
        .map_err(|source| RunnerStateError::Io {
            operation: StateOperation::Read,
            resource: StateResource::OperationJournal,
            source,
        })?;
    if content.len() as u64 > MAX_OPERATION_JOURNAL_BYTES {
        return Err(RunnerStateError::OperationJournalTooLarge);
    }
    let document: OperationJournalDocument =
        serde_json::from_slice(&content).map_err(|_| RunnerStateError::CorruptOperationJournal)?;
    if document.version != STATE_DOCUMENT_VERSION
        || document.inventory.validate().is_err()
        || !operation_journal_has_only_supported_slots(&document.inventory)
    {
        return Err(RunnerStateError::CorruptOperationJournal);
    }
    Ok(document.inventory)
}

fn write_state(directory: &File, state: &RunnerState) -> Result<(), RunnerStateError> {
    let document = StateDocument {
        version: STATE_DOCUMENT_VERSION,
        state: state.clone(),
    };
    let mut encoded = serde_json::to_vec(&document).map_err(|_| RunnerStateError::CorruptState)?;
    encoded.push(b'\n');
    let temporary_name = format!(".enrollment-state-{}.tmp", Uuid::now_v7());
    let descriptor = openat(
        directory,
        temporary_name.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|error| RunnerStateError::Io {
        operation: StateOperation::Create,
        resource: StateResource::TemporaryDocument,
        source: rustix_error(error),
    })?;
    rustix::fs::fchmod(&descriptor, Mode::RUSR | Mode::WUSR).map_err(|error| {
        RunnerStateError::Io {
            operation: StateOperation::ConfigurePermissions,
            resource: StateResource::TemporaryDocument,
            source: rustix_error(error),
        }
    })?;
    let mut temporary = File::from(descriptor);
    let prepared = (|| {
        temporary
            .write_all(&encoded)
            .map_err(|source| RunnerStateError::Io {
                operation: StateOperation::Write,
                resource: StateResource::TemporaryDocument,
                source,
            })?;
        temporary.sync_all().map_err(|source| RunnerStateError::Io {
            operation: StateOperation::Sync,
            resource: StateResource::TemporaryDocument,
            source,
        })
    })();
    if let Err(error) = prepared {
        let _ = unlinkat(directory, temporary_name.as_str(), AtFlags::empty());
        return Err(error);
    }
    if let Err(error) = renameat(directory, temporary_name.as_str(), directory, STATE_FILE) {
        let _ = unlinkat(directory, temporary_name.as_str(), AtFlags::empty());
        return Err(RunnerStateError::Io {
            operation: StateOperation::Rename,
            resource: StateResource::StateDocument,
            source: rustix_error(error),
        });
    }
    directory
        .sync_all()
        .map_err(|source| RunnerStateError::CommitAmbiguous {
            resource: StateResource::StateDocument,
            source,
        })
}

fn write_operation_journal(
    directory: &File,
    inventory: &ReconnectInventory,
) -> Result<(), RunnerStateError> {
    inventory
        .validate()
        .map_err(|_| RunnerStateError::CorruptOperationJournal)?;
    if !operation_journal_has_only_supported_slots(inventory)
        || (inventory.operation_failure.is_some()
            && (inventory.lease.is_some() || inventory.result.is_some()))
    {
        return Err(RunnerStateError::CorruptOperationJournal);
    }
    let document = OperationJournalDocument {
        version: STATE_DOCUMENT_VERSION,
        inventory: inventory.clone(),
    };
    let mut encoded =
        serde_json::to_vec(&document).map_err(|_| RunnerStateError::CorruptOperationJournal)?;
    encoded.push(b'\n');
    if encoded.len() as u64 > MAX_OPERATION_JOURNAL_BYTES {
        return Err(RunnerStateError::OperationJournalTooLarge);
    }
    let temporary_name = format!(".operation-journal-{}.tmp", Uuid::now_v7());
    let descriptor = openat(
        directory,
        temporary_name.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|error| RunnerStateError::Io {
        operation: StateOperation::Create,
        resource: StateResource::TemporaryOperationJournal,
        source: rustix_error(error),
    })?;
    rustix::fs::fchmod(&descriptor, Mode::RUSR | Mode::WUSR).map_err(|error| {
        RunnerStateError::Io {
            operation: StateOperation::ConfigurePermissions,
            resource: StateResource::TemporaryOperationJournal,
            source: rustix_error(error),
        }
    })?;
    let mut temporary = File::from(descriptor);
    let prepared = (|| {
        temporary
            .write_all(&encoded)
            .map_err(|source| RunnerStateError::Io {
                operation: StateOperation::Write,
                resource: StateResource::TemporaryOperationJournal,
                source,
            })?;
        temporary.sync_all().map_err(|source| RunnerStateError::Io {
            operation: StateOperation::Sync,
            resource: StateResource::TemporaryOperationJournal,
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
        OPERATION_JOURNAL_FILE,
    ) {
        let _ = unlinkat(directory, temporary_name.as_str(), AtFlags::empty());
        return Err(RunnerStateError::Io {
            operation: StateOperation::Rename,
            resource: StateResource::OperationJournal,
            source: rustix_error(error),
        });
    }
    directory
        .sync_all()
        .map_err(|source| RunnerStateError::CommitAmbiguous {
            resource: StateResource::OperationJournal,
            source,
        })
}

fn rustix_error(error: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}

#[cfg(test)]
mod tests {
    use signalbox_runner_wire::{
        Advertisement, DetailName, ExecutionErrorKind, FailureCategory, FailureDetail,
        ReleaseCorrelation, ReleasePhase, SandboxProfile, TerminalResult, WireToolName,
        WorkingDirectory, WorkspaceOperation, advertisement_digest,
    };
    use tempfile::TempDir;

    use super::*;

    /// Arbitrary daemon-issued enrollment identity used throughout state tests.
    const ARBITRARY_ENROLLMENT_UUID: u128 = 0x100;
    /// Arbitrary daemon-issued runner identity used throughout state tests.
    const ARBITRARY_RUNNER_UUID: u128 = 0x200;
    /// Arbitrary daemon-issued authentication-reference identity used throughout state tests.
    const ARBITRARY_AUTHENTICATION_UUID: u128 = 0x300;
    const ARBITRARY_LEASE_UUID: u128 = 0x400;
    const ARBITRARY_OTHER_LEASE_UUID: u128 = 0x401;
    const ARBITRARY_SESSION_UUID: u128 = 0x500;
    const ARBITRARY_TURN_UUID: u128 = 0x600;
    const ARBITRARY_REQUEST_UUID: u128 = 0x700;
    const ARBITRARY_ATTEMPT_UUID: u128 = 0x800;
    const ARBITRARY_ISSUING_ATTEMPT_UUID: u128 = 0x900;
    const ARBITRARY_OTHER_RUNNER_UUID: u128 = 0xa00;
    const ARBITRARY_MANIFEST_UUID: u128 = 0xb00;
    const INITIAL_REGISTRATION_REVISION: u64 = 1;
    const SUCCESSOR_REGISTRATION_REVISION: u64 = 2;

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

    fn positive(value: u64) -> PositiveU64 {
        PositiveU64::try_new(value).expect("the fixture value is positive")
    }

    fn lease_correlation() -> LeaseCorrelation {
        LeaseCorrelation {
            registration_revision: positive(INITIAL_REGISTRATION_REVISION),
            lease_id: CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_LEASE_UUID)),
            lease_generation: positive(1),
            runner_id: CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_RUNNER_UUID)),
            placement_revision: positive(1),
            working_directory: WorkingDirectory::try_new("sessions/example".to_owned())
                .expect("the fixture working directory is valid"),
            sandbox_profile: SandboxProfile::WorkspaceRestricted,
            tool_name: WireToolName::try_new("sandboxed_exec".to_owned())
                .expect("the generic exec-family fixture name is valid"),
            session_id: CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_SESSION_UUID)),
            turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_TURN_UUID)),
            tool_request_id: CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_REQUEST_UUID)),
            tool_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_ATTEMPT_UUID)),
            issuing_turn_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(
                ARBITRARY_ISSUING_ATTEMPT_UUID,
            )),
            tool_dispatch_generation: positive(1),
        }
    }

    fn lease_phase(phase: LeasePhaseKind) -> LeasePhase {
        LeasePhase {
            correlation: lease_correlation(),
            phase,
        }
    }

    fn enrolled_root(parent: &TempDir) -> RunnerStateRoot {
        let mut root =
            RunnerStateRoot::open(&root_path(parent)).expect("the private state root opens");
        let issued = receipt(root.state().request_id());
        root.record_receipt(issued)
            .expect("the enrollment receipt is durable");
        root
    }

    fn inventory_with_lease(lease: LeasePhase) -> ReconnectInventory {
        ReconnectInventory {
            lease: Some(lease),
            ..ReconnectInventory::default()
        }
    }

    fn retained_result(result: TerminalResult) -> RetainedResult {
        RetainedResult {
            correlation: lease_correlation(),
            result,
        }
    }

    fn lease_offer_failure() -> OperationFailure {
        lease_offer_failure_for(lease_correlation())
    }

    fn lease_offer_failure_for(correlation: LeaseCorrelation) -> OperationFailure {
        OperationFailure {
            correlation: OperationCorrelation::LeaseOffer(correlation),
            category: FailureCategory::LeaseAdmissionRefused,
            detail: FailureDetail::try_new(
                DetailName::try_new("fixture-refusal".to_owned())
                    .expect("the fixture detail code is valid"),
                String::from("the synthetic offer is refused"),
                serde_json::json!({}),
            )
            .expect("the fixture failure detail is bounded"),
        }
    }

    fn inventory_with_failure(failure: OperationFailure) -> ReconnectInventory {
        ReconnectInventory {
            operation_failure: Some(failure),
            ..ReconnectInventory::default()
        }
    }

    fn started_root(parent: &TempDir) -> RunnerStateRoot {
        let mut root = enrolled_root(parent);
        root.record_lease_phase(lease_phase(LeasePhaseKind::WaitingDispatch))
            .expect("the waiting-dispatch predecessor is durable");
        root.record_lease_phase(lease_phase(LeasePhaseKind::DispatchReceived))
            .expect("the dispatch-received predecessor is durable");
        root.record_lease_phase(lease_phase(LeasePhaseKind::ExecutionMayHaveStarted))
            .expect("the execution-possible predecessor is durable");
        root
    }

    fn replace_operation_journal(path: &Path, inventory: ReconnectInventory) {
        let document = OperationJournalDocument {
            version: STATE_DOCUMENT_VERSION,
            inventory,
        };
        let encoded = serde_json::to_vec(&document).expect("the journal fixture encodes");
        fs::write(path.join(OPERATION_JOURNAL_FILE), encoded)
            .expect("the operation journal fixture is replaced");
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
        assert_eq!(
            reopened.reconnect_inventory(),
            &ReconnectInventory::default()
        );
    }

    /// INV-011 / INV-024: a claim acknowledgement is durable before the runner
    /// treats the lease as waiting for dispatch.
    #[test]
    fn inv011_inv024_waiting_dispatch_phase_survives_reopen() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut root = enrolled_root(&parent);
        let waiting = lease_phase(LeasePhaseKind::WaitingDispatch);
        root.record_lease_phase(waiting.clone())
            .expect("the waiting-dispatch phase is durable");
        drop(root);

        let reopened = RunnerStateRoot::open(&path).expect("the private state root reopens");

        assert_eq!(
            reopened.reconnect_inventory(),
            &inventory_with_lease(waiting)
        );
    }

    /// INV-011 / INV-024: dispatch receipt advances only from the durable
    /// waiting phase and survives process restart exactly.
    #[test]
    fn inv011_inv024_dispatch_received_phase_survives_reopen() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut root = enrolled_root(&parent);
        root.record_lease_phase(lease_phase(LeasePhaseKind::WaitingDispatch))
            .expect("the waiting-dispatch predecessor is durable");
        let received = lease_phase(LeasePhaseKind::DispatchReceived);
        root.record_lease_phase(received.clone())
            .expect("the dispatch-received phase is durable");
        drop(root);

        let reopened = RunnerStateRoot::open(&path).expect("the private state root reopens");

        assert_eq!(
            reopened.reconnect_inventory(),
            &inventory_with_lease(received)
        );
    }

    /// INV-011 / INV-024: the execution-possible boundary advances from the
    /// exact received dispatch and survives process restart exactly.
    #[test]
    fn inv011_inv024_execution_may_have_started_phase_survives_reopen() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut root = enrolled_root(&parent);
        root.record_lease_phase(lease_phase(LeasePhaseKind::WaitingDispatch))
            .expect("the waiting-dispatch predecessor is durable");
        root.record_lease_phase(lease_phase(LeasePhaseKind::DispatchReceived))
            .expect("the dispatch-received predecessor is durable");
        let started = lease_phase(LeasePhaseKind::ExecutionMayHaveStarted);
        root.record_lease_phase(started.clone())
            .expect("the execution-possible phase is durable");
        drop(root);

        let reopened = RunnerStateRoot::open(&path).expect("the private state root reopens");

        assert_eq!(
            reopened.reconnect_inventory(),
            &inventory_with_lease(started)
        );
    }

    #[test]
    fn lease_journal_rejects_a_phase_skip() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut root = enrolled_root(&parent);

        let error = root
            .record_lease_phase(lease_phase(LeasePhaseKind::DispatchReceived))
            .expect_err("dispatch cannot precede its claim acknowledgement");

        assert!(matches!(error, RunnerStateError::InvalidTransition));
        assert_eq!(root.reconnect_inventory(), &ReconnectInventory::default());
    }

    #[test]
    fn lease_journal_rejects_a_phase_regression() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut root = enrolled_root(&parent);
        root.record_lease_phase(lease_phase(LeasePhaseKind::WaitingDispatch))
            .expect("the waiting-dispatch predecessor is durable");
        let received = lease_phase(LeasePhaseKind::DispatchReceived);
        root.record_lease_phase(received.clone())
            .expect("the dispatch-received phase is durable");

        let error = root
            .record_lease_phase(lease_phase(LeasePhaseKind::WaitingDispatch))
            .expect_err("a durable lease phase cannot regress");

        assert!(matches!(error, RunnerStateError::InvalidTransition));
        assert_eq!(root.reconnect_inventory(), &inventory_with_lease(received));
    }

    #[test]
    fn occupied_lease_slot_rejects_another_correlation() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut root = enrolled_root(&parent);
        let waiting = lease_phase(LeasePhaseKind::WaitingDispatch);
        root.record_lease_phase(waiting.clone())
            .expect("the first lease occupies the serial slot");
        let mut foreign = waiting.clone();
        foreign.correlation.lease_id =
            CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_OTHER_LEASE_UUID));

        let error = root
            .record_lease_phase(foreign)
            .expect_err("another lease cannot replace the occupied slot");

        assert!(matches!(
            error,
            RunnerStateError::OperationCorrelationMismatch
        ));
        assert_eq!(root.reconnect_inventory(), &inventory_with_lease(waiting));
    }

    #[test]
    fn lease_journal_rejects_another_registration() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut root = enrolled_root(&parent);
        let mut foreign = lease_phase(LeasePhaseKind::WaitingDispatch);
        foreign.correlation.registration_revision = positive(SUCCESSOR_REGISTRATION_REVISION);

        let error = root
            .record_lease_phase(foreign)
            .expect_err("another registration cannot journal a lease");

        assert!(matches!(
            error,
            RunnerStateError::OperationCorrelationMismatch
        ));
        assert_eq!(root.reconnect_inventory(), &ReconnectInventory::default());
    }

    /// INV-011 / INV-024: successful terminal evidence survives restart beside
    /// its exact execution-possible lease correlation.
    #[test]
    fn inv011_inv024_successful_terminal_result_survives_reopen() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut root = started_root(&parent);
        let result = retained_result(TerminalResult::Success {
            text: String::from("completed"),
        });
        root.record_terminal_result(result.clone())
            .expect("the successful result is durable");
        drop(root);

        let reopened = RunnerStateRoot::open(&path).expect("the private state root reopens");

        assert_eq!(reopened.reconnect_inventory().result, Some(result));
        assert_eq!(
            reopened.reconnect_inventory().lease,
            Some(lease_phase(LeasePhaseKind::ExecutionMayHaveStarted))
        );
    }

    /// INV-011 / INV-024: known terminal failure evidence survives restart
    /// without widening its closed failure category.
    #[test]
    fn inv011_inv024_known_failure_terminal_result_survives_reopen() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut root = started_root(&parent);
        let result = retained_result(TerminalResult::KnownFailure {
            error_kind: ExecutionErrorKind::ExecutionFailed,
            detail: Some(String::from("synthetic failure")),
        });
        root.record_terminal_result(result.clone())
            .expect("the known failure is durable");
        drop(root);

        let reopened = RunnerStateRoot::open(&path).expect("the private state root reopens");

        assert_eq!(reopened.reconnect_inventory().result, Some(result));
    }

    /// INV-011 / INV-024: ambiguous terminal evidence survives restart exactly.
    #[test]
    fn inv011_inv024_ambiguous_terminal_result_survives_reopen() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut root = started_root(&parent);
        let result = retained_result(TerminalResult::Ambiguous);
        root.record_terminal_result(result.clone())
            .expect("the ambiguous result is durable");
        drop(root);

        let reopened = RunnerStateRoot::open(&path).expect("the private state root reopens");

        assert_eq!(reopened.reconnect_inventory().result, Some(result));
    }

    #[test]
    fn terminal_result_requires_the_execution_possible_phase() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut root = enrolled_root(&parent);
        root.record_lease_phase(lease_phase(LeasePhaseKind::WaitingDispatch))
            .expect("the waiting-dispatch phase is durable");

        let error = root
            .record_terminal_result(retained_result(TerminalResult::Ambiguous))
            .expect_err("terminal evidence cannot precede the executor gate");

        assert!(matches!(error, RunnerStateError::InvalidTransition));
        assert_eq!(root.reconnect_inventory().result, None);
    }

    #[test]
    fn terminal_result_rejects_another_lease_correlation() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut root = started_root(&parent);
        let mut foreign = retained_result(TerminalResult::Ambiguous);
        foreign.correlation.lease_id =
            CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_OTHER_LEASE_UUID));

        let error = root
            .record_terminal_result(foreign)
            .expect_err("another lease cannot occupy the terminal-result slot");

        assert!(matches!(
            error,
            RunnerStateError::OperationCorrelationMismatch
        ));
        assert_eq!(root.reconnect_inventory().result, None);
    }

    /// INV-011 / INV-024: daemon recording frees the result and its lease in
    /// one durable journal replacement.
    #[test]
    fn inv011_inv024_result_acknowledgement_clears_both_slots_across_reopen() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut root = started_root(&parent);
        root.record_terminal_result(retained_result(TerminalResult::Ambiguous))
            .expect("the terminal evidence is durable");

        root.acknowledge_terminal_result(&lease_correlation())
            .expect("the exact recorded acknowledgement releases both slots");
        drop(root);

        let reopened = RunnerStateRoot::open(&path).expect("the private state root reopens");

        assert_eq!(
            reopened.reconnect_inventory(),
            &ReconnectInventory::default()
        );
    }

    #[test]
    fn result_acknowledgement_rejects_another_correlation() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut root = started_root(&parent);
        let result = retained_result(TerminalResult::Ambiguous);
        root.record_terminal_result(result.clone())
            .expect("the terminal evidence is durable");
        let mut foreign = lease_correlation();
        foreign.lease_id = CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_OTHER_LEASE_UUID));

        let error = root
            .acknowledge_terminal_result(&foreign)
            .expect_err("another lease cannot clear retained evidence");

        assert!(matches!(
            error,
            RunnerStateError::OperationCorrelationMismatch
        ));
        assert_eq!(root.reconnect_inventory().result, Some(result));
    }

    /// INV-011 / INV-024: a refused lease offer survives restart until its
    /// exact durable acknowledgement.
    #[test]
    fn inv011_inv024_lease_offer_failure_survives_reopen() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut root = enrolled_root(&parent);
        let failure = lease_offer_failure();
        root.record_lease_offer_failure(failure.clone())
            .expect("the lease-offer failure is durable");
        drop(root);

        let reopened = RunnerStateRoot::open(&path).expect("the private state root reopens");

        assert_eq!(
            reopened.reconnect_inventory(),
            &inventory_with_failure(failure)
        );
    }

    /// INV-011 / INV-024: the exact daemon acknowledgement retires the
    /// refused lease offer across restart.
    #[test]
    fn inv011_inv024_lease_offer_failure_acknowledgement_survives_reopen() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut root = enrolled_root(&parent);
        let failure = lease_offer_failure();
        root.record_lease_offer_failure(failure.clone())
            .expect("the lease-offer failure is durable");

        root.acknowledge_lease_offer_failure(&failure.correlation)
            .expect("the exact acknowledgement retires the failure");
        drop(root);

        let reopened = RunnerStateRoot::open(&path).expect("the private state root reopens");

        assert_eq!(
            reopened.reconnect_inventory(),
            &ReconnectInventory::default()
        );
    }

    #[test]
    fn lease_offer_failure_acknowledgement_rejects_another_correlation() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut root = enrolled_root(&parent);
        let failure = lease_offer_failure();
        root.record_lease_offer_failure(failure.clone())
            .expect("the lease-offer failure is durable");
        let mut foreign = lease_correlation();
        foreign.lease_id = CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_OTHER_LEASE_UUID));

        let error = root
            .acknowledge_lease_offer_failure(&OperationCorrelation::LeaseOffer(foreign))
            .expect_err("another lease cannot retire the retained failure");

        assert!(matches!(
            error,
            RunnerStateError::OperationCorrelationMismatch
        ));
        assert_eq!(root.reconnect_inventory(), &inventory_with_failure(failure));
    }

    #[test]
    fn lease_offer_failure_rejects_an_occupied_lease_slot() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut root = enrolled_root(&parent);
        let waiting = lease_phase(LeasePhaseKind::WaitingDispatch);
        root.record_lease_phase(waiting.clone())
            .expect("the waiting lease occupies the serial slot");

        let error = root
            .record_lease_offer_failure(lease_offer_failure())
            .expect_err("a refused offer cannot coexist with a current lease");

        assert!(matches!(error, RunnerStateError::InvalidTransition));
        assert_eq!(root.reconnect_inventory(), &inventory_with_lease(waiting));
    }

    #[test]
    fn lease_offer_failure_rejects_another_registration() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut root = enrolled_root(&parent);
        let mut foreign = lease_correlation();
        foreign.registration_revision = positive(SUCCESSOR_REGISTRATION_REVISION);

        let error = root
            .record_lease_offer_failure(lease_offer_failure_for(foreign))
            .expect_err("another registration cannot journal refusal evidence");

        assert!(matches!(
            error,
            RunnerStateError::OperationCorrelationMismatch
        ));
        assert_eq!(root.reconnect_inventory(), &ReconnectInventory::default());
    }

    #[test]
    fn lease_phase_rejects_a_retained_offer_failure() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut root = enrolled_root(&parent);
        let failure = lease_offer_failure();
        root.record_lease_offer_failure(failure.clone())
            .expect("the refused offer occupies the serial slot");

        let error = root
            .record_lease_phase(lease_phase(LeasePhaseKind::WaitingDispatch))
            .expect_err("a lease cannot replace retained refusal evidence");

        assert!(matches!(error, RunnerStateError::InvalidTransition));
        assert_eq!(root.reconnect_inventory(), &inventory_with_failure(failure));
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
    fn operation_journal_with_open_permissions_fails_closed() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut root = enrolled_root(&parent);
        root.record_lease_phase(lease_phase(LeasePhaseKind::WaitingDispatch))
            .expect("the operation journal is created");
        drop(root);
        fs::set_permissions(
            path.join(OPERATION_JOURNAL_FILE),
            fs::Permissions::from_mode(permission_bits(Mode::RUSR | Mode::WUSR | Mode::RGRP)),
        )
        .expect("the fixture journal permissions change");

        let error = RunnerStateRoot::open(&path)
            .expect_err("an operation journal visible to the group fails closed");

        assert!(matches!(
            error,
            RunnerStateError::InvalidOperationJournalIdentity
        ));
    }

    #[test]
    fn oversized_operation_journal_fails_closed() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut root = enrolled_root(&parent);
        root.record_lease_phase(lease_phase(LeasePhaseKind::WaitingDispatch))
            .expect("the operation journal is created");
        drop(root);
        fs::write(
            path.join(OPERATION_JOURNAL_FILE),
            vec![b'x'; (MAX_OPERATION_JOURNAL_BYTES + 1) as usize],
        )
        .expect("the oversized operation journal fixture is written");

        let error =
            RunnerStateRoot::open(&path).expect_err("an oversized operation journal fails closed");

        assert!(matches!(error, RunnerStateError::OperationJournalTooLarge));
    }

    #[test]
    fn malformed_operation_journal_fails_closed() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut root = enrolled_root(&parent);
        root.record_lease_phase(lease_phase(LeasePhaseKind::WaitingDispatch))
            .expect("the operation journal is created");
        drop(root);
        fs::write(path.join(OPERATION_JOURNAL_FILE), b"{")
            .expect("the malformed operation journal fixture is written");

        let error =
            RunnerStateRoot::open(&path).expect_err("a malformed operation journal fails closed");

        assert!(matches!(error, RunnerStateError::CorruptOperationJournal));
    }

    #[test]
    fn operation_journal_for_another_runner_fails_closed() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut root = enrolled_root(&parent);
        root.record_lease_phase(lease_phase(LeasePhaseKind::WaitingDispatch))
            .expect("the operation journal is created");
        drop(root);
        let mut foreign = lease_phase(LeasePhaseKind::WaitingDispatch);
        foreign.correlation.runner_id =
            CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_OTHER_RUNNER_UUID));
        replace_operation_journal(&path, inventory_with_lease(foreign));

        let error = RunnerStateRoot::open(&path)
            .expect_err("another runner's operation journal fails closed");

        assert!(matches!(error, RunnerStateError::CorruptOperationJournal));
    }

    #[test]
    fn operation_failure_journal_for_another_runner_fails_closed() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut root = enrolled_root(&parent);
        root.record_lease_offer_failure(lease_offer_failure())
            .expect("the operation journal is created");
        drop(root);
        let mut foreign = lease_correlation();
        foreign.runner_id = CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_OTHER_RUNNER_UUID));
        replace_operation_journal(
            &path,
            inventory_with_failure(lease_offer_failure_for(foreign)),
        );

        let error = RunnerStateRoot::open(&path)
            .expect_err("another runner's refusal journal fails closed");

        assert!(matches!(error, RunnerStateError::CorruptOperationJournal));
    }

    #[test]
    fn unsupported_operation_journal_slot_fails_closed() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut root = enrolled_root(&parent);
        root.record_lease_phase(lease_phase(LeasePhaseKind::WaitingDispatch))
            .expect("the operation journal is created");
        drop(root);
        let unsupported = ReconnectInventory {
            workspace_operation: Some(WorkspaceOperation::Release {
                correlation: ReleaseCorrelation {
                    session_id: CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_SESSION_UUID)),
                    placement_revision: positive(1),
                    runner_id: CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_RUNNER_UUID)),
                    manifest_id: CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_MANIFEST_UUID)),
                },
                phase: ReleasePhase::ReleaseAccepted,
            }),
            ..ReconnectInventory::default()
        };
        replace_operation_journal(&path, unsupported);

        let error = RunnerStateRoot::open(&path)
            .expect_err("an unauthored operation-journal slot fails closed");

        assert!(matches!(error, RunnerStateError::CorruptOperationJournal));
    }

    #[test]
    fn unsupported_release_failure_journal_fails_closed() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let mut root = enrolled_root(&parent);
        root.record_lease_phase(lease_phase(LeasePhaseKind::WaitingDispatch))
            .expect("the operation journal is created");
        drop(root);
        let unsupported = OperationFailure {
            correlation: OperationCorrelation::Release(ReleaseCorrelation {
                session_id: CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_SESSION_UUID)),
                placement_revision: positive(1),
                runner_id: CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_RUNNER_UUID)),
                manifest_id: CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_MANIFEST_UUID)),
            }),
            category: FailureCategory::WorkspaceCleanupFailed,
            detail: FailureDetail::try_new(
                DetailName::try_new("fixture-cleanup".to_owned())
                    .expect("the fixture detail code is valid"),
                String::from("the synthetic cleanup failed"),
                serde_json::json!({}),
            )
            .expect("the fixture failure detail is bounded"),
        };
        replace_operation_journal(&path, inventory_with_failure(unsupported));

        let error = RunnerStateRoot::open(&path)
            .expect_err("an unauthored release-failure journal fails closed");

        assert!(matches!(error, RunnerStateError::CorruptOperationJournal));
    }

    #[test]
    fn cross_wired_terminal_result_journal_fails_closed() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let path = root_path(&parent);
        let root = started_root(&parent);
        drop(root);
        let mut result = retained_result(TerminalResult::Ambiguous);
        result.correlation.lease_id =
            CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_OTHER_LEASE_UUID));
        let cross_wired = ReconnectInventory {
            lease: Some(lease_phase(LeasePhaseKind::ExecutionMayHaveStarted)),
            result: Some(result),
            ..ReconnectInventory::default()
        };
        replace_operation_journal(&path, cross_wired);

        let error =
            RunnerStateRoot::open(&path).expect_err("a result from another lease fails closed");

        assert!(matches!(error, RunnerStateError::CorruptOperationJournal));
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
