//! Deterministic filesystem discovery and registration validation.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, FileType},
    io::{self, Read},
    path::PathBuf,
    time::{Duration, Instant},
};

use serde::Deserialize;
use signalbox_domain::{
    InstructionBundleKind, InstructionBundleRegistration, InstructionBundleRegistrationInput,
    InstructionDigest, InstructionDiscoveryRootKind, InstructionPath, InstructionSkillMetadata,
    InstructionSkillMetadataInput,
};

/// One explicit authority root to scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstructionDiscoveryRoot {
    kind: InstructionDiscoveryRootKind,
    path: InstructionPath,
}

impl InstructionDiscoveryRoot {
    /// Binds one canonical path to its closed authority route.
    pub const fn new(kind: InstructionDiscoveryRootKind, path: InstructionPath) -> Self {
        Self { kind, path }
    }

    /// Returns the root's authority route.
    pub const fn kind(&self) -> InstructionDiscoveryRootKind {
        self.kind
    }

    /// Borrows the root's canonical absolute path.
    pub const fn path(&self) -> &InstructionPath {
        &self.path
    }
}

/// Closed discovery and registration failures retained with a scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstructionDiscoveryFindingKind {
    /// The declared root could not be opened as a real directory.
    RootUnavailable,
    /// A directory entry or candidate source could not be classified or read.
    EntryUnreadable,
    /// A candidate path could not be represented as canonical UTF-8 evidence.
    NonUtf8SourcePath,
    /// Candidate source bytes were not valid UTF-8.
    NonUtf8Source,
    /// Portable skill frontmatter or registration shape was invalid.
    InvalidSkill,
    /// A fixed discovery resource limit stopped the scan.
    LimitReached(InstructionDiscoveryLimitKind),
}

/// Fixed resource dimension that stopped one otherwise-greedy scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstructionDiscoveryLimitKind {
    /// The scan classified its maximum number of directory entries.
    ClassifiedEntries,
    /// The scan consumed its maximum number of finding records.
    Findings,
    /// The scan consumed its maximum candidate-source byte count.
    CandidateSourceBytes,
    /// The scan reached its maximum wall-clock duration.
    ElapsedTime,
}

/// One visible discovery or registration rejection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstructionDiscoveryFinding {
    path: InstructionPath,
    kind: InstructionDiscoveryFindingKind,
}

impl InstructionDiscoveryFinding {
    /// Borrows the canonical path nearest the failure.
    pub const fn path(&self) -> &InstructionPath {
        &self.path
    }

    /// Returns the closed failure classification.
    pub const fn kind(&self) -> InstructionDiscoveryFindingKind {
        self.kind
    }
}

/// One complete deterministic scan result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstructionDiscoverySnapshot {
    roots: Box<[InstructionDiscoveryRoot]>,
    bundles: Box<[InstructionBundleRegistration]>,
    findings: Box<[InstructionDiscoveryFinding]>,
    limit_set_version: u16,
    classified_entries: u64,
    candidate_source_bytes: u64,
    elapsed_millis: u64,
    complete: bool,
}

impl InstructionDiscoverySnapshot {
    /// Borrows every root in deterministic scan order.
    pub fn roots(&self) -> &[InstructionDiscoveryRoot] {
        &self.roots
    }

    /// Borrows every accepted bundle in deterministic discovery order.
    pub fn bundles(&self) -> &[InstructionBundleRegistration] {
        &self.bundles
    }

    /// Borrows every typed finding in discovery order.
    pub fn findings(&self) -> &[InstructionDiscoveryFinding] {
        &self.findings
    }

    /// Returns the fixed discovery-limit contract version.
    pub const fn limit_set_version(&self) -> u16 {
        self.limit_set_version
    }

    /// Returns the number of directory entries charged to the scan.
    pub const fn classified_entries(&self) -> u64 {
        self.classified_entries
    }

