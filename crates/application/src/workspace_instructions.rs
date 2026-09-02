//! Deterministic filesystem discovery and registration validation.

use std::{
    collections::HashSet,
    path::PathBuf,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read},
    os::{fd::OwnedFd, unix::ffi::OsStrExt},
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Condvar, LazyLock, Mutex, mpsc},
    thread::JoinHandle,
};

use serde::Deserialize;
use signalbox_domain::{
    InstructionBundleKind, InstructionBundleRegistration, InstructionBundleRegistrationInput,
    InstructionDigest, InstructionDiscoveryRootKind, InstructionPath, InstructionSkillMetadata,
    InstructionSkillMetadataInput, InstructionSourcePath, InstructionSourcePathInterner,
    InstructionSourcePathPrefix,
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

// numeric-bound: not-a-bound - names which fixed discovery-limit set was applied
const DISCOVERY_LIMIT_SET_VERSION: u16 = 2;
#[cfg(unix)]
const VCS_METADATA_DIRECTORIES: [&str; 4] = [".git", ".hg", ".svn", ".jj"];
#[cfg(unix)]
const BUILD_AND_DEPENDENCY_DIRECTORIES: [&str; 5] =
    ["target", "node_modules", ".venv", "dist", "build"];
// numeric-bound: guard - prevents a pathological workspace tree from walking the daemon forever
const MAX_CLASSIFIED_ENTRIES: u64 = 100_000;
// numeric-bound: guard - prevents an unusable workspace from exhausting memory with findings
const MAX_FINDINGS: usize = 4_096;
// numeric-bound: guard - prevents a runaway instruction tree from exhausting daemon memory
const MAX_CANDIDATE_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
// numeric-bound: guard - prevents a slow or adversarial filesystem from stalling discovery forever
const MAX_ELAPSED: Duration = Duration::from_secs(30);
#[cfg(unix)]
// numeric-bound: guard - prevents concurrent scans from exhausting the blocking thread pool
const MAX_FILESYSTEM_WORKERS: usize = 4;

#[cfg(unix)]
static FILESYSTEM_WORKERS: LazyLock<Arc<FilesystemWorkerRegistry>> =
    LazyLock::new(|| Arc::new(FilesystemWorkerRegistry::default()));

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
    source_paths: InstructionSourcePathInterner,
    seen_sources: HashSet<InstructionSourcePath>,
    complete: bool,
}

#[cfg(unix)]
#[derive(Debug)]
struct ClassifiedDirectoryEntry {
    name: OsString,
    file_type: rustix::fs::FileType,
}

#[cfg(unix)]
struct PendingDirectory {
    parent: Option<usize>,
    name: Option<OsString>,
    source_prefix: Result<InstructionSourcePathPrefix, InstructionDiscoveryFindingKind>,
}

#[cfg(unix)]
struct DirectoryRead {
    entries: Vec<Result<ClassifiedDirectoryEntry, OsString>>,
    read_errors: u64,
    entry_limit_exceeded: bool,
}

#[cfg(unix)]
struct CandidateRead {
    bytes: Vec<u8>,
    source_hash: InstructionDigest,
    is_utf8: bool,
}

#[cfg(unix)]
impl CandidateRead {
    fn new(bytes: Vec<u8>) -> Self {
        let source_hash = InstructionDigest::source_content(&bytes);
        let is_utf8 = std::str::from_utf8(&bytes).is_ok();
        Self {
            bytes,
            source_hash,
            is_utf8,
        }
    }
}

#[cfg(unix)]
#[derive(Default)]
struct FilesystemWorkerRegistry {
    state: Mutex<FilesystemWorkerState>,
    available: Condvar,
}

#[cfg(unix)]
#[derive(Default)]
struct FilesystemWorkerState {
    workers: Vec<JoinHandle<()>>,
    running: usize,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FilesystemTaskError {
    Deadline,
    Unavailable,
}

#[cfg(unix)]
enum SkillParseResult {
    Parsed(InstructionSkillMetadata),
    NonUtf8,
    Invalid,
}

#[cfg(unix)]
struct FilesystemWorkerCompletion {
    registry: Arc<FilesystemWorkerRegistry>,
}

#[cfg(unix)]
struct CountingReader<Source> {
    source: Source,
    observed: Arc<AtomicU64>,
}

#[cfg(unix)]
impl<Source: Read> Read for CountingReader<Source> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.source.read(buffer)?;
        self.observed.fetch_add(read as u64, Ordering::Relaxed);
        Ok(read)
    }
}

