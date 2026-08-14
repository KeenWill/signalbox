//! Deterministic filesystem discovery and registration validation.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::PathBuf,
    time::{Duration, Instant},
};

use serde::Deserialize;
use signalbox_domain::{
    InstructionBundleKind, InstructionBundleRegistration, InstructionDigest,
    InstructionDiscoveryRootKind, InstructionPath, InstructionSkillMetadata,
};

/// One explicit authority root to scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstructionDiscoveryRoot {
    kind: InstructionDiscoveryRootKind,
    path: InstructionPath,
}

impl InstructionDiscoveryRoot {
    pub const fn new(kind: InstructionDiscoveryRootKind, path: InstructionPath) -> Self {
        Self { kind, path }
    }

    pub const fn kind(&self) -> InstructionDiscoveryRootKind {
        self.kind
    }

    pub const fn path(&self) -> &InstructionPath {
        &self.path
    }
}

/// Closed discovery and registration failures retained with a scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstructionDiscoveryFindingKind {
    RootUnavailable,
    EntryUnreadable,
    NonUtf8SourcePath,
    NonUtf8Source,
    InvalidSkill,
    LimitReached(InstructionDiscoveryLimitKind),
}

/// Fixed resource dimension that stopped one otherwise-greedy scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstructionDiscoveryLimitKind {
    ClassifiedEntries,
    Findings,
    CandidateSourceBytes,
    ElapsedTime,
}

/// One visible discovery or registration rejection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstructionDiscoveryFinding {
    path: InstructionPath,
    kind: InstructionDiscoveryFindingKind,
}

impl InstructionDiscoveryFinding {
    pub const fn path(&self) -> &InstructionPath {
        &self.path
    }

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
    pub fn roots(&self) -> &[InstructionDiscoveryRoot] {
        &self.roots
    }

    pub fn bundles(&self) -> &[InstructionBundleRegistration] {
        &self.bundles
    }

    pub fn findings(&self) -> &[InstructionDiscoveryFinding] {
        &self.findings
    }

    pub const fn limit_set_version(&self) -> u16 {
        self.limit_set_version
    }

    pub const fn classified_entries(&self) -> u64 {
        self.classified_entries
    }

    pub const fn candidate_source_bytes(&self) -> u64 {
        self.candidate_source_bytes
    }

    pub const fn elapsed_millis(&self) -> u64 {
        self.elapsed_millis
    }

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
    complete: bool,
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
        complete: true,
    };
    for root in &roots {
        if !walk_root(root, &mut bundles, &mut findings, &mut state) {
            break;
        }
    }
    let mut seen_sources = BTreeSet::new();
    bundles.retain(|bundle| seen_sources.insert((bundle.source_path().clone(), bundle.kind())));
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
        if !check_elapsed(root.path(), findings, state)
            || !inspect_directory(root, &directory, bundles, findings, state)
        {
            return false;
        }
        let entries = match fs::read_dir(&directory) {
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
        let mut children = Vec::new();
        for entry in entries {
            if !consume_entry(root.path(), findings, state) {
                return false;
            }
            match entry {
                Ok(entry) => children.push(entry),
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
        children.sort_by_key(fs::DirEntry::file_name);
        for entry in children.into_iter().rev() {
            match entry.file_type() {
                Ok(file_type) if file_type.is_dir() && !file_type.is_symlink() => {
                    pending.push(entry.path());
                }
                Ok(_) => {}
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
    }
    true
}

fn inspect_directory(
    root: &InstructionDiscoveryRoot,
    directory: &std::path::Path,
    bundles: &mut Vec<InstructionBundleRegistration>,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> bool {
    let agents = directory.join("AGENTS.md");
    if is_regular_no_follow(&agents)
        && !register_file(
            root,
            &agents,
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
    let skill = directory.join("SKILL.md");
    if is_skill && is_regular_no_follow(&skill) {
        let parent = directory.file_name().and_then(|name| name.to_str());
        if !register_file(
            root,
            &skill,
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
    let Some(bundle) = InstructionBundleRegistration::new(
        kind,
        root.kind(),
        root.path().clone(),
        source_path.clone(),
        bytes.len() as u64,
        InstructionDigest::sha256(&bytes),
        skill,
    ) else {
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

fn is_regular_no_follow(path: &std::path::Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
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
    let file = match fs::File::open(source) {
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

fn consume_entry(
    fallback: &InstructionPath,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> bool {
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
    license: Option<String>,
    compatibility: Option<String>,
    metadata: Option<BTreeMap<String, String>>,
    #[serde(rename = "allowed-tools")]
    allowed_tools: Option<String>,
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
    InstructionSkillMetadata::try_new(parsed.name, parsed.description, parent).ok()
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
        let temporary = tempfile::tempdir().expect("temporary root exists");
        let skills = temporary.path().join(".agents/skills/review-rust");
        fs::create_dir_all(&skills).expect("nested skill directory exists");
        fs::write(
            skills.join("SKILL.md"),
            "---\nname: review-rust\ndescription: Review Rust changes.\n---\n# Review\n",
        )
        .expect("skill is written");
        let workspace = temporary.path().canonicalize().expect("root canonicalizes");
        let configured = temporary
            .path()
            .join(".agents/skills")
            .canonicalize()
            .expect("configured root canonicalizes");

        let snapshot = discover_workspace_instructions(vec![
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
        ]);

        assert_eq!(snapshot.roots().len(), 2);
        assert_eq!(snapshot.bundles().len(), 1);
        assert_eq!(
            snapshot.bundles()[0].root_kind(),
            InstructionDiscoveryRootKind::Workspace
        );
    }

    #[test]
    fn entry_limit_stops_an_incomplete_scan_with_typed_evidence() {
        let temporary = tempfile::tempdir().expect("temporary root exists");
        fs::create_dir(temporary.path().join("nested")).expect("nested directory exists");
        let root = workspace_root(&temporary);

        let snapshot =
            discover_with_limits(vec![root], test_limits(0, 4, 64, Duration::from_secs(1)));

        assert!(!snapshot.is_complete());
        assert_eq!(snapshot.classified_entries(), 0);
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
}
