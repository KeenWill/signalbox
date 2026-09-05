//! Typed workspace-instruction registration and turn-start provenance.

use std::{
    cmp::Ordering,
    collections::BTreeMap,
    error::Error,
    fmt,
    hash::{Hash, Hasher},
    path::Path,
    sync::Arc,
};

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
const SOURCE_CONTENT_PREFIX: &[u8] = b"signalbox-instruction-source-v1";
const ADMITTED_SET_PREFIX: &[u8] = b"signalbox-instruction-admitted-set-v1";
const MAX_INSTRUCTION_PATH_BYTES: usize = 4096;
const MAX_INSTRUCTION_SOURCE_PATH_BYTES: usize = MAX_INSTRUCTION_PATH_BYTES * 2 + 1;

/// One versioned SHA-256 digest over instruction evidence.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InstructionDigest([u8; 32]);

impl InstructionDigest {
    /// Hashes one already-canonical preimage with the version-one algorithm.
    ///
    /// Callers pass the complete separated representation. Content digests
    /// carried by stored evidence have their own constructor because the spec
    /// frames them; never hash raw source bytes through this entry point.
    pub fn sha256(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// Hashes registered source bytes under the version-one source separator.
    ///
    /// The preimage is `signalbox-instruction-source-v1`, the eight-byte
    /// big-endian source length, then the exact registered source bytes. The
    /// stored `source_sha256` field name does not make this the bare SHA-256 of
    /// those bytes; a later version changes the separator rather than the name.
    pub fn source_content(bytes: &[u8]) -> Self {
        let mut state = Sha256::new();
        state.update(SOURCE_CONTENT_PREFIX);
        state.update((bytes.len() as u64).to_be_bytes());
        state.update(bytes);
        Self(state.finalize().into())
    }

    /// Returns the version-one admitted-set hash of the empty admitted set.
    ///
    /// The empty-set vector is the separator followed by an all-zero eight-byte
    /// record count, so this is a frozen constant rather than the separator
    /// alone.
    pub fn empty_admitted_set() -> Self {
        let mut state = Sha256::new();
        state.update(ADMITTED_SET_PREFIX);
        state.update(0u64.to_be_bytes());
        Self(state.finalize().into())
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

/// One component in an immutable root-relative prefix chain.
#[derive(Debug)]
struct RelativePathNode {
    parent: Option<Arc<Self>>,
    component: Arc<str>,
}

/// Root-scoped builder that shares repeated source-path ancestry.
#[derive(Debug, Default)]
pub struct InstructionSourcePathInterner {
    edges: BTreeMap<(InstructionPath, Option<usize>, String), Arc<RelativePathNode>>,
}

/// One reusable interned directory prefix beneath an instruction root.
#[derive(Clone, Debug)]
pub struct InstructionSourcePathPrefix {
    root_path: InstructionPath,
    relative_leaf: Option<Arc<RelativePathNode>>,
    relative_bytes: usize,
    absolute_hash_state: Sha256,
}

impl InstructionSourcePathInterner {
    /// Creates an empty source-path interner.
    pub const fn new() -> Self {
        Self {
            edges: BTreeMap::new(),
        }
    }

    /// Starts an empty root-relative prefix without copying the root spelling.
    pub fn root_prefix(root_path: InstructionPath) -> InstructionSourcePathPrefix {
        let mut absolute_hash_state = Sha256::new();
        absolute_hash_state.update(root_path.as_str().as_bytes());
        InstructionSourcePathPrefix {
            root_path,
            relative_leaf: None,
            relative_bytes: 0,
            absolute_hash_state,
        }
    }

    /// Appends one validated component while reusing the prefix's ancestry.
    pub fn append_prefix(
        &mut self,
        prefix: &InstructionSourcePathPrefix,
        component: &str,
    ) -> Result<InstructionSourcePathPrefix, InstructionPathError> {
        if component.is_empty() {
            return Err(InstructionPathError::Empty);
        }
        if component.contains('\0') {
            return Err(InstructionPathError::ContainsNull);
        }
        if component.contains('/') || matches!(component, "." | "..") {
            return Err(InstructionPathError::NotCanonical);
        }
        let separator_bytes = usize::from(prefix.relative_leaf.is_some());
        let relative_bytes = prefix
            .relative_bytes
            .saturating_add(separator_bytes)
            .saturating_add(component.len());
        if relative_bytes > MAX_INSTRUCTION_PATH_BYTES {
            return Err(InstructionPathError::TooLong);
        }
        let parent_identity = prefix
            .relative_leaf
            .as_ref()
            .map(|node| Arc::as_ptr(node) as usize);
        let key = (
            prefix.root_path.clone(),
            parent_identity,
            component.to_owned(),
        );
        let relative_leaf = match self.edges.get(&key).cloned() {
            Some(node) => node,
            None => {
                let node = Arc::new(RelativePathNode {
                    parent: prefix.relative_leaf.clone(),
                    component: component.into(),
                });
                self.edges.insert(key, Arc::clone(&node));
                node
            }
        };
        let mut absolute_hash_state = prefix.absolute_hash_state.clone();
        absolute_hash_state.update(b"/");
        absolute_hash_state.update(component.as_bytes());
        Ok(InstructionSourcePathPrefix {
            root_path: prefix.root_path.clone(),
            relative_leaf: Some(relative_leaf),
            relative_bytes,
            absolute_hash_state,
        })
    }
}

/// One canonical absolute UTF-8 path admitted to durable instruction evidence.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InstructionPath(Arc<str>);

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
        Ok(Self(value.into()))
    }

    /// Borrows the exact path spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One canonical absolute UTF-8 candidate path admitted to durable evidence.
///
/// The representation shares its independently bounded root and any repeated
/// root-relative ancestry while retaining exact source spelling.
#[derive(Clone, Debug)]
pub struct InstructionSourcePath {
    root_path: InstructionPath,
    relative_leaf: Arc<RelativePathNode>,
    identity_hash: InstructionDigest,
}

impl InstructionSourcePath {
    /// Validates an absolute source spelling beneath its authorizing root.
    pub fn try_new(
        root_path: InstructionPath,
        value: String,
    ) -> Result<Self, InstructionPathError> {
        Self::try_new_in(&mut InstructionSourcePathInterner::new(), root_path, value)
    }

    /// Validates a source spelling while sharing ancestry through `interner`.
    pub fn try_new_in(
        interner: &mut InstructionSourcePathInterner,
        root_path: InstructionPath,
        value: String,
    ) -> Result<Self, InstructionPathError> {
        if value.is_empty() {
            return Err(InstructionPathError::Empty);
        }
        if value.len() > MAX_INSTRUCTION_SOURCE_PATH_BYTES {
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
        let Some(relative_path) = value
            .strip_prefix(root_path.as_str())
            .and_then(|suffix| suffix.strip_prefix('/'))
        else {
            return Err(InstructionPathError::NotCanonical);
        };
        if relative_path.len() > MAX_INSTRUCTION_PATH_BYTES {
            return Err(InstructionPathError::TooLong);
        }
        let mut prefix = InstructionSourcePathInterner::root_prefix(root_path);
        for component in relative_path.split('/') {
            prefix = interner.append_prefix(&prefix, component)?;
        }
        Self::from_prefix(prefix)
    }

    /// Appends one source name to an already interned directory prefix.
    pub fn try_new_under(
        interner: &mut InstructionSourcePathInterner,
        directory: &InstructionSourcePathPrefix,
        source_name: &str,
    ) -> Result<Self, InstructionPathError> {
        Self::from_prefix(interner.append_prefix(directory, source_name)?)
    }

    fn from_prefix(prefix: InstructionSourcePathPrefix) -> Result<Self, InstructionPathError> {
        let Some(relative_leaf) = prefix.relative_leaf else {
            return Err(InstructionPathError::NotCanonical);
        };
        let identity_hash = InstructionDigest(prefix.absolute_hash_state.finalize().into());
        Ok(Self {
            root_path: prefix.root_path,
            relative_leaf,
            identity_hash,
        })
    }

    /// Renders the exact canonical absolute source path.
    pub fn absolute_path(&self) -> String {
        format!("{}/{}", self.root_path.as_str(), self.relative_path())
    }

    /// Renders the source path relative to its authorizing root.
    pub fn relative_path(&self) -> String {
        let mut nodes = Vec::new();
        let mut current = Some(&*self.relative_leaf);
        while let Some(node) = current {
            nodes.push(node);
            current = node.parent.as_deref();
        }
        let byte_len = nodes.iter().map(|node| node.component.len()).sum::<usize>()
            + nodes.len().saturating_sub(1);
        let mut rendered = String::with_capacity(byte_len);
        for node in nodes.into_iter().rev() {
            if !rendered.is_empty() {
                rendered.push('/');
            }
            rendered.push_str(&node.component);
        }
        rendered
    }
}

impl PartialEq for InstructionSourcePath {
    fn eq(&self, other: &Self) -> bool {
        (Arc::ptr_eq(&self.relative_leaf, &other.relative_leaf)
            && self.root_path == other.root_path)
            || (self.identity_hash == other.identity_hash
                && self.absolute_path() == other.absolute_path())
    }
}

impl Eq for InstructionSourcePath {}

impl PartialOrd for InstructionSourcePath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for InstructionSourcePath {
    fn cmp(&self, other: &Self) -> Ordering {
        self.absolute_path().cmp(&other.absolute_path())
    }
}

impl Hash for InstructionSourcePath {
    fn hash<State: Hasher>(&self, state: &mut State) {
        self.identity_hash.hash(state);
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
    pub source_path: InstructionSourcePath,
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
    source_path: InstructionSourcePath,
    source_bytes: u64,
    source_hash: InstructionDigest,
    skill: Option<InstructionSkillMetadata>,
}

impl InstructionBundleRegistration {
    /// Validates path containment, bundle/metadata shape, and source naming.
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
        let relative_path = source_path.relative_path();
        let source = Path::new(&relative_path);
        let shape_matches = match (kind, skill.as_ref()) {
            (InstructionBundleKind::AgentDocument, None) => {
                source.file_name().is_some_and(|name| name == "AGENTS.md")
            }
            (InstructionBundleKind::AgentSkill, Some(metadata)) => {
                let nested_skill = source
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == metadata.name());
                let configured_root_skill = root_kind == InstructionDiscoveryRootKind::Configured
                    && source == Path::new("SKILL.md")
                    && Path::new(root_path.as_str())
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name == metadata.name());
                source.file_name().is_some_and(|name| name == "SKILL.md")
                    && (nested_skill || configured_root_skill)
            }
            (InstructionBundleKind::AgentDocument, Some(_))
            | (InstructionBundleKind::AgentSkill, None) => false,
        };
        let below_root = source_path.root_path == root_path;
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
    pub const fn source_path(&self) -> &InstructionSourcePath {
        &self.source_path
    }
    /// Renders the source path relative to its authorizing root.
    pub fn relative_source_path(&self) -> String {
        self.source_path.relative_path()
    }
    /// Renders an agent document's root-relative directory scope.
    pub fn agent_document_scope(&self) -> Option<String> {
        (self.kind == InstructionBundleKind::AgentDocument).then(|| {
            let relative_path = self.relative_source_path();
            relative_path
                .strip_suffix("AGENTS.md")
                .and_then(|prefix| prefix.strip_suffix('/'))
                .unwrap_or_default()
                .to_owned()
        })
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
    admitted_set_hash: InstructionDigest,
    manifest_hash: InstructionDigest,
}

/// Stored digests revalidated as one canonical empty turn-start manifest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmptyTurnInstructionManifestEvidence {
    /// SHA-256 of the frozen empty eligibility identity sequence.
    pub eligibility_hash: InstructionDigest,
    /// SHA-256 of the admitted-set head this manifest snapshotted.
    pub admitted_set_hash: InstructionDigest,
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
        let admitted_set_hash = InstructionDigest::empty_admitted_set();
        let mut bytes = Vec::with_capacity(MANIFEST_PREFIX.len() + 106);
        bytes.extend_from_slice(MANIFEST_PREFIX);
        bytes.extend_from_slice(session.as_uuid().as_bytes());
        bytes.extend_from_slice(turn.as_uuid().as_bytes());
        bytes.extend_from_slice(eligibility_hash.as_bytes());
        bytes.extend_from_slice(admitted_set_hash.as_bytes());
        bytes.extend_from_slice(b"turn_start");
        let manifest_hash = InstructionDigest::sha256(&bytes);
        Self {
            id,
            session,
            turn,
            eligibility_hash,
            admitted_set_hash,
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
            admitted_set_hash,
            manifest_hash,
        } = evidence;
        let expected = Self::empty_turn_start(id, session, turn);
        (expected.eligibility_hash == eligibility_hash
            && expected.admitted_set_hash == admitted_set_hash
            && expected.manifest_hash == manifest_hash)
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
    /// Returns the admitted-set hash of the head this manifest snapshotted.
    pub const fn admitted_set_hash(&self) -> InstructionDigest {
        self.admitted_set_hash
    }
    /// Returns the canonical manifest hash.
    pub const fn manifest_hash(&self) -> InstructionDigest {
        self.manifest_hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn identity<T>(value: u128, constructor: impl FnOnce(uuid::Uuid) -> T) -> T {
        constructor(uuid::Uuid::from_u128(value))
    }

    /// turn instruction provenance authenticates the exact turn boundary.
    #[test]
    fn manifest_hash_changes_with_the_turn() {
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
    fn source_content_hash_frames_the_versioned_source_preimage() {
        let source = b"# AGENTS.md\n";

        let framed = InstructionDigest::source_content(source);

        let mut expected = Vec::new();
        expected.extend_from_slice(b"signalbox-instruction-source-v1");
        expected.extend_from_slice(&(source.len() as u64).to_be_bytes());
        expected.extend_from_slice(source);
        assert_eq!(framed, InstructionDigest::sha256(&expected));
        assert_ne!(framed, InstructionDigest::sha256(source));
    }

    /// The length frame is what separates two sources that concatenate alike.
    #[test]
    fn source_content_hash_separates_a_shared_concatenation() {
        let split = InstructionDigest::source_content(b"ab");
        let other = InstructionDigest::source_content(b"a");

        assert_ne!(split, other);
        assert_ne!(
            InstructionDigest::source_content(b""),
            InstructionDigest::sha256(b"signalbox-instruction-source-v1")
        );
    }

    #[test]
    fn empty_admitted_set_hash_frames_an_all_zero_count() {
        let mut expected = Vec::new();
        expected.extend_from_slice(b"signalbox-instruction-admitted-set-v1");
        expected.extend_from_slice(&0u64.to_be_bytes());

        assert_eq!(
            InstructionDigest::empty_admitted_set(),
            InstructionDigest::sha256(&expected)
        );
        assert_ne!(
            InstructionDigest::empty_admitted_set(),
            InstructionDigest::sha256(b"signalbox-instruction-admitted-set-v1")
        );
    }

    #[test]
    fn empty_turn_start_authenticates_the_admitted_set_hash() {
        let manifest = TurnInstructionManifest::empty_turn_start(
            identity(1, TurnInstructionManifestId::from_uuid),
            identity(2, SessionId::from_uuid),
            identity(3, TurnId::from_uuid),
        );

        assert_eq!(
            manifest.admitted_set_hash(),
            InstructionDigest::empty_admitted_set()
        );

        let mut expected = Vec::new();
        expected.extend_from_slice(b"signalbox-turn-instruction-manifest-v1");
        expected.extend_from_slice(uuid::Uuid::from_u128(2).as_bytes());
        expected.extend_from_slice(uuid::Uuid::from_u128(3).as_bytes());
        expected.extend_from_slice(manifest.eligibility_hash().as_bytes());
        expected.extend_from_slice(InstructionDigest::empty_admitted_set().as_bytes());
        expected.extend_from_slice(b"turn_start");
        assert_eq!(
            manifest.manifest_hash(),
            InstructionDigest::sha256(&expected)
        );

        let without_admitted_set = {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(b"signalbox-turn-instruction-manifest-v1");
            bytes.extend_from_slice(uuid::Uuid::from_u128(2).as_bytes());
            bytes.extend_from_slice(uuid::Uuid::from_u128(3).as_bytes());
            bytes.extend_from_slice(manifest.eligibility_hash().as_bytes());
            bytes.extend_from_slice(b"turn_start");
            InstructionDigest::sha256(&bytes)
        };
        assert_ne!(manifest.manifest_hash(), without_admitted_set);
    }

    #[test]
    fn reconstitution_rejects_a_wrong_admitted_set_hash() {
        let id = identity(1, TurnInstructionManifestId::from_uuid);
        let session = identity(2, SessionId::from_uuid);
        let turn = identity(3, TurnId::from_uuid);
        let manifest = TurnInstructionManifest::empty_turn_start(id, session, turn);
        let canonical = EmptyTurnInstructionManifestEvidence {
            eligibility_hash: manifest.eligibility_hash(),
            admitted_set_hash: manifest.admitted_set_hash(),
            manifest_hash: manifest.manifest_hash(),
        };

        assert_eq!(
            TurnInstructionManifest::reconstitute_empty_turn_start(id, session, turn, canonical),
            Some(manifest.clone())
        );

        // The separator alone is the plausible wrong spelling: it omits the
        // all-zero record count the empty-set vector requires.
        let tampered = EmptyTurnInstructionManifestEvidence {
            admitted_set_hash: InstructionDigest::sha256(b"signalbox-instruction-admitted-set-v1"),
            ..canonical
        };
        assert_eq!(
            TurnInstructionManifest::reconstitute_empty_turn_start(id, session, turn, tampered),
            None
        );

        let zeroed = EmptyTurnInstructionManifestEvidence {
            admitted_set_hash: InstructionDigest::from_sha256([0u8; 32]),
            ..canonical
        };
        assert_eq!(
            TurnInstructionManifest::reconstitute_empty_turn_start(id, session, turn, zeroed),
            None
        );

        let borrowed_eligibility = EmptyTurnInstructionManifestEvidence {
            admitted_set_hash: manifest.eligibility_hash(),
            ..canonical
        };
        assert_eq!(
            TurnInstructionManifest::reconstitute_empty_turn_start(
                id,
                session,
                turn,
                borrowed_eligibility
            ),
            None
        );
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

    #[test]
    fn bundle_registration_requires_the_kind_specific_source_name() {
        let agent_document = InstructionBundleRegistration::new(registration_input(
            InstructionBundleKind::AgentDocument,
            "/workspace/NOT-AGENTS.md",
            None,
        ));
        let skill = InstructionBundleRegistration::new(registration_input(
            InstructionBundleKind::AgentSkill,
            "/workspace/.agents/skills/review/NOT-SKILL.md",
            Some(review_skill()),
        ));

        assert!(agent_document.is_none());
        assert!(skill.is_none());
    }

    #[test]
    fn bundle_registration_requires_the_skill_source_parent() {
        let registration = InstructionBundleRegistration::new(registration_input(
            InstructionBundleKind::AgentSkill,
            "/workspace/.agents/skills/other/SKILL.md",
            Some(review_skill()),
        ));

        assert!(registration.is_none());
    }

    #[test]
    fn configured_root_may_name_one_skill_bundle() {
        let root_path = InstructionPath::try_new(String::from("/workspace/review"))
            .expect("fixture root is valid");
        let registration = InstructionBundleRegistration::new(InstructionBundleRegistrationInput {
            kind: InstructionBundleKind::AgentSkill,
            root_kind: InstructionDiscoveryRootKind::Configured,
            root_path: root_path.clone(),
            source_path: InstructionSourcePath::try_new(
                root_path,
                String::from("/workspace/review/SKILL.md"),
            )
            .expect("fixture source is valid"),
            source_bytes: 1,
            source_hash: InstructionDigest::source_content(b"fixture"),
            skill: Some(review_skill()),
        });

        assert!(registration.is_some());
    }

    #[test]
    fn source_path_bound_keeps_root_and_relative_budgets_independent() {
        let root_text = format!("/{}", "a".repeat(MAX_INSTRUCTION_PATH_BYTES - 1));
        let source_text = format!("{root_text}/AGENTS.md");
        let root_path = InstructionPath::try_new(root_text)
            .expect("the fixture consumes the complete root-path budget");
        let registration = InstructionBundleRegistration::new(InstructionBundleRegistrationInput {
            kind: InstructionBundleKind::AgentDocument,
            root_kind: InstructionDiscoveryRootKind::Configured,
            root_path: root_path.clone(),
            source_path: InstructionSourcePath::try_new(root_path, source_text)
                .expect("a short relative source retains its own budget"),
            source_bytes: 1,
            source_hash: InstructionDigest::source_content(b"fixture"),
            skill: None,
        });

        assert!(registration.is_some());
    }

    #[test]
    fn cloned_paths_share_their_validated_storage() {
        let root =
            InstructionPath::try_new(String::from("/workspace")).expect("fixture root is valid");
        let source =
            InstructionSourcePath::try_new(root.clone(), String::from("/workspace/AGENTS.md"))
                .expect("fixture source is valid");

        let cloned_root = root.clone();
        let cloned_source = source.clone();

        assert!(Arc::ptr_eq(&root.0, &cloned_root.0));
        assert!(Arc::ptr_eq(&root.0, &source.root_path.0));
        assert!(Arc::ptr_eq(
            &source.relative_leaf,
            &cloned_source.relative_leaf
        ));
    }

    #[test]
    fn sibling_source_paths_share_their_root_relative_ancestry() {
        let root =
            InstructionPath::try_new(String::from("/workspace")).expect("fixture root is valid");
        let long_name = "long";
        let common_name = "common";
        let prefix_name = "prefix";
        let first_name = "first";
        let second_name = "second";
        let source_name = "AGENTS.md";
        let first_relative =
            format!("{long_name}/{common_name}/{prefix_name}/{first_name}/{source_name}");
        let second_relative =
            format!("{long_name}/{common_name}/{prefix_name}/{second_name}/{source_name}");
        let mut interner = InstructionSourcePathInterner::new();
        let root_prefix = InstructionSourcePathInterner::root_prefix(root);
        let long = interner
            .append_prefix(&root_prefix, long_name)
            .expect("first common component is valid");
        let common = interner
            .append_prefix(&long, common_name)
            .expect("second common component is valid");
        let common_prefix = interner
            .append_prefix(&common, prefix_name)
            .expect("third common component is valid");
        let first_directory = interner
            .append_prefix(&common_prefix, first_name)
            .expect("first sibling directory is valid");
        let second_directory = interner
            .append_prefix(&common_prefix, second_name)
            .expect("second sibling directory is valid");
        let first =
            InstructionSourcePath::try_new_under(&mut interner, &first_directory, source_name)
                .expect("first sibling source is valid");
        let second =
            InstructionSourcePath::try_new_under(&mut interner, &second_directory, source_name)
                .expect("second sibling source is valid");
        let first_parent = first
            .relative_leaf
            .parent
            .as_ref()
            .expect("the first source has a parent directory");
        let second_parent = second
            .relative_leaf
            .parent
            .as_ref()
            .expect("the second source has a parent directory");
        let first_common_prefix = first_parent
            .parent
            .as_ref()
            .expect("the first sibling has a common prefix");
        let second_common_prefix = second_parent
            .parent
            .as_ref()
            .expect("the second sibling has a common prefix");

        assert_eq!(first.relative_path(), first_relative);
        assert_eq!(second.relative_path(), second_relative);
        assert!(Arc::ptr_eq(first_common_prefix, second_common_prefix));
    }

    #[test]
    fn source_identity_is_independent_of_authorizing_root_segmentation() {
        let workspace_root =
            InstructionPath::try_new(String::from("/workspace")).expect("workspace root is valid");
        let nested_root =
            InstructionPath::try_new(String::from("/workspace/sub")).expect("nested root is valid");
        let from_workspace = InstructionSourcePath::try_new(
            workspace_root,
            String::from("/workspace/sub/AGENTS.md"),
        )
        .expect("workspace source is valid");
        let from_nested =
            InstructionSourcePath::try_new(nested_root, String::from("/workspace/sub/AGENTS.md"))
                .expect("nested source is valid");

        let distinct_sources = HashSet::from([from_workspace, from_nested]);

        assert_eq!(distinct_sources.len(), 1);
    }

    fn review_skill() -> InstructionSkillMetadata {
        InstructionSkillMetadata::try_new(InstructionSkillMetadataInput {
            name: String::from("review"),
            description: String::from("Review one change."),
            parent_directory: String::from("review"),
        })
        .expect("fixture skill is valid")
    }

    fn registration_input(
        kind: InstructionBundleKind,
        source_path: &str,
        skill: Option<InstructionSkillMetadata>,
    ) -> InstructionBundleRegistrationInput {
        let root_path =
            InstructionPath::try_new(String::from("/workspace")).expect("fixture root is valid");
        InstructionBundleRegistrationInput {
            kind,
            root_kind: InstructionDiscoveryRootKind::Workspace,
            root_path: root_path.clone(),
            source_path: InstructionSourcePath::try_new(root_path, source_path.to_owned())
                .expect("fixture source is valid"),
            source_bytes: 1,
            source_hash: InstructionDigest::source_content(b"fixture"),
            skill,
        }
    }
}
