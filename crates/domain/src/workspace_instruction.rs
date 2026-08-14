//! Typed workspace-instruction registration and turn-start provenance.

use std::{error::Error, fmt};

use sha2::{Digest, Sha256};

use crate::{SessionId, TurnId};

crate::define_identity!(
    /// Identifies one immutable discovery snapshot.
    InstructionDiscoveryId
);
crate::define_identity!(
    /// Identifies one registered instruction bundle.
    InstructionBundleId
);
crate::define_identity!(
    /// Identifies one immutable turn instruction manifest.
    TurnInstructionManifestId
);

const MANIFEST_PREFIX: &[u8] = b"signalbox-turn-instruction-manifest-v1";
const EMPTY_ELIGIBILITY_PREFIX: &[u8] = b"signalbox-instruction-eligibility-v1";
const MAX_INSTRUCTION_PATH_BYTES: usize = 4096;

/// One versioned SHA-256 digest over instruction evidence.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InstructionDigest([u8; 32]);

impl InstructionDigest {
    /// Hashes exact bytes with the version-one algorithm.
    pub fn sha256(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// Reconstitutes one stored 32-byte SHA-256 value.
    pub const fn from_sha256(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrows the exact digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// One canonical absolute UTF-8 path admitted to durable instruction evidence.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InstructionPath(String);

impl InstructionPath {
    /// Validates an absolute canonical spelling without touching the filesystem.
    pub fn try_new(value: String) -> Result<Self, InstructionPathError> {
        if value.is_empty() {
            return Err(InstructionPathError::Empty);
        }
        if value.len() > MAX_INSTRUCTION_PATH_BYTES {
            return Err(InstructionPathError::TooLong);
        }
        if value.contains('\0') {
            return Err(InstructionPathError::ContainsNull);
        }
        let Some(components) = value.strip_prefix('/') else {
            return Err(InstructionPathError::NotAbsolute);
        };
        if components.is_empty()
            || components
                .split('/')
                .any(|component| component.is_empty() || matches!(component, "." | ".."))
        {
            return Err(InstructionPathError::NotCanonical);
        }
        Ok(Self(value))
    }

    /// Borrows the exact path spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why one durable instruction path was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstructionPathError {
    Empty,
    ContainsNull,
    TooLong,
    NotAbsolute,
    NotCanonical,
}

impl fmt::Display for InstructionPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "instruction path was empty",
            Self::ContainsNull => "instruction path contained NUL",
            Self::TooLong => "instruction path exceeded its byte bound",
            Self::NotAbsolute => "instruction path was not absolute",
            Self::NotCanonical => "instruction path was not canonical",
        })
    }
}

impl Error for InstructionPathError {}

/// Authority route through which one candidate was found.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum InstructionDiscoveryRootKind {
    Workspace,
    Configured,
}

/// Closed version-one bundle kinds.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum InstructionBundleKind {
    AgentDocument,
    AgentSkill,
}

/// Validated portable Agent Skills display metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstructionSkillMetadata {
    name: String,
    description: String,
}

impl InstructionSkillMetadata {
    /// Validates the required portable fields and parent-directory identity.
    pub fn try_new(
        name: String,
        description: String,
        parent_directory: &str,
    ) -> Result<Self, InstructionSkillMetadataError> {
        if !(1..=64).contains(&name.len())
            || name.starts_with('-')
            || name.ends_with('-')
            || name.contains("--")
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(InstructionSkillMetadataError::InvalidName);
        }
        if name != parent_directory {
            return Err(InstructionSkillMetadataError::ParentMismatch);
        }
        if description.is_empty() || description.chars().count() > 1024 {
            return Err(InstructionSkillMetadataError::InvalidDescription);
        }
        Ok(Self { name, description })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }
}

/// Why portable skill metadata was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstructionSkillMetadataError {
    InvalidName,
    InvalidDescription,
    ParentMismatch,
}

impl fmt::Display for InstructionSkillMetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidName => "skill name violated the portable grammar",
            Self::InvalidDescription => "skill description violated the portable bound",
            Self::ParentMismatch => "skill name differed from its parent directory",
        })
    }
}

impl Error for InstructionSkillMetadataError {}

/// Validated content ready for append-only registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstructionBundleRegistration {
    kind: InstructionBundleKind,
    root_kind: InstructionDiscoveryRootKind,
    root_path: InstructionPath,
    source_path: InstructionPath,
    source_bytes: u64,
    source_hash: InstructionDigest,
    skill: Option<InstructionSkillMetadata>,
}

