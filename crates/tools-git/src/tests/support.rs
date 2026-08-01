//! Shared fixtures for the local Git tool tests.

use std::{
    ffi::OsString,
    fs,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use git2::{
    IndexAddOption, IndexEntry, IndexTime, ObjectType, Oid, Repository, Signature,
    build::CheckoutBuilder,
};
use rustix::fs::{CWD, Mode, OFlags, mkfifoat, openat};
use signalbox_tools_workspace::{
    LocalWorkspaceFileSystem, WorkspaceDirectoryRead, WorkspaceEntryKind, WorkspaceFileBytes,
    WorkspaceFileSystem, WorkspaceResolveError, WorkspaceRoot, WorkspaceRootError,
};
use tempfile::TempDir;

use crate::arguments::{GitCommitArguments, LocalOperation};
use crate::catalog::LocalGitTools;
use crate::executor::{LocalGitExecutor, clone_index_entry};
use crate::identity::GitIdentity;
use crate::limits::{INDEX_SKIP_WORKTREE, MAX_LOG_IDENTITY_BYTES};
use crate::status::status;

pub(super) const AUTHOR_NAME: &str = "Signalbox Fixer";

pub(super) const AUTHOR_EMAIL: &str = "fixer@example.test";

pub(super) const INITIAL_MESSAGE: &str = "initial";

pub(super) const MODEL_MESSAGE: &str = "subject\n\nmodel data: $(not interpreted)\n";

pub(super) const FIX_BRANCH: &str = "agent/fix";

pub(super) const TRACKED_PATH: &str = "tracked.txt";

pub(super) const UNTRACKED_PATH: &str = "untracked.txt";

pub(super) const INITIAL_CONTENT: &str = "before\n";

pub(super) const CHANGED_CONTENT: &str = "after\n";

pub(super) const TARGET_CONTENT: &str = "target\n";

pub(super) const CONFLICT_OURS_CONTENT: &str = "ours\n";

pub(super) const CRLF_CONTENT: &[u8] = b"first\r\nsecond\r\n";

pub(super) const UNTRACKED_CONTENT: &str = "untracked\n";

pub(super) const NESTED_TRACKED_DIRECTORY: &str = "removed";

pub(super) const NESTED_TRACKED_PATH: &str = "removed/tracked.txt";

pub(super) const RENAMED_TRACKED_PATH: &str = "renamed.txt";

pub(super) const TWICE_RENAMED_TRACKED_PATH: &str = "twice-renamed.txt";

pub(super) const EMBEDDED_REPOSITORY_PATH: &str = "vendor";

pub(super) const SUBMODULE_PATH: &str = "dependency";

pub(super) struct Fixture {
    directory: TempDir,
    pub(super) initial: Oid,
}

pub(super) struct ModeOnlyPathFixture {
    path: OsString,
    quoted_header: &'static str,
    unquoted_header: &'static str,
}

impl ModeOnlyPathFixture {
    pub(super) fn non_utf8() -> Self {
        Self {
            path: OsString::from_vec(vec![b'n', 0xff, b'.', b't', b'x', b't']),
            quoted_header: "\"a/n\\377.txt\" \"b/n\\377.txt\"",
            unquoted_header: "",
        }
    }

    pub(super) fn control() -> Self {
        Self {
            path: OsString::from("line\nbreak.txt"),
            quoted_header: "diff --git \"a/line\\nbreak.txt\" \"b/line\\nbreak.txt\"\n",
            unquoted_header: "diff --git a/line\nbreak.txt",
        }
    }

    pub(super) fn path(&self) -> &Path {
        Path::new(&self.path)
    }

    pub(super) fn quoted_header(&self) -> &str {
        self.quoted_header
    }

    pub(super) fn unquoted_header(&self) -> &str {
        self.unquoted_header
    }
}

#[derive(Clone, Debug)]
pub(super) struct ReplacingRootFileSystem {
    pub(super) retired_root: PathBuf,
    pub(super) replacement_root: PathBuf,
}

#[derive(Clone, Debug)]
pub(super) struct ObservingIndexLockFileSystem {
    pub(super) root_path: PathBuf,
    pub(super) lock_observed: Arc<AtomicBool>,
}

#[derive(Clone, Debug)]
pub(super) struct ConcurrentRootOpenFileSystem {
    pub(super) extra_root: Arc<Mutex<Option<fs::File>>>,
}

impl WorkspaceFileSystem for ReplacingRootFileSystem {
    fn open_root(&self, root: &Path) -> Result<WorkspaceRoot, WorkspaceRootError> {
        fs::rename(root, &self.retired_root).expect("original root retires during fixture open");
        fs::create_dir(root).expect("replacement root constructs during fixture open");
        Repository::init(root).expect("replacement repository initializes during fixture open");
        let pinned = LocalWorkspaceFileSystem.open_root(root)?;
        fs::rename(root, &self.replacement_root)
            .expect("replacement root retires after fixture pin");
        fs::rename(&self.retired_root, root).expect("original root restores after fixture pin");
        Ok(pinned)
    }

    fn entry_kind(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
    ) -> Result<WorkspaceEntryKind, WorkspaceResolveError> {
        LocalWorkspaceFileSystem.entry_kind(root, path)
    }

    fn read_directory(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
        max_entries: usize,
        max_inspections: usize,
        max_path_bytes: usize,
    ) -> Result<WorkspaceDirectoryRead, WorkspaceResolveError> {
        LocalWorkspaceFileSystem.read_directory(
            root,
            path,
            max_entries,
            max_inspections,
            max_path_bytes,
        )
    }

    fn read_file_prefix(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
        max_bytes: usize,
    ) -> Result<WorkspaceFileBytes, WorkspaceResolveError> {
        LocalWorkspaceFileSystem.read_file_prefix(root, path, max_bytes)
    }
}

impl WorkspaceFileSystem for ObservingIndexLockFileSystem {
    fn open_root(&self, root: &Path) -> Result<WorkspaceRoot, WorkspaceRootError> {
        LocalWorkspaceFileSystem.open_root(root)
    }

    fn entry_kind(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
    ) -> Result<WorkspaceEntryKind, WorkspaceResolveError> {
        LocalWorkspaceFileSystem.entry_kind(root, path)
    }

    fn read_directory(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
        max_entries: usize,
        max_inspections: usize,
        max_path_bytes: usize,
    ) -> Result<WorkspaceDirectoryRead, WorkspaceResolveError> {
        LocalWorkspaceFileSystem.read_directory(
            root,
            path,
            max_entries,
            max_inspections,
            max_path_bytes,
        )
    }

    fn read_file_prefix(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
        max_bytes: usize,
    ) -> Result<WorkspaceFileBytes, WorkspaceResolveError> {
        let read = LocalWorkspaceFileSystem.read_file_prefix(root, path, max_bytes)?;
        self.lock_observed.store(
            self.root_path.join(".git/index.lock").is_file(),
            Ordering::SeqCst,
        );
        Ok(read)
    }
}

impl WorkspaceFileSystem for ConcurrentRootOpenFileSystem {
    fn open_root(&self, root: &Path) -> Result<WorkspaceRoot, WorkspaceRootError> {
        let pinned = LocalWorkspaceFileSystem.open_root(root)?;
        let extra = fs::File::open(root).expect("concurrent root descriptor opens");
        self.extra_root
            .lock()
            .expect("concurrent root holder locks")
            .replace(extra);
        Ok(pinned)
    }

    fn entry_kind(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
    ) -> Result<WorkspaceEntryKind, WorkspaceResolveError> {
        LocalWorkspaceFileSystem.entry_kind(root, path)
    }

    fn read_directory(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
        max_entries: usize,
        max_inspections: usize,
        max_path_bytes: usize,
    ) -> Result<WorkspaceDirectoryRead, WorkspaceResolveError> {
        LocalWorkspaceFileSystem.read_directory(
            root,
            path,
            max_entries,
            max_inspections,
            max_path_bytes,
        )
    }

    fn read_file_prefix(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
        max_bytes: usize,
    ) -> Result<WorkspaceFileBytes, WorkspaceResolveError> {
        LocalWorkspaceFileSystem.read_file_prefix(root, path, max_bytes)
    }
}

impl Fixture {
    pub(super) fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary repository root constructs");
        let repository = Repository::init(directory.path()).expect("repository initializes");
        fs::write(directory.path().join(TRACKED_PATH), INITIAL_CONTENT)
            .expect("fixture file writes");
        let initial = commit_all(&repository, INITIAL_MESSAGE);
        Self { directory, initial }
    }

    pub(super) fn root(&self) -> &Path {
        self.directory.path()
    }
}