#[cfg(unix)]
impl Drop for FilesystemWorkerCompletion {
    fn drop(&mut self) {
        if let Ok(mut state) = self.registry.state.lock() {
            state.running = state.running.saturating_sub(1);
            self.registry.available.notify_one();
        }
    }
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct CandidateLocation<'a> {
    directory_descriptor: &'a OwnedFd,
    directory: &'a std::path::Path,
    directory_prefix: Result<&'a InstructionSourcePathPrefix, InstructionDiscoveryFindingKind>,
    source_name: &'a OsStr,
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct DirectoryLocation<'a> {
    descriptor: &'a OwnedFd,
    absolute: &'a std::path::Path,
    relative: &'a std::path::Path,
    source_prefix: Result<&'a InstructionSourcePathPrefix, InstructionDiscoveryFindingKind>,
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
        source_paths: InstructionSourcePathInterner::new(),
        seen_sources: HashSet::new(),
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

#[cfg(unix)]
fn walk_root(
    root: &InstructionDiscoveryRoot,
    bundles: &mut Vec<InstructionBundleRegistration>,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> bool {
    let root_path = PathBuf::from(root.path().as_str());
    let root_deadline = state.started + state.limits.elapsed;
    let root_descriptor =
        match open_directory_no_follow_before_deadline(root_path.clone(), root_deadline) {
            Ok(Ok(Some(descriptor))) => descriptor,
            Ok(Ok(None)) | Err(FilesystemTaskError::Deadline) => {
                return reach_limit(
                    root.path().clone(),
                    InstructionDiscoveryLimitKind::ElapsedTime,
                    findings,
                    state,
                );
            }
            Ok(Err(_)) | Err(FilesystemTaskError::Unavailable) => {
                return push_finding(
                    root.path().clone(),
                    InstructionDiscoveryFindingKind::RootUnavailable,
                    findings,
                    state,
                );
            }
        };
    let mut directories = vec![PendingDirectory {
        parent: None,
        name: None,
        source_prefix: Ok(InstructionSourcePathInterner::root_prefix(
            root.path().clone(),
        )),
    }];
    let mut pending = vec![0_usize];
    while let Some(current_directory) = pending.pop() {
        let is_root = directories[current_directory].name.is_none();
        let relative_directory = pending_directory_path(current_directory, &directories);
        let directory = root_path.join(&relative_directory);
        if !check_elapsed(root.path(), findings, state) {
            return false;
        }
        let directory_deadline = state.started + state.limits.elapsed;
        let directory_descriptor = match open_directory_beneath_before_deadline(
            &root_descriptor,
            relative_directory.clone(),
            directory_deadline,
        ) {
            Ok(Ok(Some(descriptor))) => descriptor,
            Ok(Ok(None)) | Err(FilesystemTaskError::Deadline) => {
                return reach_limit(
                    root.path().clone(),
                    InstructionDiscoveryLimitKind::ElapsedTime,
                    findings,
                    state,
                );
            }
            Ok(Err(_)) | Err(FilesystemTaskError::Unavailable) => {
                let kind = if is_root {
                    InstructionDiscoveryFindingKind::RootUnavailable
                } else {
                    InstructionDiscoveryFindingKind::EntryUnreadable
                };
                if !push_path_finding(&directory, root.path(), kind, findings, state) {
                    return false;
                }
                continue;
            }
        };
        if !is_root && root.kind() == InstructionDiscoveryRootKind::Workspace {
            let metadata_deadline = state.started + state.limits.elapsed;
            match contains_vcs_metadata_before_deadline(&directory_descriptor, metadata_deadline) {
                Ok(Ok(true)) => continue,
                Ok(Ok(false)) => {}
                Ok(Err(_)) | Err(FilesystemTaskError::Unavailable) => {
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
                Err(FilesystemTaskError::Deadline) => {
                    return reach_limit(
                        root.path().clone(),
                        InstructionDiscoveryLimitKind::ElapsedTime,
                        findings,
                        state,
                    );
                }
            }
        }
        let remaining_entries = state
            .limits
            .classified_entries
            .saturating_sub(state.classified_entries);
        let deadline = state.started + state.limits.elapsed;
        let classified = Arc::new(AtomicU64::new(0));
        let directory_read = match read_directory_before_deadline(
            &directory_descriptor,
            remaining_entries,
            deadline,
            Arc::clone(&classified),
        ) {
            Ok(Ok(directory_read)) => directory_read,
            Ok(Err(_)) | Err(FilesystemTaskError::Unavailable) => {
                let kind = if is_root {
                    InstructionDiscoveryFindingKind::RootUnavailable
                } else {
                    InstructionDiscoveryFindingKind::EntryUnreadable
                };
                if !push_path_finding(&directory, root.path(), kind, findings, state) {
                    return false;
                }
                continue;
            }
            Err(FilesystemTaskError::Deadline) => {
                state.classified_entries +=
                    classified.load(Ordering::Relaxed).min(remaining_entries);
                return reach_limit(
                    root.path().clone(),
                    InstructionDiscoveryLimitKind::ElapsedTime,
                    findings,
                    state,
                );
            }
        };
        state.classified_entries += classified.load(Ordering::Relaxed).min(remaining_entries);
        let mut entries = Vec::new();
        for entry in directory_read.entries {
            if !check_elapsed(root.path(), findings, state) {
                return false;
            }
            match entry {
                Ok(entry) => entries.push(entry),
                Err(name) => {
                    if !push_path_finding(
                        &directory.join(name),
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
        for _read_error in 0..directory_read.read_errors {
            if !check_elapsed(root.path(), findings, state)
                || !push_path_finding(
                    &directory,
                    root.path(),
                    InstructionDiscoveryFindingKind::EntryUnreadable,
                    findings,
                    state,
                )
            {
                return false;
            }
        }
        if directory_read.entry_limit_exceeded {
            return reach_limit(
                root.path().clone(),
                InstructionDiscoveryLimitKind::ClassifiedEntries,
                findings,
                state,
            );
        }
        if !inspect_directory(
            root,
            DirectoryLocation {
                descriptor: &directory_descriptor,
                absolute: &directory,
                relative: &relative_directory,
                source_prefix: directories[current_directory]
                    .source_prefix
                    .as_ref()
                    .map_err(|kind| *kind),
            },
            &entries,
            bundles,
            findings,
            state,
        ) {
            return false;
        }
        for classified_entry in entries.into_iter().rev() {
            if !check_elapsed(root.path(), findings, state) {
                return false;
            }
            if classified_entry.file_type == rustix::fs::FileType::Directory
                && !is_excluded_directory_name(&classified_entry.name)
            {
                let source_prefix = match directories[current_directory].source_prefix.as_ref() {
                    Ok(prefix) => classified_entry
                        .name
                        .to_str()
                        .ok_or(InstructionDiscoveryFindingKind::NonUtf8SourcePath)
                        .and_then(|name| {
                            state
                                .source_paths
                                .append_prefix(prefix, name)
                                .map_err(|_| InstructionDiscoveryFindingKind::EntryUnreadable)
                        }),
                    Err(kind) => Err(*kind),
                };
                let child = directories.len();
                directories.push(PendingDirectory {
                    parent: Some(current_directory),
                    name: Some(classified_entry.name),
                    source_prefix,
                });
                pending.push(child);
            }
        }
    }
    true
}

#[cfg(unix)]
fn is_excluded_directory_name(name: &OsStr) -> bool {
    VCS_METADATA_DIRECTORIES
        .iter()
        .chain(BUILD_AND_DEPENDENCY_DIRECTORIES.iter())
        .any(|excluded| name == OsStr::new(excluded))
}

#[cfg(unix)]
fn contains_vcs_metadata_before_deadline(
    directory: &OwnedFd,
    deadline: Instant,
) -> Result<io::Result<bool>, FilesystemTaskError> {
    let directory = rustix::io::dup(directory).map_err(|_| FilesystemTaskError::Unavailable)?;
    run_bounded_filesystem_task(
        Arc::clone(&FILESYSTEM_WORKERS),
        MAX_FILESYSTEM_WORKERS,
        deadline,
        "signalbox-instruction-repository-probe",
        move || contains_vcs_metadata(&directory),
    )
}

#[cfg(unix)]
fn contains_vcs_metadata(directory: &OwnedFd) -> io::Result<bool> {
    use rustix::fs::{AtFlags, FileType, statat};

    for name in VCS_METADATA_DIRECTORIES {
        match statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(status) if FileType::from_raw_mode(status.st_mode) == FileType::Directory => {
                return Ok(true);
            }
            Ok(_) | Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

#[cfg(unix)]
fn pending_directory_path(directory: usize, directories: &[PendingDirectory]) -> PathBuf {
    let mut names = Vec::new();
    let mut current = Some(directory);
    while let Some(index) = current {
        let component = &directories[index];
        if let Some(name) = &component.name {
            names.push(name);
        }
        current = component.parent;
    }
    names.reverse();
    names.into_iter().collect()
}

#[cfg(not(unix))]
fn walk_root(
    root: &InstructionDiscoveryRoot,
    _bundles: &mut Vec<InstructionBundleRegistration>,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> bool {
    push_finding(
        root.path().clone(),
        InstructionDiscoveryFindingKind::RootUnavailable,
        findings,
        state,
    )
}

#[cfg(unix)]
fn inspect_directory(
    root: &InstructionDiscoveryRoot,
    location: DirectoryLocation<'_>,
    entries: &[ClassifiedDirectoryEntry],
    bundles: &mut Vec<InstructionBundleRegistration>,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> bool {
    let agents = entries
        .iter()
        .find(|entry| entry.name == OsStr::new("AGENTS.md"))
        .filter(|entry| entry.file_type == rustix::fs::FileType::RegularFile);
    if let Some(agents) = agents
        && !register_file(
            root,
            CandidateLocation {
                directory_descriptor: location.descriptor,
                directory: location.absolute,
                directory_prefix: location.source_prefix,
                source_name: &agents.name,
            },
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
            location
                .relative
                .parent()
                .and_then(std::path::Path::file_name)
                .is_some_and(|name| name == "skills")
                && location
                    .relative
                    .parent()
                    .and_then(std::path::Path::parent)
                    .and_then(std::path::Path::file_name)
                    .is_some_and(|name| name == ".agents")
        }
    };
    let skill = entries
        .iter()
        .find(|entry| entry.name == OsStr::new("SKILL.md"))
        .filter(|entry| entry.file_type == rustix::fs::FileType::RegularFile);
    if is_skill && let Some(skill) = skill {
        let parent = location.absolute.file_name().and_then(|name| name.to_str());
        if !register_file(
            root,
            CandidateLocation {
                directory_descriptor: location.descriptor,
                directory: location.absolute,
                directory_prefix: location.source_prefix,
                source_name: &skill.name,
            },
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

#[cfg(unix)]
fn register_file(
    root: &InstructionDiscoveryRoot,
    location: CandidateLocation<'_>,
    kind: InstructionBundleKind,
    skill_parent: Option<&str>,
    bundles: &mut Vec<InstructionBundleRegistration>,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> bool {
    let source = location.directory.join(location.source_name);
    let source_path = match source_path_for_registration(
        location.directory_prefix,
        location.source_name,
        &mut state.source_paths,
    ) {
        Ok(path) => path,
        Err(kind) => {
            return push_path_finding(&source, root.path(), kind, findings, state);
        }
    };
    if !state.seen_sources.insert(source_path.clone()) {
        return true;
    }
    let candidate = match read_candidate(location, &source, root.path(), findings, state) {
        Ok(candidate) => candidate,
        Err(continue_scan) => return continue_scan,
    };
    let CandidateRead {
        bytes,
        source_hash,
        is_utf8,
    } = candidate;
    let source_bytes = bytes.len() as u64;
    let skill = match skill_parent {
        Some(parent) => match parse_skill_before_deadline(
            bytes,
            parent.to_owned(),
            state.started + state.limits.elapsed,
        ) {
            Ok(SkillParseResult::Parsed(skill)) => Some(skill),
            Ok(SkillParseResult::NonUtf8) => {
                return push_path_finding(
                    &source,
                    root.path(),
                    InstructionDiscoveryFindingKind::NonUtf8Source,
                    findings,
                    state,
                );
            }
            Ok(SkillParseResult::Invalid) => {
                return push_path_finding(
                    &source,
                    root.path(),
                    InstructionDiscoveryFindingKind::InvalidSkill,
                    findings,
                    state,
                );
            }
            Err(FilesystemTaskError::Deadline) => {
                return reach_limit(
                    path_for_finding(&source, root.path()),
                    InstructionDiscoveryLimitKind::ElapsedTime,
                    findings,
                    state,
                );
            }
            Err(FilesystemTaskError::Unavailable) => {
                return push_path_finding(
                    &source,
                    root.path(),
                    InstructionDiscoveryFindingKind::EntryUnreadable,
                    findings,
                    state,
                );
            }
        },
        None => {
            if !is_utf8 {
                return push_path_finding(
                    &source,
                    root.path(),
                    InstructionDiscoveryFindingKind::NonUtf8Source,
                    findings,
                    state,
                );
            }
            None
        }
    };
    let Some(bundle) = InstructionBundleRegistration::new(InstructionBundleRegistrationInput {
        kind,
        root_kind: root.kind(),
        root_path: root.path().clone(),
        source_path: source_path.clone(),
        source_bytes,
        source_hash,
        skill,
    }) else {
        return push_path_finding(
            &source,
            root.path(),
            InstructionDiscoveryFindingKind::InvalidSkill,
            findings,
            state,
        );
    };
    bundles.push(bundle);
    true
}

#[cfg(unix)]
fn parse_skill_before_deadline(
    bytes: Vec<u8>,
    parent: String,
    deadline: Instant,
) -> Result<SkillParseResult, FilesystemTaskError> {
    run_bounded_filesystem_task(
        Arc::clone(&FILESYSTEM_WORKERS),
        MAX_FILESYSTEM_WORKERS,
        deadline,
        "signalbox-instruction-skill-parse",
        move || {
            let Ok(text) = std::str::from_utf8(&bytes) else {
                return SkillParseResult::NonUtf8;
            };
            match parse_skill(text, &parent) {
                Some(skill) => SkillParseResult::Parsed(skill),
                None => SkillParseResult::Invalid,
            }
        },
    )
}

fn source_path_for_registration(
    directory: Result<&InstructionSourcePathPrefix, InstructionDiscoveryFindingKind>,
    source_name: &OsStr,
    interner: &mut InstructionSourcePathInterner,
) -> Result<InstructionSourcePath, InstructionDiscoveryFindingKind> {
    let directory = directory?;
    let source_name = source_name
        .to_str()
        .ok_or(InstructionDiscoveryFindingKind::NonUtf8SourcePath)?;
    InstructionSourcePath::try_new_under(interner, directory, source_name)
        .map_err(|_| InstructionDiscoveryFindingKind::EntryUnreadable)
}

#[cfg(unix)]
fn read_directory_before_deadline(
    directory: &OwnedFd,
    remaining_entries: u64,
    deadline: Instant,
    classified: Arc<AtomicU64>,
) -> Result<io::Result<DirectoryRead>, FilesystemTaskError> {
    let directory = rustix::io::dup(directory).map_err(|_| FilesystemTaskError::Unavailable)?;
    run_bounded_filesystem_task(
        Arc::clone(&FILESYSTEM_WORKERS),
        MAX_FILESYSTEM_WORKERS,
        deadline,
        "signalbox-instruction-directory",
        move || read_directory(directory, remaining_entries, classified),
    )
}

#[cfg(unix)]
fn read_directory(
    directory: OwnedFd,
    remaining_entries: u64,
    classified: Arc<AtomicU64>,
) -> io::Result<DirectoryRead> {
    let mut directory_entries = rustix::fs::Dir::read_from(&directory)?;
    let mut names = Vec::new();
    let mut read_errors = 0_u64;
    let mut observations = 0_u64;
    let mut entry_limit_exceeded = false;
    while let Some(entry) = directory_entries.read() {
        match entry {
            Ok(entry) => {
                let name = OsStr::from_bytes(entry.file_name().to_bytes());
                if name == OsStr::new(".") || name == OsStr::new("..") {
                    continue;
                }
                if observations >= remaining_entries {
                    entry_limit_exceeded = true;
                    break;
                }
                observations += 1;
                names.push(name.to_os_string());
            }
            Err(_) => {
                if observations >= remaining_entries {
                    entry_limit_exceeded = true;
                    break;
                }
                read_errors += 1;
                classified.fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }
    if entry_limit_exceeded {
        classified.store(0, Ordering::Relaxed);
        return Ok(DirectoryRead {
            entries: Vec::new(),
            read_errors: 0,
            entry_limit_exceeded,
        });
    }
    let entries = classify_sorted_names(
        names,
        |name| classify_entry_at(&directory, name),
        || {
            classified.fetch_add(1, Ordering::Relaxed);
        },
    );
    Ok(DirectoryRead {
        entries,
        read_errors,
        entry_limit_exceeded,
    })
}

#[cfg(unix)]
fn classify_sorted_names(
    mut names: Vec<OsString>,
    mut classify: impl FnMut(&OsStr) -> io::Result<rustix::fs::FileType>,
    mut classified: impl FnMut(),
) -> Vec<Result<ClassifiedDirectoryEntry, OsString>> {
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let result = classify(&name)
                .map(|file_type| ClassifiedDirectoryEntry {
                    name: name.clone(),
                    file_type,
                })
                .map_err(|_| name);
            classified();
            result
        })
        .collect()
}

#[cfg(unix)]
fn run_bounded_filesystem_task<T: Send + 'static>(
    registry: Arc<FilesystemWorkerRegistry>,
    max_workers: usize,
    deadline: Instant,
    worker_name: &'static str,
    task: impl FnOnce() -> T + Send + 'static,
) -> Result<T, FilesystemTaskError> {
    run_bounded_filesystem_task_with_wait_deadline(
        registry,
        max_workers,
        deadline,
        || deadline,
        worker_name,
        task,
    )
}

#[cfg(unix)]
fn run_bounded_filesystem_task_with_wait_deadline<T: Send + 'static>(
    registry: Arc<FilesystemWorkerRegistry>,
    max_workers: usize,
    admission_deadline: Instant,
    wait_deadline: impl FnOnce() -> Instant,
    worker_name: &'static str,
    task: impl FnOnce() -> T + Send + 'static,
) -> Result<T, FilesystemTaskError> {
    let mut state = registry
        .state
        .lock()
        .map_err(|_| FilesystemTaskError::Unavailable)?;
    loop {
        let mut index = 0;
        while index < state.workers.len() {
            if state.workers[index].is_finished() {
                let worker = state.workers.swap_remove(index);
                let _joined = worker.join();
            } else {
                index += 1;
            }
        }
        if state.running < max_workers {
            break;
        }
        let Some(remaining) = admission_deadline.checked_duration_since(Instant::now()) else {
            return Err(FilesystemTaskError::Deadline);
        };
        let waited = registry
            .available
            .wait_timeout(state, remaining)
            .map_err(|_| FilesystemTaskError::Unavailable)?;
        state = waited.0;
        if waited.1.timed_out() && Instant::now() >= admission_deadline {
            return Err(FilesystemTaskError::Deadline);
        }
    }
    if Instant::now() >= admission_deadline {
        return Err(FilesystemTaskError::Deadline);
    }

    let (sender, receiver) = mpsc::sync_channel(1);
    let worker_registry = Arc::clone(&registry);
    state.running += 1;
    let worker = std::thread::Builder::new()
        .name(String::from(worker_name))
        .spawn(move || {
            let _completion = FilesystemWorkerCompletion {
                registry: worker_registry,
            };
            let result = task();
            let _sent = sender.send(result);
        })
        .map_err(|_| {
            state.running = state.running.saturating_sub(1);
            FilesystemTaskError::Unavailable
        })?;
    state.workers.push(worker);
    drop(state);

    let deadline = wait_deadline();
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return Err(FilesystemTaskError::Deadline);
    };
    match receiver.recv_timeout(remaining) {
        Ok(result) => Ok(result),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(FilesystemTaskError::Deadline),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(FilesystemTaskError::Unavailable),
    }
}

#[cfg(unix)]
fn read_candidate(
    location: CandidateLocation<'_>,
    source: &std::path::Path,
    fallback: &InstructionPath,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> Result<CandidateRead, bool> {
    let directory = match rustix::io::dup(location.directory_descriptor) {
        Ok(directory) => directory,
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
    let source_name = location.source_name.to_os_string();
    let remaining_bytes = state
        .limits
        .candidate_source_bytes
        .saturating_sub(state.candidate_source_bytes);
    let deadline = state.started + state.limits.elapsed;
    let observed = Arc::new(AtomicU64::new(0));
    let worker_observed = Arc::clone(&observed);
    let read_result = run_bounded_filesystem_task(
        Arc::clone(&FILESYSTEM_WORKERS),
        MAX_FILESYSTEM_WORKERS,
        deadline,
        "signalbox-instruction-read",
        move || {
            let source_file = open_candidate_at_no_follow(&directory, &source_name)?;
            let mut bytes = Vec::new();
            let result = CountingReader {
                source: source_file,
                observed: worker_observed,
            }
            .take(remaining_bytes.saturating_add(1))
            .read_to_end(&mut bytes);
            Ok::<_, io::Error>((CandidateRead::new(bytes), result))
        },
    );
    match read_result {
        Ok(Ok((bytes, result))) => finish_candidate_read(
            bytes,
            result,
            source,
            fallback,
            findings,
            state,
            remaining_bytes,
        ),
        Ok(Err(_)) | Err(FilesystemTaskError::Unavailable) => Err(push_path_finding(
            source,
            fallback,
            InstructionDiscoveryFindingKind::EntryUnreadable,
            findings,
            state,
        )),
        Err(FilesystemTaskError::Deadline) => {
            state.candidate_source_bytes += observed.load(Ordering::Relaxed).min(remaining_bytes);
            Err(reach_limit(
                path_for_finding(source, fallback),
                InstructionDiscoveryLimitKind::ElapsedTime,
                findings,
                state,
            ))
        }
    }
}

#[cfg(all(unix, test))]
fn read_candidate_before_deadline(
    source_file: impl Read + Send + 'static,
    wait_deadline: impl FnOnce() -> Instant,
    source: &std::path::Path,
    fallback: &InstructionPath,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> Result<CandidateRead, bool> {
    let remaining_bytes = state
        .limits
        .candidate_source_bytes
        .saturating_sub(state.candidate_source_bytes);
    let admission_deadline = Instant::now() + Duration::from_secs(30);
    let observed = Arc::new(AtomicU64::new(0));
    let worker_observed = Arc::clone(&observed);
    let read_result = run_bounded_filesystem_task_with_wait_deadline(
        Arc::clone(&FILESYSTEM_WORKERS),
        MAX_FILESYSTEM_WORKERS,
        admission_deadline,
        wait_deadline,
        "signalbox-instruction-read",
        move || {
            let mut bytes = Vec::new();
            let result = CountingReader {
                source: source_file,
                observed: worker_observed,
            }
            .take(remaining_bytes.saturating_add(1))
            .read_to_end(&mut bytes);
            (CandidateRead::new(bytes), result)
        },
    );
    match read_result {
        Ok((bytes, result)) => finish_candidate_read(
            bytes,
            result,
            source,
            fallback,
            findings,
            state,
            remaining_bytes,
        ),
        Err(FilesystemTaskError::Deadline) => {
            state.candidate_source_bytes += observed.load(Ordering::Relaxed).min(remaining_bytes);
            Err(reach_limit(
                path_for_finding(source, fallback),
                InstructionDiscoveryLimitKind::ElapsedTime,
                findings,
                state,
            ))
        }
        Err(FilesystemTaskError::Unavailable) => Err(push_path_finding(
            source,
            fallback,
            InstructionDiscoveryFindingKind::EntryUnreadable,
            findings,
            state,
        )),
    }
}

#[cfg(all(unix, test))]
fn read_candidate_from(
    source_file: impl Read,
    source: &std::path::Path,
    fallback: &InstructionPath,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
) -> Result<CandidateRead, bool> {
    let remaining_bytes = state
        .limits
        .candidate_source_bytes
        .saturating_sub(state.candidate_source_bytes);
    let mut bytes = Vec::new();
    let read_result = source_file
        .take(remaining_bytes.saturating_add(1))
        .read_to_end(&mut bytes);
    finish_candidate_read(
        CandidateRead::new(bytes),
        read_result,
        source,
        fallback,
        findings,
        state,
        remaining_bytes,
    )
}

#[cfg(unix)]
fn finish_candidate_read(
    candidate: CandidateRead,
    read_result: io::Result<usize>,
    source: &std::path::Path,
    fallback: &InstructionPath,
    findings: &mut Vec<InstructionDiscoveryFinding>,
    state: &mut DiscoveryState,
    remaining_bytes: u64,
) -> Result<CandidateRead, bool> {
    if u64::try_from(candidate.bytes.len()).unwrap_or(u64::MAX) > remaining_bytes {
        state.candidate_source_bytes = state.limits.candidate_source_bytes;
        reach_limit(
            path_for_finding(source, fallback),
            InstructionDiscoveryLimitKind::CandidateSourceBytes,
            findings,
            state,
        );
        return Err(false);
    }
    state.candidate_source_bytes += u64::try_from(candidate.bytes.len()).unwrap_or(remaining_bytes);
    if read_result.is_err() {
        return Err(push_path_finding(
            source,
            fallback,
            InstructionDiscoveryFindingKind::EntryUnreadable,
            findings,
            state,
        ));
    }
    if !check_elapsed(fallback, findings, state) {
        return Err(false);
    }
    Ok(candidate)
}

#[cfg(unix)]
fn open_directory_no_follow(
    source: &std::path::Path,
    mut continue_opening: impl FnMut() -> bool,
) -> io::Result<Option<OwnedFd>> {
    use rustix::fs::{CWD, Mode, OFlags, openat};

    let mut descriptor = openat(
        CWD,
        std::path::Path::new("/"),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    for component in source.components() {
        if !continue_opening() {
            return Ok(None);
        }
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => {
                descriptor = open_directory_at_no_follow(&descriptor, name)?;
            }
            _ => return Err(io::Error::other("instruction root is not canonical")),
        }
    }
    if !continue_opening() {
        return Ok(None);
    }
    Ok(Some(descriptor))
}

#[cfg(unix)]
fn open_directory_no_follow_before_deadline(
    source: PathBuf,
    deadline: Instant,
) -> Result<io::Result<Option<OwnedFd>>, FilesystemTaskError> {
    run_bounded_filesystem_task(
        Arc::clone(&FILESYSTEM_WORKERS),
        MAX_FILESYSTEM_WORKERS,
        deadline,
        "signalbox-instruction-directory-open",
        move || open_directory_no_follow(&source, || Instant::now() < deadline),
    )
}

#[cfg(unix)]
fn open_directory_beneath(
    root: &OwnedFd,
    relative: &std::path::Path,
    mut continue_opening: impl FnMut() -> bool,
) -> io::Result<Option<OwnedFd>> {
    let mut descriptor = rustix::io::dup(root)?;
    for component in relative.components() {
        if !continue_opening() {
            return Ok(None);
        }
        let std::path::Component::Normal(name) = component else {
            return Err(io::Error::other("instruction path is not relative"));
        };
        descriptor = open_directory_at_no_follow(&descriptor, name)?;
    }
    if !continue_opening() {
        return Ok(None);
    }
    Ok(Some(descriptor))
}

#[cfg(unix)]
fn open_directory_beneath_before_deadline(
    root: &OwnedFd,
    relative: PathBuf,
    deadline: Instant,
) -> Result<io::Result<Option<OwnedFd>>, FilesystemTaskError> {
    let root = rustix::io::dup(root).map_err(|_| FilesystemTaskError::Unavailable)?;
    run_bounded_filesystem_task(
        Arc::clone(&FILESYSTEM_WORKERS),
        MAX_FILESYSTEM_WORKERS,
        deadline,
        "signalbox-instruction-directory-open",
        move || open_directory_beneath(&root, &relative, || Instant::now() < deadline),
    )
}

#[cfg(unix)]
fn classify_entry_at(directory: &OwnedFd, name: &OsStr) -> io::Result<rustix::fs::FileType> {
    use rustix::fs::{AtFlags, FileType, statat};

    statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
        .map(|status| FileType::from_raw_mode(status.st_mode))
        .map_err(io::Error::from)
}

#[cfg(unix)]
fn open_directory_at_no_follow(directory: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
    use rustix::fs::{Mode, OFlags, openat};

    openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(io::Error::from)
}

#[cfg(unix)]
fn open_candidate_at_no_follow(directory: &OwnedFd, name: &OsStr) -> io::Result<fs::File> {
    use rustix::fs::{Mode, OFlags, openat};

    let descriptor = openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
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
    metadata: OptionalFrontmatterField<DiscardStringMap>,
    #[serde(default, rename = "allowed-tools")]
    allowed_tools: OptionalFrontmatterField<String>,
}

#[derive(Default)]
enum OptionalFrontmatterField<T> {
    #[default]
    Missing,
    Present(T),
}

struct DiscardStringMap;

impl<'de> Deserialize<'de> for DiscardStringMap {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(DiscardStringMapVisitor)
    }
}

struct DiscardStringMapVisitor;

impl<'de> serde::de::Visitor<'de> for DiscardStringMapVisitor {
    type Value = DiscardStringMap;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a string-to-string metadata mapping")
    }

    fn visit_map<Mapping>(self, mut mapping: Mapping) -> Result<Self::Value, Mapping::Error>
    where
        Mapping: serde::de::MapAccess<'de>,
    {
        while mapping.next_entry::<String, String>()?.is_some() {}
        Ok(DiscardStringMap)
    }
}

struct OptionalFrontmatterFieldVisitor<T>(std::marker::PhantomData<T>);

impl<'de, T> serde::de::Visitor<'de> for OptionalFrontmatterFieldVisitor<T>
where
    T: Deserialize<'de>,
{
    type Value = OptionalFrontmatterField<T>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a non-null optional skill field")
    }

    fn visit_none<Error>(self) -> Result<Self::Value, Error>
    where
        Error: serde::de::Error,
    {
        Err(Error::custom("an optional skill field cannot be null"))
    }

    fn visit_unit<Error>(self) -> Result<Self::Value, Error>
    where
        Error: serde::de::Error,
    {
        self.visit_none()
    }

    fn visit_some<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        T::deserialize(deserializer).map(OptionalFrontmatterField::Present)
    }
}

impl<'de, T> Deserialize<'de> for OptionalFrontmatterField<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        deserializer.deserialize_option(OptionalFrontmatterFieldVisitor(std::marker::PhantomData))
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
    let body = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let boundary = frontmatter_boundary(body)?;
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

fn frontmatter_boundary(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    body.match_indices("---").find_map(|(index, _)| {
        let starts_line = index == 0 || bytes[index - 1] == b'\n';
        let remainder = &body[index + 3..];
        let ends_line =
            remainder.is_empty() || remainder.starts_with('\n') || remainder.starts_with("\r\n");
        (starts_line && ends_line).then(|| {
            if index > 1 && bytes[index - 2..index] == *b"\r\n" {
                index - 2
            } else if index > 0 && bytes[index - 1] == b'\n' {
                index - 1
            } else {
                index
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    struct BytesThenError {
        bytes: Option<Vec<u8>>,
    }

    #[cfg(unix)]
    struct BytesThenBlock {
        bytes: Option<Vec<u8>>,
        blocker: std::os::unix::net::UnixStream,
        ready: Option<mpsc::SyncSender<()>>,
        release: mpsc::Receiver<()>,
    }

    #[cfg(unix)]
    impl Read for BytesThenError {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let Some(bytes) = self.bytes.take() else {
                return Err(io::Error::other("fixture read failure"));
            };
            buffer[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }
    }

    #[cfg(unix)]
    impl Read for BytesThenBlock {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if let Some(bytes) = self.bytes.take() {
                buffer[..bytes.len()].copy_from_slice(&bytes);
                self.ready
                    .take()
                    .expect("fixture readiness is signalled once")
                    .send(())
                    .expect("deadline controller awaits fixture readiness");
                self.release
                    .recv()
                    .expect("deadline controller releases the partial read");
                return Ok(bytes.len());
            }
            self.blocker.read(buffer)
        }
    }

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
    fn excluded_directories_do_not_consume_their_contents_or_yield_documents() {
        const SMALL_CLASSIFIED_ENTRY_LIMIT: u64 = 8;

        let temporary = tempfile::tempdir().expect("temporary root exists");
        let adjacent_document = temporary.path().join("AGENTS.md");
        fs::write(&adjacent_document, "workspace instructions")
            .expect("adjacent agent document is written");
        let vcs_metadata = temporary.path().join(".git");
        fs::create_dir(&vcs_metadata).expect("VCS metadata directory exists");
        fill_directory_beyond_limit(&vcs_metadata, SMALL_CLASSIFIED_ENTRY_LIMIT);
        fs::write(
            vcs_metadata.join("AGENTS.md"),
            "excluded metadata instructions",
        )
        .expect("metadata agent document is written");
        let build_output = temporary.path().join("target");
        fs::create_dir(&build_output).expect("build output directory exists");
        fill_directory_beyond_limit(&build_output, SMALL_CLASSIFIED_ENTRY_LIMIT);
        fs::write(
            build_output.join("AGENTS.md"),
            "excluded build instructions",
        )
        .expect("build agent document is written");
        let nested_repository = temporary.path().join("nested-clone");
        fs::create_dir_all(nested_repository.join(".git"))
            .expect("nested repository metadata exists");
        fill_directory_beyond_limit(&nested_repository, SMALL_CLASSIFIED_ENTRY_LIMIT);
        fs::write(
            nested_repository.join("AGENTS.md"),
            "excluded nested repository instructions",
        )
        .expect("nested repository agent document is written");
        let root = workspace_root(&temporary);

        let snapshot = discover_with_limits(
            vec![root],
            test_limits(SMALL_CLASSIFIED_ENTRY_LIMIT, 4, 64, Duration::from_secs(1)),
        );

        assert!(snapshot.is_complete());
        assert!(snapshot.findings().is_empty());
        assert_eq!(snapshot.classified_entries(), 4);
        assert_eq!(snapshot.bundles().len(), 1);
        assert_eq!(
            snapshot.bundles()[0].source_path().absolute_path(),
            adjacent_document.to_string_lossy()
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_skill_location_is_relative_to_the_registered_root() {
        let temporary = tempfile::tempdir().expect("temporary root exists");
        let workspace = temporary.path().join(".agents/skills/review-rust");
        fs::create_dir_all(&workspace).expect("workspace exists");
        fs::write(
            workspace.join("SKILL.md"),
            "---\nname: review-rust\ndescription: Review Rust changes.\n---\n# Review\n",
        )
        .expect("skill is written");
        let canonical = workspace.canonicalize().expect("root canonicalizes");
        let root = InstructionDiscoveryRoot::new(
            InstructionDiscoveryRootKind::Workspace,
            InstructionPath::try_new(canonical.to_string_lossy().into_owned())
                .expect("path is valid"),
        );

        let snapshot = discover_workspace_instructions(vec![root]);

        assert!(snapshot.bundles().is_empty());
        assert!(snapshot.findings().is_empty());
    }

    #[test]
    fn configured_root_may_point_directly_at_one_skill() {
        let temporary = tempfile::tempdir().expect("temporary root exists");
        let skill = temporary.path().join("review-rust");
        fs::create_dir(&skill).expect("skill directory exists");
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: review-rust\ndescription: Review Rust changes.\n---\n# Review\n",
        )
        .expect("skill is written");
        let canonical = skill.canonicalize().expect("root canonicalizes");
        let root = InstructionDiscoveryRoot::new(
            InstructionDiscoveryRootKind::Configured,
            InstructionPath::try_new(canonical.to_string_lossy().into_owned())
                .expect("path is valid"),
        );

        let snapshot = discover_workspace_instructions(vec![root]);

        assert_eq!(snapshot.bundles().len(), 1);
        assert!(snapshot.findings().is_empty());
        assert_eq!(
            snapshot.bundles()[0].kind(),
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

    #[cfg(unix)]
    #[test]
    fn root_open_failure_is_root_unavailable() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary root exists");
        let outside = tempfile::tempdir().expect("outside root exists");
        let linked_root = temporary.path().join("linked-root");
        symlink(outside.path(), &linked_root).expect("root link exists");
        let root = InstructionDiscoveryRoot::new(
            InstructionDiscoveryRootKind::Workspace,
            InstructionPath::try_new(linked_root.to_string_lossy().into_owned())
                .expect("linked path has canonical spelling"),
        );

        let snapshot = discover_workspace_instructions(vec![root]);

        assert!(snapshot.bundles().is_empty());
        assert_eq!(snapshot.findings().len(), 1);
        assert_eq!(
            snapshot.findings()[0].kind(),
            InstructionDiscoveryFindingKind::RootUnavailable
        );
    }

    #[cfg(unix)]
    #[test]
    fn root_open_refuses_an_intermediate_symlink() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary root exists");
        let outside = tempfile::tempdir().expect("outside root exists");
        let outside_root = outside.path().join("workspace");
        fs::create_dir(&outside_root).expect("outside workspace exists");
        fs::write(outside_root.join("AGENTS.md"), "outside instructions")
            .expect("outside source exists");
        let linked_parent = temporary.path().join("linked-parent");
        symlink(outside.path(), &linked_parent).expect("intermediate link exists");
        let linked_root = linked_parent.join("workspace");
        let root = InstructionDiscoveryRoot::new(
            InstructionDiscoveryRootKind::Workspace,
            InstructionPath::try_new(linked_root.to_string_lossy().into_owned())
                .expect("linked path has canonical spelling"),
        );

        let snapshot = discover_workspace_instructions(vec![root]);

        assert!(snapshot.bundles().is_empty());
        assert_eq!(snapshot.findings().len(), 1);
        assert_eq!(
            snapshot.findings()[0].kind(),
            InstructionDiscoveryFindingKind::RootUnavailable
        );
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
    fn entry_limit_discards_the_nondeterministic_partial_directory_batch() {
        let temporary = tempfile::tempdir().expect("temporary root exists");
        fs::write(temporary.path().join("AGENTS.md"), "not admitted").expect("candidate exists");
        fs::write(temporary.path().join("another"), "also observed").expect("second entry exists");
        let root = workspace_root(&temporary);

        let snapshot =
            discover_with_limits(vec![root], test_limits(1, 4, 64, Duration::from_secs(1)));

        assert!(!snapshot.is_complete());
        assert_eq!(snapshot.classified_entries(), 0);
        assert!(snapshot.bundles().is_empty());
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
        let limits = test_limits(4, 4, 1, Duration::from_secs(1));

        let snapshot = discover_with_limits(vec![root], limits);

        assert!(!snapshot.is_complete());
        assert_eq!(
            snapshot.candidate_source_bytes(),
            limits.candidate_source_bytes
        );
        assert!(snapshot.bundles().is_empty());
        assert_eq!(
            snapshot.findings()[0].kind(),
            InstructionDiscoveryFindingKind::LimitReached(
                InstructionDiscoveryLimitKind::CandidateSourceBytes
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn candidate_read_stops_at_the_elapsed_deadline() {
        use std::os::unix::net::UnixStream;

        let (reader, writer) = UnixStream::pair().expect("fixture stream pair opens");
        let partial = b"read before timeout".to_vec();
        let source = PathBuf::from("/workspace/AGENTS.md");
        let fallback = InstructionPath::try_new(String::from("/workspace"))
            .expect("fixture fallback is valid");
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let mut findings = Vec::new();
        let mut state = test_state(test_limits(4, 4, 64, Duration::from_millis(5)));

        let result = read_candidate_before_deadline(
            BytesThenBlock {
                bytes: Some(partial.clone()),
                blocker: reader,
                ready: Some(ready_sender),
                release: release_receiver,
            },
            move || {
                ready_receiver
                    .recv()
                    .expect("the reader publishes partial bytes before the deadline starts");
                let deadline = Instant::now() + Duration::from_millis(5);
                release_sender
                    .send(())
                    .expect("the reader accepts its partial-read release");
                deadline
            },
            &source,
            &fallback,
            &mut findings,
            &mut state,
        );

        assert!(result.is_err());
        assert!(!state.complete);
        assert_eq!(
            state.candidate_source_bytes,
            u64::try_from(partial.len()).expect("fixture byte length fits u64")
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].kind(),
            InstructionDiscoveryFindingKind::LimitReached(
                InstructionDiscoveryLimitKind::ElapsedTime
            )
        );
        drop(writer);
    }

    #[cfg(unix)]
    #[test]
    fn stalled_filesystem_workers_are_globally_bounded() {
        use std::os::unix::net::UnixStream;

        let registry = Arc::new(FilesystemWorkerRegistry::default());
        let (first_reader, first_writer) = UnixStream::pair().expect("first stream pair opens");
        let (second_reader, second_writer) = UnixStream::pair().expect("second stream pair opens");

        let first = run_bounded_filesystem_task(
            Arc::clone(&registry),
            1,
            Instant::now() + Duration::from_millis(5),
            "signalbox-test-filesystem-worker",
            move || {
                let mut bytes = Vec::new();
                std::io::BufReader::new(first_reader).read_to_end(&mut bytes)
            },
        );
        let first_worker_count = registry
            .state
            .lock()
            .expect("registry is readable")
            .workers
            .len();
        let second = run_bounded_filesystem_task(
            Arc::clone(&registry),
            1,
            Instant::now() + Duration::from_millis(5),
            "signalbox-test-filesystem-worker",
            move || {
                let mut bytes = Vec::new();
                std::io::BufReader::new(second_reader).read_to_end(&mut bytes)
            },
        );
        let second_worker_count = registry
            .state
            .lock()
            .expect("registry is readable")
            .workers
            .len();

        assert_eq!(first.err(), Some(FilesystemTaskError::Deadline));
        assert_eq!(first_worker_count, 1);
        assert_eq!(second.err(), Some(FilesystemTaskError::Deadline));
        assert_eq!(second_worker_count, 1);
        drop(first_writer);
        drop(second_writer);
    }

    #[cfg(unix)]
    #[test]
    fn expired_deadline_does_not_spawn_a_filesystem_worker() {
        let registry = Arc::new(FilesystemWorkerRegistry::default());

        let result = run_bounded_filesystem_task(
            Arc::clone(&registry),
            1,
            Instant::now() - Duration::from_millis(1),
            "signalbox-test-filesystem-worker",
            || (),
        );
        let worker_count = registry
            .state
            .lock()
            .expect("registry is readable")
            .workers
            .len();

        assert_eq!(result, Err(FilesystemTaskError::Deadline));
        assert_eq!(worker_count, 0);
    }

    #[cfg(unix)]
    #[test]
    fn raw_entry_names_sort_before_classification_failures() {
        let names = vec![OsString::from("zeta"), OsString::from("alpha")];

        let entries = classify_sorted_names(
            names,
            |_name| Err(io::Error::other("fixture classification failure")),
            || {},
        );

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].as_ref().expect_err("alpha fails"), "alpha");
        assert_eq!(entries[1].as_ref().expect_err("zeta fails"), "zeta");
    }

    #[cfg(unix)]
    #[test]
    fn directory_classification_publishes_finished_entry_progress() {
        let names = vec![OsString::from("alpha"), OsString::from("beta")];
        let classified = AtomicU64::new(0);

        let entries = classify_sorted_names(
            names,
            |_name| Ok(rustix::fs::FileType::RegularFile),
            || {
                classified.fetch_add(1, Ordering::Relaxed);
            },
        );

        assert_eq!(entries.len(), 2);
        assert_eq!(classified.load(Ordering::Relaxed), 2);
    }

    #[cfg(unix)]
    #[test]
    fn pending_siblings_share_their_parent_index() {
        let directories = vec![
            PendingDirectory {
                parent: None,
                name: Some(OsString::from("parent")),
                source_prefix: Err(InstructionDiscoveryFindingKind::NonUtf8SourcePath),
            },
            PendingDirectory {
                parent: Some(0),
                name: Some(OsString::from("left")),
                source_prefix: Err(InstructionDiscoveryFindingKind::NonUtf8SourcePath),
            },
            PendingDirectory {
                parent: Some(0),
                name: Some(OsString::from("right")),
                source_prefix: Err(InstructionDiscoveryFindingKind::NonUtf8SourcePath),
            },
        ];

        assert_eq!(directories[1].parent, directories[2].parent);
        assert_eq!(
            pending_directory_path(1, &directories),
            PathBuf::from("parent/left")
        );
        assert_eq!(
            pending_directory_path(2, &directories),
            PathBuf::from("parent/right")
        );
        assert_eq!(directories[1].name.as_deref(), Some(OsStr::new("left")));
        assert_eq!(directories[2].name.as_deref(), Some(OsStr::new("right")));
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
        let descriptor = open_directory_no_follow(temporary.path(), || true)
            .expect("root descriptor opens")
            .expect("the unlimited fixture keeps opening");
        let mut findings = Vec::new();
        let mut state = test_state(test_limits(4, 4, 64, Duration::from_secs(1)));

        let result = read_candidate(
            CandidateLocation {
                directory_descriptor: &descriptor,
                directory: temporary.path(),
                directory_prefix: Err(InstructionDiscoveryFindingKind::NonUtf8SourcePath),
                source_name: OsStr::new("AGENTS.md"),
            },
            &candidate,
            root.path(),
            &mut findings,
            &mut state,
        );

        assert!(result.is_err());
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].kind(),
            InstructionDiscoveryFindingKind::EntryUnreadable
        );
    }

    #[cfg(unix)]
    #[test]
    fn candidate_descriptor_is_nonblocking_before_type_validation() {
        let temporary = tempfile::tempdir().expect("temporary root exists");
        fs::write(temporary.path().join("AGENTS.md"), "instructions").expect("candidate exists");
        let descriptor = open_directory_no_follow(temporary.path(), || true)
            .expect("root descriptor opens")
            .expect("the unlimited fixture keeps opening");

        let candidate = open_candidate_at_no_follow(&descriptor, OsStr::new("AGENTS.md"))
            .expect("regular candidate opens");
        let flags = rustix::fs::fcntl_getfl(&candidate).expect("candidate flags are readable");

        assert!(flags.contains(rustix::fs::OFlags::NONBLOCK));
    }

    #[cfg(unix)]
    #[test]
    fn directory_open_refuses_a_symlink_after_enumeration() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary root exists");
        let outside = tempfile::tempdir().expect("outside root exists");
        let child = temporary.path().join("child");
        symlink(outside.path(), &child).expect("child link exists");
        let descriptor = open_directory_no_follow(temporary.path(), || true)
            .expect("root descriptor opens")
            .expect("the unlimited fixture keeps opening");

        let result = open_directory_at_no_follow(&descriptor, OsStr::new("child"));

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn failed_candidate_read_charges_retained_bytes() {
        let temporary = tempfile::tempdir().expect("temporary root exists");
        let source = temporary.path().join("AGENTS.md");
        let root = workspace_root(&temporary);
        let mut findings = Vec::new();
        let mut state = test_state(test_limits(4, 4, 64, Duration::from_secs(1)));
        let retained = b"read before failure".to_vec();

        let result = read_candidate_from(
            BytesThenError {
                bytes: Some(retained.clone()),
            },
            &source,
            root.path(),
            &mut findings,
            &mut state,
        );

        assert!(result.is_err());
        assert_eq!(
            state.candidate_source_bytes,
            u64::try_from(retained.len()).expect("fixture length fits u64")
        );
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

    #[test]
    fn portable_skill_metadata_validates_without_becoming_retained_metadata() {
        let expected_name = "review-rust";
        let expected_description = "Review.";
        let source = format!(
            "---\nname: {expected_name}\ndescription: {expected_description}\nmetadata:\n  alpha: one\n  beta: two\n---\n"
        );

        let skill = parse_skill(&source, expected_name).expect("string metadata is portable");

        assert_eq!(skill.name(), expected_name);
        assert_eq!(skill.description(), expected_description);
    }

    #[cfg(unix)]
    #[test]
    fn component_reopen_stops_when_the_deadline_expires() {
        let temporary = tempfile::tempdir().expect("temporary root exists");
        fs::create_dir_all(temporary.path().join("first/second"))
            .expect("nested fixture directories exist");
        let descriptor = open_directory_no_follow(temporary.path(), || true)
            .expect("root descriptor opens")
            .expect("the unlimited fixture keeps opening");
        let mut checks = [true, false].into_iter();

        let reopened =
            open_directory_beneath(&descriptor, PathBuf::from("first/second").as_path(), || {
                checks.next().unwrap_or(false)
            })
            .expect("the deadline stop is not an I/O failure");

        assert!(reopened.is_none());
    }

    #[test]
    fn crlf_portable_skill_frontmatter_is_accepted() {
        let expected_name = "review-rust";
        let expected_description = "Review Rust.";
        let source = format!(
            "---\r\nname: {expected_name}\r\ndescription: {expected_description}\r\n---\r\n# Review\r\n"
        );

        let skill = parse_skill(&source, expected_name).expect("CRLF frontmatter is portable");

        assert_eq!(skill.name(), expected_name);
        assert_eq!(skill.description(), expected_description);
    }

    #[test]
    fn mixed_line_endings_around_portable_skill_frontmatter_are_accepted() {
        let expected_name = "review-rust";
        let expected_crlf_open_description = "Review CRLF opener.";
        let expected_lf_open_description = "Review LF opener.";
        let crlf_open_source = format!(
            "---\r\nname: {expected_name}\ndescription: {expected_crlf_open_description}\n---\nsteps\n"
        );
        let lf_open_source = format!(
            "---\nname: {expected_name}\r\ndescription: {expected_lf_open_description}\r\n---\r\nsteps\r\n"
        );

        let crlf_open_skill = parse_skill(&crlf_open_source, expected_name)
            .expect("CRLF opener and LF closer are portable");
        let lf_open_skill = parse_skill(&lf_open_source, expected_name)
            .expect("LF opener and CRLF closer are portable");

        assert_eq!(
            crlf_open_skill.description(),
            expected_crlf_open_description
        );
        assert_eq!(lf_open_skill.description(), expected_lf_open_description);
    }

    #[test]
    fn portable_skill_frontmatter_closing_delimiter_may_end_the_file() {
        let expected_name = "review-rust";
        let expected_lf_description = "Review LF.";
        let expected_crlf_description = "Review CRLF.";
        let lf_source =
            format!("---\nname: {expected_name}\ndescription: {expected_lf_description}\n---");
        let crlf_source = format!(
            "---\r\nname: {expected_name}\r\ndescription: {expected_crlf_description}\r\n---"
        );

        let lf_skill = parse_skill(&lf_source, expected_name).expect("LF frontmatter is portable");
        let crlf_skill =
            parse_skill(&crlf_source, expected_name).expect("CRLF frontmatter is portable");

        assert_eq!(lf_skill.description(), expected_lf_description);
        assert_eq!(crlf_skill.description(), expected_crlf_description);
    }

    #[test]
    fn source_relative_path_must_fit_the_independent_path_bound() {
        let root = InstructionPath::try_new(String::from("/r")).expect("fixture root is valid");
        let root_prefix = InstructionSourcePathInterner::root_prefix(root);
        let source_name = "a".repeat(4_097);
        let mut interner = InstructionSourcePathInterner::new();

        let result =
            source_path_for_registration(Ok(&root_prefix), OsStr::new(&source_name), &mut interner);

        assert_eq!(
            result,
            Err(InstructionDiscoveryFindingKind::EntryUnreadable)
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

    fn fill_directory_beyond_limit(directory: &std::path::Path, limit: u64) {
        for index in 0..limit.saturating_mul(4) {
            fs::write(directory.join(format!("ignored-{index}")), "ignored")
                .expect("ignored fixture entry is written");
        }
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
            source_paths: InstructionSourcePathInterner::new(),
            seen_sources: HashSet::new(),
            complete: true,
        }
    }
}
