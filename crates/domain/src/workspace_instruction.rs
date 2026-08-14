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
    /// The supplied path had no bytes.
    Empty,
    /// The supplied path contained U+0000.
    ContainsNull,
    /// The supplied path exceeded the durable byte bound.
    TooLong,
    /// The supplied path was not absolute.
    NotAbsolute,
    /// The supplied path contained an empty, dot, or dot-dot component.
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
    /// The session's fixed daemon-local workspace root.
    Workspace,
    /// One daemon root registered by configuration.
    Configured,
}

/// Closed version-one bundle kinds.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum InstructionBundleKind {
    /// One scoped `AGENTS.md` document.
    AgentDocument,
    /// One portable Agent Skills directory represented by `SKILL.md`.
    AgentSkill,
}

/// Named portable fields whose roles must remain visible at call sites.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstructionSkillMetadataInput {
    /// Portable skill name from frontmatter.
    pub name: String,
    /// Portable skill description from frontmatter.
    pub description: String,
    /// Candidate directory name that must equal the portable skill name.
    pub parent_directory: String,
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
        input: InstructionSkillMetadataInput,
    ) -> Result<Self, InstructionSkillMetadataError> {
        let InstructionSkillMetadataInput {
            name,
            description,
            parent_directory,
        } = input;
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

    /// Borrows the validated portable name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Borrows the validated portable description.
    pub fn description(&self) -> &str {
        &self.description
    }
}

/// Why portable skill metadata was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstructionSkillMetadataError {
    /// The name violated the portable grammar or byte bound.
    InvalidName,
    /// The description violated the portable content bound.
    InvalidDescription,
    /// The portable name did not equal the candidate directory name.
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

/// Named fields validated into one append-only bundle registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstructionBundleRegistrationInput {
    /// Closed kind of instruction source.
    pub kind: InstructionBundleKind,
    /// Authority route that yielded the candidate.
    pub root_kind: InstructionDiscoveryRootKind,
    /// Canonical absolute path of the authorizing root.
    pub root_path: InstructionPath,
    /// Canonical absolute path of the candidate source file.
    pub source_path: InstructionPath,
    /// Exact source byte length.
    pub source_bytes: u64,
    /// Digest of the exact source bytes.
    pub source_hash: InstructionDigest,
    /// Validated skill metadata, present exactly for skill bundles.
    pub skill: Option<InstructionSkillMetadata>,
}

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
    /// Validates path containment and bundle/metadata shape.
    pub fn new(input: InstructionBundleRegistrationInput) -> Option<Self> {
        let InstructionBundleRegistrationInput {
            kind,
            root_kind,
            root_path,
            source_path,
            source_bytes,
            source_hash,
            skill,
        } = input;
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

    /// Returns the closed bundle kind.
    pub const fn kind(&self) -> InstructionBundleKind {
        self.kind
    }
    /// Returns the authority route that yielded the source.
    pub const fn root_kind(&self) -> InstructionDiscoveryRootKind {
        self.root_kind
    }
    /// Borrows the canonical authorizing root path.
    pub const fn root_path(&self) -> &InstructionPath {
        &self.root_path
    }
    /// Borrows the canonical source-file path.
    pub const fn source_path(&self) -> &InstructionPath {
        &self.source_path
    }
    /// Returns the exact source byte length.
    pub const fn source_bytes(&self) -> u64 {
        self.source_bytes
    }
    /// Returns the digest of the exact source bytes.
    pub const fn source_hash(&self) -> InstructionDigest {
        self.source_hash
    }
    /// Borrows validated skill metadata when the bundle is a skill.
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

/// Stored digests revalidated as one canonical empty turn-start manifest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmptyTurnInstructionManifestEvidence {
    /// SHA-256 of the frozen empty eligibility identity sequence.
    pub eligibility_hash: InstructionDigest,
    /// SHA-256 of the complete canonical turn-start manifest representation.
    pub manifest_hash: InstructionDigest,
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
        evidence: EmptyTurnInstructionManifestEvidence,
    ) -> Option<Self> {
        let EmptyTurnInstructionManifestEvidence {
            eligibility_hash,
            manifest_hash,
        } = evidence;
        let expected = Self::empty_turn_start(id, session, turn);
        (expected.eligibility_hash == eligibility_hash && expected.manifest_hash == manifest_hash)
            .then_some(expected)
    }

    /// Returns this manifest's durable identity.
    pub const fn id(&self) -> TurnInstructionManifestId {
        self.id
    }
    /// Returns the session whose turn owns this manifest.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the turn whose start boundary this manifest authenticates.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Returns the frozen empty eligibility hash.
    pub const fn eligibility_hash(&self) -> InstructionDigest {
        self.eligibility_hash
    }
    /// Returns the canonical manifest hash.
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
        let error = InstructionSkillMetadata::try_new(InstructionSkillMetadataInput {
            name: String::from("review-rust"),
            description: String::from("Review Rust changes."),
            parent_directory: String::from("other"),
        });

        assert_eq!(error, Err(InstructionSkillMetadataError::ParentMismatch));
    }
}