    /// Returns the number of candidate-source bytes charged to the scan.
    pub const fn candidate_source_bytes(&self) -> u64 {
        self.candidate_source_bytes
    }

    /// Returns the observed scan duration rounded down to milliseconds.
    pub const fn elapsed_millis(&self) -> u64 {
        self.elapsed_millis
    }

    /// Reports whether the scan completed before every fixed limit.
    pub const fn is_complete(&self) -> bool {
        self.complete
    }
}

const DISCOVERY_LIMIT_SET_VERSION: u16 = 1;
const MAX_CLASSIFIED_ENTRIES: u64 = 100_000;
const MAX_FINDINGS: usize = 4_096;
const MAX_CANDIDATE_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ELAPSED: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
struct DiscoveryLimits {
    classified_entries: u64,
    findings: usize,
    candidate_source_bytes: u64,
    elapsed: Duration,
}

struct DiscoveryState {
    limits: DiscoveryLimits,
    started: Instant,
    classified_entries: u64,
    candidate_source_bytes: u64,
    seen_sources: BTreeSet<InstructionPath>,
    complete: bool,
}

struct ClassifiedDirectoryEntry {
    entry: fs::DirEntry,
    file_type: FileType,
}

/// Greedily walks every supplied root without following symbolic links.
pub fn discover_workspace_instructions(
    roots: Vec<InstructionDiscoveryRoot>,
) -> InstructionDiscoverySnapshot {
    discover_with_limits(
        roots,
        DiscoveryLimits {
            classified_entries: MAX_CLASSIFIED_ENTRIES,
            findings: MAX_FINDINGS,
            candidate_source_bytes: MAX_CANDIDATE_SOURCE_BYTES,
            elapsed: MAX_ELAPSED,
        },
    )
}

fn discover_with_limits(
    mut roots: Vec<InstructionDiscoveryRoot>,
    limits: DiscoveryLimits,
) -> InstructionDiscoverySnapshot {
    roots.sort_by(|left, right| (left.kind(), left.path()).cmp(&(right.kind(), right.path())));
    let mut bundles = Vec::new();
    let mut findings = Vec::new();
    let mut state = DiscoveryState {
        limits,
        started: Instant::now(),
        classified_entries: 0,
        candidate_source_bytes: 0,
        seen_sources: BTreeSet::new(),
        complete: true,
    };
    for root in &roots {
        if !walk_root(root, &mut bundles, &mut findings, &mut state) {
            break;
        }
    }
    InstructionDiscoverySnapshot {
        roots: roots.into_boxed_slice(),
        bundles: bundles.into_boxed_slice(),
        findings: findings.into_boxed_slice(),
        limit_set_version: DISCOVERY_LIMIT_SET_VERSION,
        classified_entries: state.classified_entries,
        candidate_source_bytes: state.candidate_source_bytes,
        elapsed_millis: u64::try_from(state.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        complete: state.complete,
    }
}

fn walk_root(
    root: &InstructionDiscoveryRoot,
    bundles: &mut Vec<InstructionBundleRegistration>,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> bool {
    let root_path = PathBuf::from(root.path().as_str());
    match fs::symlink_metadata(&root_path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) | Err(_) => {
            return push_finding(
                root.path().clone(),
                InstructionDiscoveryFindingKind::RootUnavailable,
                findings,
                state,
            );
        }
    }
    let mut pending = vec![root_path];
    while let Some(directory) = pending.pop() {
        if !check_elapsed(root.path(), findings, state) {
            return false;
        }
        let directory_entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => {
                if !push_path_finding(
                    &directory,
                    root.path(),
                    InstructionDiscoveryFindingKind::EntryUnreadable,
                    findings,
                    state,
                ) {
                    return false;
                }
                continue;
            }
        };
        let mut entries = Vec::new();
        for entry in directory_entries {
            if !consume_entry(root.path(), findings, state) {
                return false;
            }
            match entry {
                Ok(entry) => entries.push(entry),
                Err(_) => {
                    if !push_path_finding(
                        &directory,
                        root.path(),
                        InstructionDiscoveryFindingKind::EntryUnreadable,
                        findings,
                        state,
                    ) {
                        return false;
                    }
                }
            }
        }
        entries.sort_by_key(fs::DirEntry::file_name);
        let mut classified = Vec::with_capacity(entries.len());
        for entry in entries {
            if !check_elapsed(root.path(), findings, state) {
                return false;
            }
            match entry.file_type() {
                Ok(file_type) => classified.push(ClassifiedDirectoryEntry { entry, file_type }),
                Err(_) => {
                    if !push_path_finding(
                        &entry.path(),
                        root.path(),
                        InstructionDiscoveryFindingKind::EntryUnreadable,
                        findings,
                        state,
                    ) {
                        return false;
                    }
                }
            }
        }
        if !inspect_directory(root, &directory, &classified, bundles, findings, state) {
            return false;
        }
        for classified_entry in classified.into_iter().rev() {
            if !check_elapsed(root.path(), findings, state) {
                return false;
            }
            if classified_entry.file_type.is_dir() {
                pending.push(classified_entry.entry.path());
            }
        }
    }
    true
}