impl Fixture {
    pub(super) fn executor(&self) -> LocalGitExecutor<LocalWorkspaceFileSystem> {
        LocalGitTools::try_new(LocalWorkspaceFileSystem, self.root(), identity())
            .expect("local Git suite constructs")
            .into_parts()
            .1
    }
}

pub(super) fn identity() -> GitIdentity {
    GitIdentity::try_new(AUTHOR_NAME, AUTHOR_EMAIL).expect("fixture identity is admitted")
}

pub(super) fn commit_all(repository: &Repository, message: &str) -> Oid {
    let mut index = repository.index().expect("fixture index opens");
    index
        .add_all(["*"], IndexAddOption::DEFAULT, None)
        .expect("fixture stages");
    index.write().expect("fixture index writes");
    commit_index(repository, message)
}

pub(super) fn commit_index(repository: &Repository, message: &str) -> Oid {
    let mut index = repository.index().expect("fixture index opens");
    let tree_id = index.write_tree().expect("fixture tree writes");
    let tree = repository.find_tree(tree_id).expect("fixture tree opens");
    let signature =
        Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).expect("fixture signature constructs");
    let parent = repository
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok());
    let parents = parent.iter().collect::<Vec<_>>();
    repository
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parents,
        )
        .expect("fixture commit writes")
}

