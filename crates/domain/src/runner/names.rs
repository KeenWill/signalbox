//! Runner names for `docs/spec/runner-protocol.md`.

use super::catalog::{RunnerSandboxProfile, WorkspaceCapability};
use crate::{RunnerId, ToolName};
use std::num::NonZeroU64;

pub(super) const NAME_MAX_BYTES: usize = 64;
const EXACT_VALUE_MAX_BYTES: usize = 4_096;

const WORKSPACE_BRANCH_MAX_BYTES: usize = 255;

/// Why runner domain input or stored facts fail closed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunnerDomainError {
    /// The supplied text is empty.
    Empty,
    /// The supplied text contains a null byte.
    ContainsNull,
    /// The supplied text exceeds its byte limit.
    TooLong,
    /// The supplied portable name has invalid syntax.
    InvalidName,
    /// The supplied digest or revision is not canonical lowercase hexadecimal text.
    InvalidHex,
    /// The supplied Git branch name is not a canonical branch ref name.
    InvalidBranchName,
    /// The supplied runner-root-relative path is not canonical.
    InvalidRelativePath,
    /// The tool input schema is not a normalized JSON object.
    InvalidToolInputSchema,
    /// A capability class appears more than once.
    DuplicateCapabilityClass(RunnerCapabilityClass),
    /// A tool name appears more than once.
    DuplicateTool(ToolName),
    /// A credential profile appears more than once.
    DuplicateProfile(CredentialProfileName),
    /// A workspace capability appears more than once.
    DuplicateWorkspaceCapability(WorkspaceCapability),
    /// A sandbox profile appears more than once.
    DuplicateSandboxProfile(RunnerSandboxProfile),
    /// The placement contains too many per-tool permission overrides.
    TooManyPermissionOverrides,
    /// The advertisement contains too many repository entries.
    TooManyAdvertisedRepositories,
    /// A credential profile names a tool absent from the catalog.
    UndeclaredProfileTool(ToolName),
    /// An idempotent tool is incorrectly admissible on the daemon.
    UnsupportedDaemonIdempotency(ToolName),
    /// The runner enrollment has been revoked.
    EnrollmentRevoked,
    /// The enrollment or catalog does not allow the capability class.
    CapabilityClassNotAllowed(RunnerCapabilityClass),
    /// The runner advertised a tool absent from the catalog.
    ToolUndeclared(ToolName),
    /// The runner does not satisfy the tool placement policy.
    ToolLocusNotAllowed(ToolName),
    /// The runner advertised a credential profile absent from the catalog.
    CredentialProfileUndeclared(CredentialProfileName),
    /// The runner advertised a workspace capability absent from the catalog.
    WorkspaceCapabilityNotAllowed(WorkspaceCapability),
    /// The runner advertised a sandbox profile absent from the catalog.
    SandboxProfileNotAllowed(RunnerSandboxProfile),
    /// A repository entry requires a profile absent from the same advertisement.
    RepositoryProfileUnavailable(CredentialProfileName),
    /// The requested transition is invalid from the current state.
    InvalidState,
    /// Supplied facts do not correlate with the authoritative aggregate.
    CorrelationMismatch,
    /// A positive generation has no representable successor.
    GenerationExhausted,
    /// A retry reused an existing physical attempt identity.
    AttemptIdentityReuse,
    /// The selected runner does not satisfy the placement request.
    SelectorMismatch,
    /// The requested credential profile is unavailable on the selected runner.
    CredentialProfileUnavailable,
    /// The supplied working directory differs from the pinned directory.
    WorkingDirectoryMismatch,
    /// The selected runner lacks the required workspace capability.
    WorkspaceCapabilityUnavailable,
    /// The selected runner lacks the required sandbox profile.
    SandboxProfileUnavailable,
    /// The selected runner lacks the requested repository entry.
    RepositoryUnavailable,
    /// The provisioned workspace does not match the placement request.
    WorkspaceMismatch,
    /// A required tool is unavailable on the selected runner.
    ToolUnavailable,
    /// The credential grant has been revoked.
    GrantRevoked,
    /// The runner registration is no longer the current revision.
    RegistrationChanged,
    /// Another registration preparation already holds the enrollment fence.
    RegistrationInProgress,
    /// Independently stored facts disagree during reconstitution.
    CorruptStoredFacts,
}

fn validate_name(value: String) -> Result<String, RunnerDomainError> {
    if value.is_empty() {
        return Err(RunnerDomainError::Empty);
    }
    if value.contains('\0') {
        return Err(RunnerDomainError::ContainsNull);
    }
    if value.len() > NAME_MAX_BYTES {
        return Err(RunnerDomainError::TooLong);
    }
    if !value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(RunnerDomainError::InvalidName);
    }
    Ok(value)
}

pub(super) fn validate_exact(value: String) -> Result<String, RunnerDomainError> {
    if value.is_empty() {
        return Err(RunnerDomainError::Empty);
    }
    if value.contains('\0') {
        return Err(RunnerDomainError::ContainsNull);
    }
    if value.len() > EXACT_VALUE_MAX_BYTES {
        return Err(RunnerDomainError::TooLong);
    }
    Ok(value)
}