fn inspect_directory(
    root: &InstructionDiscoveryRoot,
    directory: &std::path::Path,
    entries: &[ClassifiedDirectoryEntry],
    bundles: &mut Vec<InstructionBundleRegistration>,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> bool {
    let agents = entries
        .iter()
        .find(|entry| entry.entry.file_name() == "AGENTS.md")
        .filter(|entry| entry.file_type.is_file());
    if let Some(agents) = agents
        && !register_file(
            root,
            &agents.entry.path(),
            InstructionBundleKind::AgentDocument,
            None,
            bundles,
            findings,
            state,
        )
    {
        return false;
    }
    let is_skill = match root.kind() {
        InstructionDiscoveryRootKind::Configured => true,
        InstructionDiscoveryRootKind::Workspace => {
            directory
                .parent()
                .and_then(std::path::Path::file_name)
                .is_some_and(|name| name == "skills")
                && directory
                    .parent()
                    .and_then(std::path::Path::parent)
                    .and_then(std::path::Path::file_name)
                    .is_some_and(|name| name == ".agents")
        }
    };
    let skill = entries
        .iter()
        .find(|entry| entry.entry.file_name() == "SKILL.md")
        .filter(|entry| entry.file_type.is_file());
    if is_skill && let Some(skill) = skill {
        let parent = directory.file_name().and_then(|name| name.to_str());
        if !register_file(
            root,
            &skill.entry.path(),
            InstructionBundleKind::AgentSkill,
            parent,
            bundles,
            findings,
            state,
        ) {
            return false;
        }
    }
    true
}

fn register_file(
    root: &InstructionDiscoveryRoot,
    source: &std::path::Path,
    kind: InstructionBundleKind,
    skill_parent: Option<&str>,
    bundles: &mut Vec<InstructionBundleRegistration>,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> bool {
    let source_path = match source
        .to_str()
        .and_then(|value| InstructionPath::try_new(value.to_owned()).ok())
    {
        Some(path) => path,
        None => {
            return push_path_finding(
                source,
                root.path(),
                InstructionDiscoveryFindingKind::NonUtf8SourcePath,
                findings,
                state,
            );
        }
    };
    if !state.seen_sources.insert(source_path.clone()) {
        return true;
    }
    let bytes = match read_candidate(source, root.path(), findings, state) {
        Ok(bytes) => bytes,
        Err(continue_scan) => return continue_scan,
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => {
            return push_finding(
                source_path,
                InstructionDiscoveryFindingKind::NonUtf8Source,
                findings,
                state,
            );
        }
    };
    let skill = match skill_parent {
        Some(parent) => match parse_skill(text, parent) {
            Some(skill) => Some(skill),
            None => {
                return push_finding(
                    source_path,
                    InstructionDiscoveryFindingKind::InvalidSkill,
                    findings,
                    state,
                );
            }
        },
        None => None,
    };
    let Some(bundle) = InstructionBundleRegistration::new(InstructionBundleRegistrationInput {
        kind,
        root_kind: root.kind(),
        root_path: root.path().clone(),
        source_path: source_path.clone(),
        source_bytes: bytes.len() as u64,
        source_hash: InstructionDigest::sha256(&bytes),
        skill,
    }) else {
        return push_finding(
            source_path,
            InstructionDiscoveryFindingKind::InvalidSkill,
            findings,
            state,
        );
    };
    bundles.push(bundle);
    true
}

fn read_candidate(
    source: &std::path::Path,
    fallback: &InstructionPath,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> Result<Vec<u8>, bool> {
    let remaining = state
        .limits
        .candidate_source_bytes
        .saturating_sub(state.candidate_source_bytes);
    let file = match open_candidate_no_follow(source) {
        Ok(file) => file,
        Err(_) => {
            return Err(push_path_finding(
                source,
                fallback,
                InstructionDiscoveryFindingKind::EntryUnreadable,
                findings,
                state,
            ));
        }
    };
    let mut bytes = Vec::new();
    if file
        .take(remaining.saturating_add(1))
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Err(push_path_finding(
            source,
            fallback,
            InstructionDiscoveryFindingKind::EntryUnreadable,
            findings,
            state,
        ));
    }
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > remaining {
        state.candidate_source_bytes = state.limits.candidate_source_bytes;
        reach_limit(
            path_for_finding(source, fallback),
            InstructionDiscoveryLimitKind::CandidateSourceBytes,
            findings,
            state,
        );
        return Err(false);
    }
    state.candidate_source_bytes += u64::try_from(bytes.len()).unwrap_or(remaining);
    if !check_elapsed(fallback, findings, state) {
        return Err(false);
    }
    Ok(bytes)
}

#[cfg(unix)]
fn open_candidate_no_follow(source: &std::path::Path) -> io::Result<fs::File> {
    use rustix::fs::{Mode, OFlags, open};

    let descriptor = open(
        source,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    let file = fs::File::from(descriptor);
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(
            "instruction candidate is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_candidate_no_follow(_source: &std::path::Path) -> io::Result<fs::File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no-follow instruction reads are unavailable on this platform",
    ))
}

fn consume_entry(
    fallback: &InstructionPath,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> bool {
    if !check_elapsed(fallback, findings, state) {
        return false;
    }
    if state.classified_entries >= state.limits.classified_entries {
        return reach_limit(
            fallback.clone(),
            InstructionDiscoveryLimitKind::ClassifiedEntries,
            findings,
            state,
        );
    }
    state.classified_entries += 1;
    true
}

fn check_elapsed(
    fallback: &InstructionPath,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> bool {
    if state.started.elapsed() >= state.limits.elapsed {
        return reach_limit(
            fallback.clone(),
            InstructionDiscoveryLimitKind::ElapsedTime,
            findings,
            state,
        );
    }
    true
}

fn push_finding(
    path: InstructionPath,
    kind: InstructionDiscoveryFindingKind,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> bool {
    if findings.len() >= state.limits.findings.saturating_sub(1) {
        return reach_limit(
            path,
            InstructionDiscoveryLimitKind::Findings,
            findings,
            state,
        );
    }
    findings.push(InstructionDiscoveryFinding { path, kind });
    true
}

fn push_path_finding(
    path: &std::path::Path,
    fallback: &InstructionPath,
    kind: InstructionDiscoveryFindingKind,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> bool {
    push_finding(path_for_finding(path, fallback), kind, findings, state)
}

fn path_for_finding(path: &std::path::Path, fallback: &InstructionPath) -> InstructionPath {
    path.to_str()
        .and_then(|value| InstructionPath::try_new(value.to_owned()).ok())
        .unwrap_or_else(|| fallback.clone())
}

fn reach_limit(
    path: InstructionPath,
    limit: InstructionDiscoveryLimitKind,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> bool {
    state.complete = false;
    findings.truncate(state.limits.findings.saturating_sub(1));
    if state.limits.findings > 0 {
        findings.push(InstructionDiscoveryFinding {
            path,
            kind: InstructionDiscoveryFindingKind::LimitReached(limit),
        });
    }
    false
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PortableSkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    license: OptionalFrontmatterField<String>,
    #[serde(default)]
    compatibility: OptionalFrontmatterField<String>,
    #[serde(default)]
    metadata: OptionalFrontmatterField<BTreeMap<String, String>>,
    #[serde(default, rename = "allowed-tools")]
    allowed_tools: OptionalFrontmatterField<String>,
}

#[derive(Default)]
enum OptionalFrontmatterField<T> {
    #[default]
    Missing,
    Present(T),
}

impl<'de, T> Deserialize<'de> for OptionalFrontmatterField<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let value = serde_yaml_ng::Value::deserialize(deserializer)?;
        if matches!(value, serde_yaml_ng::Value::Null) {
            return Err(serde::de::Error::custom(
                "an optional skill field cannot be null",
            ));
        }
        T::deserialize(value)
            .map(Self::Present)
            .map_err(serde::de::Error::custom)
    }
}

impl<T> OptionalFrontmatterField<T> {
    fn as_ref(&self) -> Option<&T> {
        match self {
            Self::Missing => None,
            Self::Present(value) => Some(value),
        }
    }
}

fn parse_skill(text: &str, parent: &str) -> Option<InstructionSkillMetadata> {
    let body = text.strip_prefix("---\n")?;
    let boundary = body.find("\n---\n")?;
    let parsed: PortableSkillFrontmatter = serde_yaml_ng::from_str(&body[..boundary]).ok()?;
    if parsed
        .compatibility
        .as_ref()
        .is_some_and(|value| value.is_empty() || value.chars().count() > 500)
        || parsed.license.as_ref().is_some_and(String::is_empty)
    {
        return None;
    }
    let _portable_optional_fields = (parsed.metadata, parsed.allowed_tools);
    InstructionSkillMetadata::try_new(InstructionSkillMetadataInput {
        name: parsed.name,
        description: parsed.description,
        parent_directory: parent.to_owned(),
    })
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greedy_discovery_finds_nested_documents_and_skills() {
        let temporary = tempfile::tempdir().expect("temporary root exists");
        let nested = temporary.path().join("apps/api");
        let skill = nested.join(".agents/skills/review-rust");
        fs::create_dir_all(&skill).expect("nested skill directory exists");
        fs::write(nested.join("AGENTS.md"), "# API rules\n").expect("agent document is written");
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: review-rust\ndescription: Review Rust changes.\n---\n# Review\n",
        )
        .expect("skill is written");
        let canonical = temporary.path().canonicalize().expect("root canonicalizes");
        let root = InstructionDiscoveryRoot::new(
            InstructionDiscoveryRootKind::Workspace,
            InstructionPath::try_new(canonical.to_string_lossy().into_owned())
                .expect("path is valid"),
        );

        let snapshot = discover_workspace_instructions(vec![root]);

        assert_eq!(snapshot.bundles().len(), 2);
        assert!(snapshot.findings().is_empty());
        assert_eq!(
            snapshot.bundles()[0].kind(),
            InstructionBundleKind::AgentDocument
        );
        assert_eq!(
            snapshot.bundles()[1].kind(),
            InstructionBundleKind::AgentSkill
        );
    }

    #[cfg(unix)]
    #[test]
    fn discovery_does_not_follow_a_skill_symlink() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary root exists");
        let outside = tempfile::tempdir().expect("outside root exists");
        fs::write(outside.path().join("SKILL.md"), "not admitted").expect("outside file exists");
        let skills = temporary.path().join(".agents/skills");
        fs::create_dir_all(&skills).expect("skills root exists");
        symlink(outside.path(), skills.join("linked")).expect("skill link exists");
        let canonical = temporary.path().canonicalize().expect("root canonicalizes");
        let root = InstructionDiscoveryRoot::new(
            InstructionDiscoveryRootKind::Workspace,
            InstructionPath::try_new(canonical.to_string_lossy().into_owned())
                .expect("path is valid"),
        );

        let snapshot = discover_workspace_instructions(vec![root]);

        assert!(snapshot.bundles().is_empty());
    }

    #[test]
    fn overlapping_roots_emit_one_registration_for_the_same_source() {
        let source = "---\nname: review-rust\ndescription: Review Rust changes.\n---\n# Review\n";
        let temporary = tempfile::tempdir().expect("temporary root exists");
        let skills = temporary.path().join(".agents/skills/review-rust");
        fs::create_dir_all(&skills).expect("nested skill directory exists");
        fs::write(skills.join("SKILL.md"), source).expect("skill is written");
        let workspace = temporary.path().canonicalize().expect("root canonicalizes");
        let configured = temporary
            .path()
            .join(".agents/skills")
            .canonicalize()
            .expect("configured root canonicalizes");

        let snapshot = discover_with_limits(
            vec![
                InstructionDiscoveryRoot::new(
                    InstructionDiscoveryRootKind::Configured,
                    InstructionPath::try_new(configured.to_string_lossy().into_owned())
                        .expect("configured path is valid"),
                ),
                InstructionDiscoveryRoot::new(
                    InstructionDiscoveryRootKind::Workspace,
                    InstructionPath::try_new(workspace.to_string_lossy().into_owned())
                        .expect("workspace path is valid"),
                ),
            ],
            test_limits(100, 4, source.len() as u64, Duration::from_secs(1)),
        );

        assert_eq!(snapshot.roots().len(), 2);
        assert_eq!(snapshot.bundles().len(), 1);
        assert!(snapshot.is_complete());
        assert_eq!(snapshot.candidate_source_bytes(), source.len() as u64);
        assert_eq!(
            snapshot.bundles()[0].root_kind(),
            InstructionDiscoveryRootKind::Workspace
        );
    }

    #[test]
    fn entry_limit_stops_an_incomplete_scan_with_typed_evidence() {
        let temporary = tempfile::tempdir().expect("temporary root exists");
        fs::write(temporary.path().join("AGENTS.md"), "not admitted").expect("candidate exists");
        let root = workspace_root(&temporary);

        let snapshot =
            discover_with_limits(vec![root], test_limits(0, 4, 64, Duration::from_secs(1)));

        assert!(!snapshot.is_complete());
        assert_eq!(snapshot.classified_entries(), 0);
        assert!(snapshot.bundles().is_empty());
        assert_eq!(snapshot.findings().len(), 1);
        assert_eq!(
            snapshot.findings()[0].kind(),
            InstructionDiscoveryFindingKind::LimitReached(
                InstructionDiscoveryLimitKind::ClassifiedEntries
            )
        );
    }

    #[test]
    fn candidate_byte_limit_bounds_the_source_read() {
        let temporary = tempfile::tempdir().expect("temporary root exists");
        fs::write(temporary.path().join("AGENTS.md"), "too large")
            .expect("agent document is written");
        let root = workspace_root(&temporary);

        let snapshot =
            discover_with_limits(vec![root], test_limits(4, 4, 1, Duration::from_secs(1)));

        assert!(!snapshot.is_complete());
        assert_eq!(snapshot.candidate_source_bytes(), 1);
        assert!(snapshot.bundles().is_empty());
        assert_eq!(
            snapshot.findings()[0].kind(),
            InstructionDiscoveryFindingKind::LimitReached(
                InstructionDiscoveryLimitKind::CandidateSourceBytes
            )
        );
    }

    #[test]
    fn elapsed_limit_stops_before_candidate_inspection() {
        let temporary = tempfile::tempdir().expect("temporary root exists");
        fs::write(temporary.path().join("AGENTS.md"), "not read")
            .expect("agent document is written");
        let root = workspace_root(&temporary);

        let snapshot = discover_with_limits(vec![root], test_limits(4, 4, 64, Duration::ZERO));

        assert!(!snapshot.is_complete());
        assert!(snapshot.bundles().is_empty());
        assert_eq!(
            snapshot.findings()[0].kind(),
            InstructionDiscoveryFindingKind::LimitReached(
                InstructionDiscoveryLimitKind::ElapsedTime
            )
        );
    }

    #[test]
    fn finding_limit_reserves_the_terminal_evidence_slot() {
        let temporary = tempfile::tempdir().expect("temporary root exists");
        let missing = temporary.path().join("missing");
        let root = InstructionDiscoveryRoot::new(
            InstructionDiscoveryRootKind::Workspace,
            InstructionPath::try_new(missing.to_string_lossy().into_owned())
                .expect("missing path is valid"),
        );

        let snapshot =
            discover_with_limits(vec![root], test_limits(4, 1, 64, Duration::from_secs(1)));

        assert!(!snapshot.is_complete());
        assert_eq!(snapshot.findings().len(), 1);
        assert_eq!(
            snapshot.findings()[0].kind(),
            InstructionDiscoveryFindingKind::LimitReached(InstructionDiscoveryLimitKind::Findings)
        );
    }

    #[cfg(unix)]
    #[test]
    fn candidate_open_refuses_a_symlink_after_directory_classification() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary root exists");
        let outside = temporary.path().join("outside");
        let candidate = temporary.path().join("AGENTS.md");
        fs::write(&outside, "outside instructions").expect("outside source exists");
        symlink(&outside, &candidate).expect("candidate link exists");
        let root = workspace_root(&temporary);
        let mut findings = Vec::new();
        let mut state = test_state(test_limits(4, 4, 64, Duration::from_secs(1)));

        let result = read_candidate(&candidate, root.path(), &mut findings, &mut state);

        assert!(result.is_err());
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].kind(),
            InstructionDiscoveryFindingKind::EntryUnreadable
        );
    }

    #[test]
    fn explicit_null_portable_skill_fields_are_invalid() {
        let license = "---\nname: review-rust\ndescription: Review.\nlicense: null\n---\n";
        let compatibility =
            "---\nname: review-rust\ndescription: Review.\ncompatibility: null\n---\n";
        let metadata = "---\nname: review-rust\ndescription: Review.\nmetadata: null\n---\n";
        let allowed_tools =
            "---\nname: review-rust\ndescription: Review.\nallowed-tools: null\n---\n";

        assert!(parse_skill(license, "review-rust").is_none());
        assert!(parse_skill(compatibility, "review-rust").is_none());
        assert!(parse_skill(metadata, "review-rust").is_none());
        assert!(parse_skill(allowed_tools, "review-rust").is_none());
    }

    fn workspace_root(temporary: &tempfile::TempDir) -> InstructionDiscoveryRoot {
        let canonical = temporary.path().canonicalize().expect("root canonicalizes");
        InstructionDiscoveryRoot::new(
            InstructionDiscoveryRootKind::Workspace,
            InstructionPath::try_new(canonical.to_string_lossy().into_owned())
                .expect("path is valid"),
        )
    }

    const fn test_limits(
        classified_entries: u64,
        findings: usize,
        candidate_source_bytes: u64,
        elapsed: Duration,
    ) -> DiscoveryLimits {
        DiscoveryLimits {
            classified_entries,
            findings,
            candidate_source_bytes,
            elapsed,
        }
    }

    fn test_state(limits: DiscoveryLimits) -> DiscoveryState {
        DiscoveryState {
            limits,
            started: Instant::now(),
            classified_entries: 0,
            candidate_source_bytes: 0,
            seen_sources: BTreeSet::new(),
            complete: true,
        }
    }
}