pub(super) fn index_extension(bytes: &[u8], signature: &[u8; 4]) -> Vec<u8> {
    let start = bytes
        .windows(signature.len())
        .position(|window| window == signature)
        .expect("fixture index extension exists");
    let size = u32::from_be_bytes(
        bytes[start + 4..start + 8]
            .try_into()
            .expect("fixture extension length exists"),
    ) as usize;
    bytes[start..start + 8 + size].to_vec()
}

pub(super) fn long_status_path() -> PathBuf {
    let segment = "a".repeat(200);
    PathBuf::from(&segment)
        .join(&segment)
        .join(&segment)
        .join(&segment)
        .join(&segment)
        .join(&segment)
        .join(TRACKED_PATH)
}

pub(super) fn long_author_name() -> String {
    "n".repeat(MAX_LOG_IDENTITY_BYTES + 1)
}

pub(super) fn long_author_email() -> String {
    format!("{}@example.test", "e".repeat(MAX_LOG_IDENTITY_BYTES))
}

pub(super) fn install_gitlink(repository: &Repository, path: &str, target: Oid) {
    let mut index = repository.index().expect("fixture index opens");
    let entry = IndexEntry {
        ctime: IndexTime::new(0, 0),
        mtime: IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode: 0o160000,
        uid: 0,
        gid: 0,
        file_size: 0,
        id: target,
        flags: 0,
        flags_extended: 0,
        path: path.as_bytes().to_vec(),
    };
    index.add(&entry).expect("gitlink stages");
    index.write().expect("gitlink index writes");
}

pub(super) fn count_loose_objects(root: &Path) -> usize {
    fs::read_dir(root.join(".git/objects"))
        .expect("fixture object directory reads")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| entry.file_name() != "info" && entry.file_name() != "pack")
        .map(|entry| {
            fs::read_dir(entry.path())
                .expect("fixture loose-object directory reads")
                .count()
        })
        .sum()
}

pub(super) fn packed_object_counts(root: &Path) -> Vec<u32> {
    let mut counts = fs::read_dir(root.join(".git/objects/pack"))
        .expect("fixture pack directory reads")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "idx")
        })
        .map(|entry| {
            let bytes = fs::read(entry.path()).expect("fixture pack index reads");
            u32::from_be_bytes(
                bytes[1028..1032]
                    .try_into()
                    .expect("fixture pack fanout count exists"),
            )
        })
        .collect::<Vec<_>>();
    counts.sort_unstable();
    counts
}

pub(super) fn set_index_flags(repository: &Repository, path: &str, flags: u16) {
    let mut index = repository.index().expect("fixture index opens");
    let mut entry = clone_index_entry(
        &index
            .get_path(Path::new(path), 0)
            .expect("fixture index entry exists"),
    );
    entry.flags |= flags;
    index.add(&entry).expect("fixture flagged entry installs");
    index.write().expect("fixture flagged index writes");
}

pub(super) fn install_deleted_conflict(fixture: &Fixture) {
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let original_reference = repository
        .head()
        .expect("fixture HEAD exists")
        .name()
        .expect("fixture HEAD name is UTF-8")
        .to_owned();
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists");
    repository
        .branch("conflicting", &initial, false)
        .expect("conflicting branch creates");
    fs::write(fixture.root().join(TRACKED_PATH), CONFLICT_OURS_CONTENT)
        .expect("ours fixture content writes");
    commit_all(&repository, "ours");
    repository
        .set_head("refs/heads/conflicting")
        .expect("conflicting fixture branch selects");
    repository
        .checkout_head(Some(CheckoutBuilder::new().force()))
        .expect("conflicting fixture branch checks out");
    fs::write(fixture.root().join(TRACKED_PATH), "theirs\n")
        .expect("theirs fixture content writes");
    let theirs = commit_all(&repository, "theirs");
    repository
        .set_head(&original_reference)
        .expect("original fixture branch selects");
    repository
        .checkout_head(Some(CheckoutBuilder::new().force()))
        .expect("original fixture branch checks out");
    let annotated = repository
        .find_annotated_commit(theirs)
        .expect("theirs annotated commit opens");
    repository
        .merge(&[&annotated], None, None)
        .expect("fixture merge produces conflict");
    fs::remove_file(fixture.root().join(TRACKED_PATH)).expect("conflicted fixture path deletes");
}