fn validate_lower_hex(value: String, lengths: &[usize]) -> Result<String, RunnerDomainError> {
    if !lengths.contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(RunnerDomainError::InvalidHex);
    }
    Ok(value)
}

/// A daemon-defined class used to target an unpinned runner.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunnerCapabilityClass(String);

impl RunnerCapabilityClass {
    /// Validates and constructs a portable runner capability class.
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        validate_name(value).map(Self)
    }

    /// Returns the validated capability class text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A runner-local credential profile represented to the daemon by name only.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CredentialProfileName(String);

impl CredentialProfileName {
    /// Validates and constructs a portable credential profile name.
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        validate_name(value).map(Self)
    }

    /// Returns the validated credential profile name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Exact runner-interpreted working-directory text.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunnerWorkingDirectory(String);

impl RunnerWorkingDirectory {
    /// Maximum UTF-8 bytes admitted by an exact runner working directory.
    pub const MAX_BYTES: usize = EXACT_VALUE_MAX_BYTES;

    /// Validates and constructs exact runner working-directory text.
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        validate_exact(value).map(Self)
    }

    /// Returns the exact runner working-directory text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Exact repository key used for worktree provisioning.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceRepositoryKey(String);

impl WorkspaceRepositoryKey {
    /// Validates and constructs a portable workspace repository key.
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        validate_name(value).map(Self)
    }

    /// Returns the validated repository key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Canonical lowercase SHA-256 identity of one configuration-validated clone URL.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalCloneUrlDigest(String);

impl CanonicalCloneUrlDigest {
    /// Validates and constructs a canonical clone-URL digest.
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        validate_lower_hex(value, &[64]).map(Self)
    }

    /// Returns the canonical lowercase hexadecimal digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Canonical full Git object identity used to recover a workspace.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceRevision(String);

impl WorkspaceRevision {
    /// Validates a full SHA-1 or SHA-256 Git object identity.
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        validate_lower_hex(value, &[40, 64]).map(Self)
    }

    /// Returns the canonical lowercase full object identity.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Validated Git branch name, without the `refs/heads/` prefix.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceBranchName(String);

impl WorkspaceBranchName {
    /// Validates the branch as the complete `refs/heads/<name>` ref form.
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        let invalid_component = value.split('/').any(|component| {
            component.is_empty() || component.starts_with('.') || component.ends_with(".lock")
        });
        if value.is_empty()
            || value.len() > WORKSPACE_BRANCH_MAX_BYTES
            || value == "@"
            || value.starts_with('-')
            || value.ends_with('.')
            || value.contains("..")
            || value.contains("@{")
            || value.bytes().any(|byte| {
                byte <= 0x20
                    || byte == 0x7f
                    || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
            })
            || invalid_component
        {
            return Err(RunnerDomainError::InvalidBranchName);
        }
        Ok(Self(value))
    }

    /// Returns the validated branch name without `refs/heads/`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Runner-root-relative path recorded in a workspace manifest.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceRelativePath(String);

impl WorkspaceRelativePath {
    /// Validates a bounded nonempty relative path without traversal components.
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        let exact = validate_exact(value)?;
        if exact.starts_with('/')
            || exact
                .split('/')
                .any(|component| component.is_empty() || matches!(component, "." | ".."))
        {
            return Err(RunnerDomainError::InvalidRelativePath);
        }
        Ok(Self(exact))
    }

    /// Returns the exact runner-root-relative path.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Exclusive Git recovery facts retained by a repository workspace.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum WorkspaceRecovery {
    /// Recovery checks out one exact detached commit.
    Commit {
        /// The exact commit to recover.
        revision: WorkspaceRevision,
    },
    /// Recovery checks out one validated branch at its exact revision.
    Branch {
        /// The validated branch name without `refs/heads/`.
        name: WorkspaceBranchName,
        /// The exact revision the branch must name.
        revision: WorkspaceRevision,
    },
}

/// Class-or-identity runner targeting.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RunnerSelector {
    /// Selects one exact runner identity.
    Identity(RunnerId),
    /// Selects any runner advertising the required capability class.
    CapabilityClass(RunnerCapabilityClass),
}

/// Positive runner lease, placement, or grant generation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunnerGeneration(NonZeroU64);

impl RunnerGeneration {
    /// Returns the first positive runner generation.
    pub const fn one() -> Self {
        Self(NonZeroU64::MIN)
    }

    /// Converts a nonzero integer into a runner generation.
    pub const fn try_from_u64(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the positive generation as an integer.
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// Returns the next generation when the integer range permits it.
    pub const fn checked_next(self) -> Option<Self> {
        match self.get().checked_add(1) {
            Some(value) => match NonZeroU64::new(value) {
                Some(value) => Some(Self(value)),
                None => None,
            },
            None => None,
        }
    }
}