impl InstructionBundleRegistration {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: InstructionBundleKind,
        root_kind: InstructionDiscoveryRootKind,
        root_path: InstructionPath,
        source_path: InstructionPath,
        source_bytes: u64,
        source_hash: InstructionDigest,
        skill: Option<InstructionSkillMetadata>,
    ) -> Option<Self> {
        let shape_matches = matches!(kind, InstructionBundleKind::AgentSkill) == skill.is_some();
        let below_root = source_path
            .as_str()
            .strip_prefix(root_path.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'));
        (shape_matches && below_root).then_some(Self {
            kind,
            root_kind,
            root_path,
            source_path,
            source_bytes,
            source_hash,
            skill,
        })
    }

    pub const fn kind(&self) -> InstructionBundleKind {
        self.kind
    }
    pub const fn root_kind(&self) -> InstructionDiscoveryRootKind {
        self.root_kind
    }
    pub const fn root_path(&self) -> &InstructionPath {
        &self.root_path
    }
    pub const fn source_path(&self) -> &InstructionPath {
        &self.source_path
    }
    /// Borrows the source path relative to its authorizing root.
    pub fn relative_source_path(&self) -> &str {
        &self.source_path.as_str()[self.root_path.as_str().len() + 1..]
    }
    /// Borrows an agent document's root-relative directory scope.
    pub fn agent_document_scope(&self) -> Option<&str> {
        (self.kind == InstructionBundleKind::AgentDocument).then(|| {
            self.relative_source_path()
                .strip_suffix("AGENTS.md")
                .and_then(|prefix| prefix.strip_suffix('/'))
                .unwrap_or_default()
        })
    }
    pub const fn source_bytes(&self) -> u64 {
        self.source_bytes
    }
    pub const fn source_hash(&self) -> InstructionDigest {
        self.source_hash
    }
    pub const fn skill(&self) -> Option<&InstructionSkillMetadata> {
        self.skill.as_ref()
    }
}

/// The immutable empty-eligibility turn-start manifest implemented in slice one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnInstructionManifest {
    id: TurnInstructionManifestId,
    session: SessionId,
    turn: TurnId,
    eligibility_hash: InstructionDigest,
    manifest_hash: InstructionDigest,
}

impl TurnInstructionManifest {
    /// Constructs the exact empty turn-start manifest and its canonical hash.
    pub fn empty_turn_start(
        id: TurnInstructionManifestId,
        session: SessionId,
        turn: TurnId,
    ) -> Self {
        let eligibility_hash = InstructionDigest::sha256(EMPTY_ELIGIBILITY_PREFIX);
        let mut bytes = Vec::with_capacity(MANIFEST_PREFIX.len() + 80);
        bytes.extend_from_slice(MANIFEST_PREFIX);
        bytes.extend_from_slice(session.as_uuid().as_bytes());
        bytes.extend_from_slice(turn.as_uuid().as_bytes());
        bytes.extend_from_slice(eligibility_hash.as_bytes());
        bytes.extend_from_slice(b"turn_start");
        let manifest_hash = InstructionDigest::sha256(&bytes);
        Self {
            id,
            session,
            turn,
            eligibility_hash,
            manifest_hash,
        }
    }

    /// Reconstitutes only evidence equal to the canonical empty manifest.
    pub fn reconstitute_empty_turn_start(
        id: TurnInstructionManifestId,
        session: SessionId,
        turn: TurnId,
        eligibility_hash: InstructionDigest,
        manifest_hash: InstructionDigest,
    ) -> Option<Self> {
        let expected = Self::empty_turn_start(id, session, turn);
        (expected.eligibility_hash == eligibility_hash && expected.manifest_hash == manifest_hash)
            .then_some(expected)
    }

    pub const fn id(&self) -> TurnInstructionManifestId {
        self.id
    }
    pub const fn session(&self) -> SessionId {
        self.session
    }
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    pub const fn eligibility_hash(&self) -> InstructionDigest {
        self.eligibility_hash
    }
    pub const fn manifest_hash(&self) -> InstructionDigest {
        self.manifest_hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity<T>(value: u128, constructor: impl FnOnce(uuid::Uuid) -> T) -> T {
        constructor(uuid::Uuid::from_u128(value))
    }

    /// INV-061: turn instruction provenance authenticates the exact turn boundary.
    #[test]
    fn inv061_manifest_hash_changes_with_the_turn() {
        let first = TurnInstructionManifest::empty_turn_start(
            identity(1, TurnInstructionManifestId::from_uuid),
            identity(2, SessionId::from_uuid),
            identity(3, TurnId::from_uuid),
        );
        let second = TurnInstructionManifest::empty_turn_start(
            identity(1, TurnInstructionManifestId::from_uuid),
            identity(2, SessionId::from_uuid),
            identity(4, TurnId::from_uuid),
        );

        assert_ne!(first.manifest_hash(), second.manifest_hash());
    }

    #[test]
    fn skill_metadata_requires_its_portable_parent_name() {
        let error = InstructionSkillMetadata::try_new(
            String::from("review-rust"),
            String::from("Review Rust changes."),
            "other",
        );

        assert_eq!(error, Err(InstructionSkillMetadataError::ParentMismatch));
    }
}