pub(super) fn install_missing_skip_worktree_entry(fixture: &Fixture) {
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let mut index = repository.index().expect("fixture index opens");
    let mut entry = clone_index_entry(
        &index
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("fixture tracked entry exists"),
    );
    entry.flags_extended |= INDEX_SKIP_WORKTREE;
    index.add(&entry).expect("skip-worktree entry installs");
    index.write().expect("skip-worktree index writes");
    fs::remove_file(fixture.root().join(TRACKED_PATH)).expect("skip-worktree fixture file removes");
}

pub(super) fn install_staged_missing_skip_worktree_entry(fixture: &Fixture) {
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let changed_blob = repository
        .blob(CHANGED_CONTENT.as_bytes())
        .expect("changed fixture blob writes");
    let mut index = repository.index().expect("fixture index opens");
    let mut entry = clone_index_entry(
        &index
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("fixture tracked entry exists"),
    );
    entry.id = changed_blob;
    entry.file_size = CHANGED_CONTENT.len() as u32;
    entry.flags_extended |= INDEX_SKIP_WORKTREE;
    index
        .add(&entry)
        .expect("staged skip-worktree entry installs");
    index.write().expect("staged skip-worktree index writes");
    fs::remove_file(fixture.root().join(TRACKED_PATH))
        .expect("staged skip-worktree fixture file removes");
}

pub(super) fn invalid_utf8_commit(repository: &Repository, parent: Oid) -> Oid {
    let tree = repository
        .find_commit(parent)
        .expect("fixture parent commit exists")
        .tree_id();
    let mut raw = format!("tree {tree}\nparent {parent}\nauthor ").into_bytes();
    raw.extend_from_slice(b"bad\xff <bad\xff@example.test> 0 +0000\n");
    raw.extend_from_slice(b"committer Signalbox <fixer@example.test> 0 +0000\n\n");
    raw.extend_from_slice(b"message-\xff\n");
    repository
        .odb()
        .expect("fixture object database opens")
        .write(ObjectType::Commit, &raw)
        .expect("invalid UTF-8 fixture commit writes")
}

pub(super) fn raw_message_commit(repository: &Repository, parent: Oid) -> Oid {
    let tree = repository
        .find_commit(parent)
        .expect("fixture parent commit exists")
        .tree_id();
    let raw = format!(
        "tree {tree}\nparent {parent}\nauthor Signalbox <fixer@example.test> 0 +0000\ncommitter Signalbox <fixer@example.test> 0 +0000\n\n\n\nmessage\n"
    );
    repository
        .odb()
        .expect("fixture object database opens")
        .write(ObjectType::Commit, raw.as_bytes())
        .expect("raw-message fixture commit writes")
}

pub(super) fn commit_with_parents(repository: &Repository, parents: &[Oid], message: &str) -> Oid {
    let tree_id = repository
        .find_commit(parents[0])
        .expect("fixture parent exists")
        .tree_id();
    let tree = repository.find_tree(tree_id).expect("fixture tree exists");
    let parent_commits = parents
        .iter()
        .map(|parent| {
            repository
                .find_commit(*parent)
                .expect("fixture parent exists")
        })
        .collect::<Vec<_>>();
    let parent_refs = parent_commits.iter().collect::<Vec<_>>();
    let signature =
        Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).expect("fixture signature constructs");
    repository
        .commit(None, &signature, &signature, message, &tree, &parent_refs)
        .expect("fixture commit writes")
}

pub(super) fn deep_full_path_tree_commit(repository: &Repository, parent: Oid) -> Oid {
    let blob = repository.blob(b"bounded\n").expect("fixture blob writes");
    let mut builder = repository
        .treebuilder(None)
        .expect("leaf tree builder opens");
    builder
        .insert("leaf", blob, 0o100644)
        .expect("leaf inserts");
    let mut tree = builder.write().expect("leaf tree writes");
    let component = "d".repeat(200);
    for _depth in 0..256 {
        let mut builder = repository
            .treebuilder(None)
            .expect("deep tree builder opens");
        builder
            .insert(&component, tree, 0o040000)
            .expect("deep tree inserts");
        tree = builder.write().expect("deep tree writes");
    }
    raw_commit_with_tree(repository, tree, parent)
}

pub(super) fn raw_commit_with_tree(repository: &Repository, tree: Oid, parent: Oid) -> Oid {
    let raw = format!(
        "tree {tree}\nparent {parent}\nauthor Signalbox <fixer@example.test> 0 +0000\ncommitter Signalbox <fixer@example.test> 0 +0000\n\ndeep tree\n"
    );
    repository
        .odb()
        .expect("fixture object database opens")
        .write(ObjectType::Commit, raw.as_bytes())
        .expect("fixture commit object writes")
}

pub(super) fn plant_linear_history(repository: &Repository, mut parent: Oid, count: usize) -> Oid {
    for sequence in 0..count {
        let tree = repository
            .find_commit(parent)
            .expect("linear-history parent exists")
            .tree_id();
        let raw = format!(
            "tree {tree}\nparent {parent}\nauthor Signalbox <fixer@example.test> {sequence} +0000\ncommitter Signalbox <fixer@example.test> {sequence} +0000\n\nlinear {sequence}\n"
        );
        parent = repository
            .odb()
            .expect("fixture object database opens")
            .write(ObjectType::Commit, raw.as_bytes())
            .expect("linear-history commit writes");
    }
    parent
}

pub(super) fn execute(
    executor: &LocalGitExecutor<LocalWorkspaceFileSystem>,
    operation: LocalOperation,
) -> serde_json::Value {
    let encoded = executor
        .execute_operation(operation)
        .expect("operation succeeds");
    serde_json::from_str(&encoded).expect("result is JSON")
}

pub(super) fn repository_uses_pinned_config_without_fifo_wait(
    executor: LocalGitExecutor<LocalWorkspaceFileSystem>,
    replacement_config: PathBuf,
) -> bool {
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let opened = executor
            .repository_authority
            .repository()
            .map(|repository| !repository.is_bare())
            .unwrap_or(false);
        sender.send(opened).expect("fixture result sends");
    });
    match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(opened) => {
            worker.join().expect("fixture worker joins");
            opened
        }
        Err(_) => {
            let unblock = openat(
                CWD,
                replacement_config,
                OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .expect("replacement FIFO unblocks");
            drop(unblock);
            worker.join().expect("blocked fixture worker joins");
            false
        }
    }
}

pub(super) fn commit_rejects_reflog_without_wait(
    executor: LocalGitExecutor<LocalWorkspaceFileSystem>,
    fifo_path: PathBuf,
) -> bool {
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let rejected = executor
            .execute_operation(LocalOperation::Commit(GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            }))
            .is_err();
        sender.send(rejected).expect("fixture result sends");
    });
    match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(rejected) => {
            worker.join().expect("fixture worker joins");
            rejected
        }
        Err(_) => {
            let unblock = openat(
                CWD,
                &fifo_path,
                OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            );
            worker.join().expect("fixture worker joins after unblock");
            drop(unblock);
            false
        }
    }
}

pub(super) fn status_uses_bound_index_without_fifo_wait(
    executor: LocalGitExecutor<LocalWorkspaceFileSystem>,
    index_path: PathBuf,
) -> bool {
    let (ready_sender, ready_receiver) = mpsc::channel();
    let (proceed_sender, proceed_receiver) = mpsc::channel();
    let (result_sender, result_receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let repository = executor
            .repository_authority
            .repository()
            .expect("pinned fixture repository opens");
        let _index_lock = executor
            .bind_locked_index(&repository)
            .expect("fixture index binds");
        ready_sender.send(()).expect("fixture readiness sends");
        proceed_receiver
            .recv()
            .expect("fixture continuation receives");
        result_sender
            .send(
                status(
                    &repository,
                    &executor.repository_authority,
                    &executor.filesystem,
                    &executor.root,
                    Vec::new(),
                )
                .is_ok(),
            )
            .expect("fixture status result sends");
    });
    ready_receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("fixture index binds in time");
    fs::remove_file(&index_path).expect("repository index removes for fixture");
    mkfifoat(CWD, &index_path, Mode::RUSR | Mode::WUSR).expect("replacement index FIFO constructs");
    proceed_sender.send(()).expect("fixture continuation sends");
    match result_receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(completed) => {
            worker.join().expect("fixture worker joins");
            completed
        }
        Err(_) => {
            let unblock = openat(
                CWD,
                index_path,
                OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .expect("replacement FIFO unblocks");
            drop(unblock);
            worker.join().expect("blocked fixture worker joins");
            false
        }
    }
}
