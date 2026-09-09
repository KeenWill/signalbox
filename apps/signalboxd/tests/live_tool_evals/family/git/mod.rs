//! Git evaluation fixtures and verification.

use crate::*;

mod tests;

pub(crate) const GIT_AUTHOR_NAME: &str = "Signalbox Tool Eval";
pub(crate) const GIT_AUTHOR_EMAIL: &str = "signalbox-tool-eval@example.test";
pub(crate) const SYNTHETIC_OTHER_GIT_AUTHOR_NAME: &str = "Synthetic Other Author";
pub(crate) const SYNTHETIC_OTHER_GIT_AUTHOR_EMAIL: &str = "other-author@example.test";
pub(crate) const SYNTHETIC_GIT_CONFIG_KEY: &str = "signalbox.synthetic";
pub(crate) const SYNTHETIC_GIT_CONFIG_VALUE: &str = "drifted";
pub(crate) const GIT_SEED_PATH: &str = "seed.txt";
pub(crate) const GIT_STAGE_PATH: &str = "stage-me.txt";
pub(crate) const GIT_STAGE_CONTENT: &str = "stage me\n";
pub(crate) const GIT_WRONG_STAGE_CONTENT: &str = "wrong staged bytes\n";
pub(crate) const GIT_COMMIT_PATH: &str = "commit-me.txt";
pub(crate) const GIT_COMMIT_CONTENT: &str = "commit me\n";
pub(crate) const GIT_DIFF_OVERFLOW_PATH: &str = "zz-diff-overflow.txt";
pub(crate) const GIT_DIFF_OVERFLOW_BYTE: char = 'x';
pub(crate) const GIT_DIFF_OVERFLOW_CONTENT_BYTES: usize = MAX_DIFF_BYTES + 1;
pub(crate) const GIT_STATUS_OVERFLOW_DIRECTORY: &str = "status-overflow";
pub(crate) const GIT_STATUS_OVERFLOW_CONTENT: &str = "status overflow fixture\n";
pub(crate) const GIT_STATUS_OVERFLOW_ENTRY_COUNT: usize = MAX_STATUS_ENTRIES - 2;
pub(crate) const GIT_NATURAL_PATH: &str = "eval.txt";
pub(crate) const GIT_NATURAL_CONTENT: &str = "natural eval\n";
pub(crate) const GIT_NATURAL_STAGED_PATH_COUNT: usize = 1;
pub(crate) const GIT_DRIFTED_NATURAL_CONTENT: &str = "drifted eval\n";
pub(crate) const GIT_COLLATERAL_PATH: &str = "collateral.txt";
pub(crate) const GIT_COLLATERAL_CONTENT: &str = "collateral\n";
pub(crate) const GIT_COLLATERAL_OBJECT_CONTENT: &[u8] = b"collateral object";
pub(crate) const GIT_COLLATERAL_DIRECTORY: &str = "collateral-directory";
pub(crate) const GIT_BRANCHES_DIRECTORY: &str = "branches";
pub(crate) const GIT_HOOKS_DIRECTORY: &str = "hooks";
pub(crate) const GIT_LOGS_DIRECTORY: &str = "logs";
pub(crate) const GIT_REFS_DIRECTORY: &str = "refs";
pub(crate) const GIT_HEAD_PATH: &str = "HEAD";
pub(crate) const GIT_INDEX_PATH: &str = "index";
pub(crate) const GIT_DESCRIPTION_PATH: &str = "description";
pub(crate) const GIT_PRE_COMMIT_HOOK_PATH: &str = "hooks/pre-commit";
pub(crate) const GIT_PRE_COMMIT_HOOK_CONTENT: &str = "#!/bin/sh\nexit 1\n";
pub(crate) const GIT_NATURAL_MESSAGE: &str = "tool eval commit";
pub(crate) const GIT_SWITCH_CONTENT: &str = "seed two\n";
pub(crate) const GIT_BASE_CONTENT: &str = "seed three\n";
pub(crate) const GIT_BASE_BRANCH: &str = "eval-base";
pub(crate) const GIT_COMMIT_REFLOG_MESSAGE: &str = "commit";
pub(crate) const GIT_SWITCH_REFLOG_MESSAGE: &str = "checkout: moving to configured local branch";
pub(crate) const GIT_RESTORE_BRANCH_REFLOG_MESSAGE: &str = "restore synthetic seeded branch";
pub(crate) const GIT_MERGE_HEAD_PATH: &str = "MERGE_HEAD";
pub(crate) const GIT_MERGE_MESSAGE_PATH: &str = "MERGE_MSG";
pub(crate) const GIT_MERGE_MODE_PATH: &str = "MERGE_MODE";
pub(crate) const GIT_CHERRY_PICK_HEAD_PATH: &str = "CHERRY_PICK_HEAD";
pub(crate) const GIT_CONFIG_PATH: &str = "config";
pub(crate) const GIT_OBJECTS_DIRECTORY: &str = "objects";
pub(crate) const GIT_MERGE_MESSAGE: &str = "synthetic forced merge\n";
pub(crate) const GIT_MERGE_MODE: &str = "";
pub(crate) const GIT_REGULAR_FILE_MODE: i32 = 0o100644;
pub(crate) const GIT_REGULAR_INDEX_FILE_MODE: u32 = 0o100644;
pub(crate) const GIT_INDEX_EXTENDED_FLAG: u16 = 0x4000;
pub(crate) const GIT_INDEX_SKIP_WORKTREE_FLAG: u16 = 0x4000;
pub(crate) const GIT_INDEX_HEADER_BYTES: usize = 12;
pub(crate) const GIT_INDEX_OBJECT_ID_BYTES: usize = 20;
pub(crate) const GIT_INDEX_ENTRY_FIELDS_BEFORE_ID_BYTES: usize = 40;
pub(crate) const GIT_INDEX_ENTRY_FLAGS_BYTES: usize = 2;
pub(crate) const GIT_INDEX_EXTENDED_FLAGS_BYTES: usize = 2;
pub(crate) const GIT_INDEX_EXTENSION_HEADER_BYTES: usize = 8;
pub(crate) const SYNTHETIC_GIT_INDEX_EXTENSION_SIGNATURE: [u8; 4] = *b"ZZZZ";
pub(crate) const SYNTHETIC_GIT_INDEX_EXTENSION_CONTENT: &[u8] = b"synthetic optional extension";
pub(crate) const SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS: i64 = 1_700_000_000;
pub(crate) const SYNTHETIC_GIT_EXECUTION_FINISHED_SECONDS: i64 = 1_700_000_002;
pub(crate) const SYNTHETIC_GIT_EXECUTION_RECORDED_SECONDS: i64 = 1_700_000_001;
pub(crate) const SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET: i32 = -300;
pub(crate) const SYNTHETIC_OTHER_GIT_TIMEZONE_OFFSET: i32 = 60;
pub(crate) const SYNTHETIC_WRONG_STAGED_PATH_COUNT: usize = 0;
pub(crate) const SYNTHETIC_WRONG_COMMIT_ID: &str = "synthetic-wrong-commit-id";
pub(crate) const GIT_NATURAL_STAGE_REQUEST_ENTRY_INDEX: u64 = 1;
pub(crate) const GIT_NATURAL_STAGE_RESULT_ENTRY_INDEX: u64 = 2;
pub(crate) const GIT_NATURAL_COMMIT_REQUEST_ENTRY_INDEX: u64 = 3;
pub(crate) const GIT_NATURAL_COMMIT_RESULT_ENTRY_INDEX: u64 = 4;
pub(crate) const GIT_CASES: &[ForcedCase] = &[
    ForcedCase {
        name: GIT_BRANCH_CREATE_NAME,
        expected_arguments: r#"{"name":"created-by-eval","start":"refs/heads/log-target"}"#,
        prompt: "Call git_branch_create with exactly {\"name\":\"created-by-eval\",\"start\":\"refs/heads/log-target\"}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: GIT_BRANCH_SWITCH_NAME,
        expected_arguments: r#"{"name":"switch-target"}"#,
        prompt: "Call git_branch_switch with exactly {\"name\":\"switch-target\"}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: GIT_CREATE_COMMIT_NAME,
        expected_arguments: r#"{"message":"forced eval commit"}"#,
        prompt: "Call git_create_commit with exactly {\"message\":\"forced eval commit\"}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: GIT_DIFF_NAME,
        expected_arguments: r#"{"scope":"worktree"}"#,
        prompt: "Call git_diff with exactly {\"scope\":\"worktree\"}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: GIT_LOG_NAME,
        expected_arguments: r#"{"revision":"refs/heads/log-target","max_entries":1}"#,
        prompt: "Call git_log with exactly {\"revision\":\"refs/heads/log-target\",\"max_entries\":1}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: GIT_STAGE_NAME,
        expected_arguments: r#"{"paths":["stage-me.txt"]}"#,
        prompt: "Call git_stage with exactly {\"paths\":[\"stage-me.txt\"]}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: GIT_STATUS_NAME,
        expected_arguments: "{}",
        prompt: "Call git_status with exactly {}. After its result, answer done without another tool call.",
    },
];

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct GitFixtureSnapshot {
    pub(crate) modes: BTreeMap<PathBuf, Option<u32>>,
    pub(crate) config: Vec<u8>,
    pub(crate) worktree_entries: BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    pub(crate) worktree_modified_times: BTreeMap<PathBuf, SystemTime>,
    pub(crate) worktree_entry_identities: BTreeMap<PathBuf, FilesystemIdentity>,
    pub(crate) worktree_extended_attributes: BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
    pub(crate) metadata_extended_attributes: BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
    pub(crate) metadata_root_kind: GitMetadataEntryKind,
    pub(crate) metadata_root_mode: Option<u32>,
    pub(crate) metadata_root_modified_time: Option<SystemTime>,
    pub(crate) metadata_root_identity: Option<FilesystemIdentity>,
    pub(crate) metadata_top_level: BTreeMap<PathBuf, GitMetadataEntrySnapshot>,
    pub(crate) index_entries: Vec<GitIndexEntrySnapshot>,
    pub(crate) index_complete_entries: Vec<GitIndexCompleteEntrySnapshot>,
    pub(crate) index_extensions: Vec<GitIndexExtensionSnapshot>,
    pub(crate) static_metadata_entries: BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    pub(crate) static_metadata_modified_times: BTreeMap<PathBuf, SystemTime>,
    pub(crate) static_metadata_entry_identities: BTreeMap<PathBuf, FilesystemIdentity>,
    pub(crate) reflog_entries: BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    pub(crate) reflog_modified_times: BTreeMap<PathBuf, SystemTime>,
    pub(crate) reflog_entry_identities: BTreeMap<PathBuf, FilesystemIdentity>,
    pub(crate) reference_entries: BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    pub(crate) reference_modified_times: BTreeMap<PathBuf, SystemTime>,
    pub(crate) reference_entry_identities: BTreeMap<PathBuf, FilesystemIdentity>,
    pub(crate) objects: GitObjectInventory,
    pub(crate) object_entries: BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    pub(crate) object_modified_times: BTreeMap<PathBuf, SystemTime>,
    pub(crate) object_entry_identities: BTreeMap<PathBuf, FilesystemIdentity>,
}

pub(crate) type GitObjectInventory = BTreeMap<Oid, GitObjectSnapshot>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitObjectSnapshot {
    pub(crate) kind: ObjectType,
    pub(crate) content: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum GitMetadataEntryKind {
    #[default]
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitMetadataEntrySnapshot {
    pub(crate) kind: GitMetadataEntryKind,
    pub(crate) mode: Option<u32>,
    pub(crate) links: Option<u64>,
    pub(crate) content: Option<Vec<u8>>,
    pub(crate) modified: Option<SystemTime>,
    pub(crate) identity: Option<FilesystemIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitIndexEntrySnapshot {
    pub(crate) path: Vec<u8>,
    pub(crate) id: Oid,
    pub(crate) mode: u32,
    pub(crate) flags: u16,
    pub(crate) flags_extended: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitIndexCompleteEntrySnapshot {
    pub(crate) semantic: GitIndexEntrySnapshot,
    pub(crate) ctime: IndexTime,
    pub(crate) mtime: IndexTime,
    pub(crate) dev: u32,
    pub(crate) ino: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) file_size: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitIndexExtensionSnapshot {
    pub(crate) signature: [u8; 4],
    pub(crate) content: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GitRecordedTime {
    pub(crate) seconds: i64,
    pub(crate) offset_minutes: i32,
}

impl From<Time> for GitRecordedTime {
    fn from(time: Time) -> Self {
        Self {
            seconds: time.seconds(),
            offset_minutes: time.offset_minutes(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GitExecutionTimeWindow {
    pub(crate) started: GitRecordedTime,
    pub(crate) finished: GitRecordedTime,
}

pub(crate) fn synthetic_filesystem_identity(change_time_nanoseconds: i64) -> FilesystemIdentity {
    FilesystemIdentity {
        device: 1,
        inode: 1,
        user_id: 1,
        group_id: 1,
        change_time_seconds: 1,
        change_time_nanoseconds,
    }
}

impl GitExecutionTimeWindow {
    pub(crate) fn contains(self, time: Time) -> bool {
        let time = GitRecordedTime::from(time);
        (self.started.seconds..=self.finished.seconds).contains(&time.seconds)
            && matches!(
                time.offset_minutes,
                offset if offset == self.started.offset_minutes || offset == self.finished.offset_minutes
            )
    }
}

pub(crate) fn git_commit_times_match_execution(
    author: Time,
    committer: Time,
    execution_window: Option<GitExecutionTimeWindow>,
) -> bool {
    let author_time = GitRecordedTime::from(author);
    let committer_time = GitRecordedTime::from(committer);
    author_time == committer_time
        && execution_window
            .is_some_and(|window| window.contains(author) && window.contains(committer))
}

pub(crate) type GitReferenceInventory = BTreeMap<Vec<u8>, GitReferenceTarget>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GitReferenceTarget {
    Direct(Oid),
    Symbolic(Vec<u8>),
}

impl FamilySuite {
    pub(crate) fn git() -> EvalResult<Self> {
        let workspace = tempfile::tempdir()?;
        let git_seed = seed_git_repository(workspace.path())?;
        let git_seed_refs = git_reference_inventory(&Repository::open(workspace.path())?)?;
        let git_seed_fixture = git_fixture_snapshot(workspace.path())?;
        let tools = LocalGitTools::try_new(
            LocalWorkspaceFileSystem,
            workspace.path(),
            GitIdentity::try_new(GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL)?,
        )?;
        let (catalog, executor) = tools.into_parts();
        let git_pre_execution_object_entries = Arc::new(StdMutex::new(None));
        let git_pre_execution_object_modified_times = Arc::new(StdMutex::new(None));
        let git_pre_execution_object_entry_identities = Arc::new(StdMutex::new(None));
        let executor = SharedFamilyExecutor::new(FamilyExecutor::Git(executor)).with_git_capture(
            workspace.path().to_path_buf(),
            Arc::clone(&git_pre_execution_object_entries),
            Arc::clone(&git_pre_execution_object_modified_times),
            Arc::clone(&git_pre_execution_object_entry_identities),
        );
        Ok(Self {
            family: EvalFamily::Git,
            workspace,
            git_seed: Some(git_seed),
            git_seed_refs,
            git_seed_fixture,
            catalog: MergedCatalog::try_new([catalog])?,
            executor,
            workspace_seed_entries: BTreeMap::new(),
            workspace_seed_modified_times: BTreeMap::new(),
            workspace_seed_entry_identities: BTreeMap::new(),
            workspace_seed_extended_attributes: BTreeMap::new(),
            workspace_seed_inode_flags: BTreeMap::new(),
            git_pre_execution_worktree_entries: StdMutex::new(None),
            git_pre_execution_worktree_modified_times: StdMutex::new(None),
            git_pre_execution_worktree_entry_identities: StdMutex::new(None),
            git_pre_execution_worktree_extended_attributes: StdMutex::new(None),
            git_pre_execution_metadata_extended_attributes: StdMutex::new(None),
            git_pre_execution_index_entries: StdMutex::new(None),
            git_pre_execution_metadata_root_modified_time: StdMutex::new(None),
            git_pre_execution_metadata_root_identity: StdMutex::new(None),
            git_pre_execution_metadata_top_level: StdMutex::new(None),
            git_pre_execution_objects: StdMutex::new(None),
            git_pre_execution_object_entries,
            git_pre_execution_object_modified_times,
            git_pre_execution_object_entry_identities,
        })
    }

    pub(crate) fn prepare_git_case(&self, tool: &str) -> EvalResult {
        if self.family == EvalFamily::Git {
            match tool {
                GIT_CREATE_COMMIT_NAME => {
                    stage_path(self.workspace.path(), GIT_COMMIT_PATH)?;
                    let seed = self.git_seed.ok_or_else(|| {
                        io::Error::other("the Git eval suite has no captured seed identity")
                    })?;
                    install_git_merge_state(self.workspace.path(), seed)?;
                }
                GIT_DIFF_NAME => {
                    stage_path(self.workspace.path(), GIT_STAGE_PATH)?;
                    fs::write(
                        self.workspace.path().join(GIT_DIFF_OVERFLOW_PATH),
                        git_diff_overflow_content(),
                    )?;
                }
                GIT_STATUS_NAME => {
                    fs::create_dir(self.workspace.path().join(GIT_STATUS_OVERFLOW_DIRECTORY))?;
                    for index in 0..GIT_STATUS_OVERFLOW_ENTRY_COUNT {
                        fs::write(
                            self.workspace.path().join(git_status_overflow_path(index)),
                            GIT_STATUS_OVERFLOW_CONTENT,
                        )?;
                    }
                }
                _ => {}
            }
            *self
                .git_pre_execution_worktree_entries
                .lock()
                .expect("Git pre-execution inventory lock is available") =
                Some(git_worktree_entries(self.workspace.path())?);
            *self
                .git_pre_execution_worktree_modified_times
                .lock()
                .expect("Git pre-execution worktree-time lock is available") =
                Some(git_worktree_modified_times(self.workspace.path())?);
            *self
                .git_pre_execution_worktree_entry_identities
                .lock()
                .expect("Git pre-execution worktree-identity lock is available") =
                Some(git_worktree_entry_identities(self.workspace.path())?);
            *self
                .git_pre_execution_worktree_extended_attributes
                .lock()
                .expect("Git pre-execution worktree-attribute lock is available") =
                Some(git_worktree_extended_attributes(self.workspace.path())?);
            *self
                .git_pre_execution_metadata_extended_attributes
                .lock()
                .expect("Git pre-execution metadata-attribute lock is available") =
                Some(git_metadata_extended_attributes(self.workspace.path())?);
            *self
                .git_pre_execution_index_entries
                .lock()
                .expect("Git pre-execution index lock is available") = Some(
                git_index_complete_entries(&Repository::open(self.workspace.path())?)?,
            );
            *self
                .git_pre_execution_metadata_root_modified_time
                .lock()
                .expect("Git pre-execution metadata-root-time lock is available") =
                Some(git_metadata_root_modified_time(self.workspace.path())?);
            *self
                .git_pre_execution_metadata_root_identity
                .lock()
                .expect("Git pre-execution metadata-root-identity lock is available") =
                git_metadata_root_identity(self.workspace.path())?;
            *self
                .git_pre_execution_metadata_top_level
                .lock()
                .expect("Git pre-execution metadata lock is available") =
                Some(git_metadata_top_level(self.workspace.path())?);
            *self
                .git_pre_execution_objects
                .lock()
                .expect("Git pre-execution object lock is available") = Some(git_object_inventory(
                &Repository::open(self.workspace.path())?,
            )?);
            *self
                .git_pre_execution_object_entries
                .lock()
                .expect("Git pre-execution object-entry lock is available") =
                Some(git_object_entries(self.workspace.path())?);
            *self
                .git_pre_execution_object_modified_times
                .lock()
                .expect("Git pre-execution object-time lock is available") =
                Some(git_object_modified_times(self.workspace.path())?);
            *self
                .git_pre_execution_object_entry_identities
                .lock()
                .expect("Git pre-execution object-identity lock is available") =
                Some(git_object_entry_identities(self.workspace.path())?);
        }
        Ok(())
    }

    pub(crate) fn commit_staged_paths_for_test(&self, message: &str) -> EvalResult {
        self.commit_staged_paths_with_identity_for_test(message, GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL)
    }

    pub(crate) fn commit_staged_paths_with_identity_for_test(
        &self,
        message: &str,
        author_name: &str,
        author_email: &str,
    ) -> EvalResult {
        let started = current_git_recorded_time()?;
        let filesystem_started = current_filesystem_recorded_time()?;
        commit_staged_paths_with_identity(
            self.workspace.path(),
            message,
            author_name,
            author_email,
        )?;
        let finished = current_git_recorded_time()?;
        self.executor.record_git_execution_window(
            GIT_CREATE_COMMIT_NAME,
            GitExecutionTimeWindow { started, finished },
        );
        self.executor.record_filesystem_execution_window(
            GIT_CREATE_COMMIT_NAME,
            FilesystemExecutionTimeWindow {
                started: filesystem_started,
                finished: current_filesystem_recorded_time()?,
            },
        );
        Ok(())
    }
}

pub(crate) fn git_worktree_entries(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, WorkspaceEntrySnapshot>> {
    filesystem_entries(root, Some(Path::new(".git")))
}

pub(crate) fn git_worktree_extended_attributes(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, ExtendedAttributeSnapshot>> {
    filesystem_extended_attributes(root, Some(Path::new(".git")))
}

pub(crate) fn git_worktree_entry_identities(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, FilesystemIdentity>> {
    filesystem_entry_identities(root, Some(Path::new(".git")))
}

pub(crate) fn git_worktree_modified_times(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, SystemTime>> {
    filesystem_modified_times(root, Some(Path::new(".git")))
}

pub(crate) struct GitForcedVerification<'a> {
    pub(crate) root: &'a Path,
    pub(crate) seed: Oid,
    pub(crate) seed_refs: &'a GitReferenceInventory,
    pub(crate) seed_fixture: &'a GitFixtureSnapshot,
    pub(crate) pre_execution_worktree_entries:
        Option<&'a BTreeMap<PathBuf, WorkspaceEntrySnapshot>>,
    pub(crate) pre_execution_worktree_modified_times: Option<&'a BTreeMap<PathBuf, SystemTime>>,
    pub(crate) pre_execution_worktree_entry_identities:
        Option<&'a BTreeMap<PathBuf, FilesystemIdentity>>,
    pub(crate) pre_execution_worktree_extended_attributes:
        Option<&'a BTreeMap<PathBuf, ExtendedAttributeSnapshot>>,
    pub(crate) pre_execution_metadata_extended_attributes:
        Option<&'a BTreeMap<PathBuf, ExtendedAttributeSnapshot>>,
    pub(crate) pre_execution_index_entries: Option<&'a [GitIndexCompleteEntrySnapshot]>,
    pub(crate) pre_execution_metadata_root_modified_time: Option<SystemTime>,
    pub(crate) pre_execution_metadata_root_identity: Option<FilesystemIdentity>,
    pub(crate) pre_execution_metadata_top_level:
        Option<&'a BTreeMap<PathBuf, GitMetadataEntrySnapshot>>,
    pub(crate) pre_execution_objects: Option<&'a GitObjectInventory>,
    pub(crate) pre_execution_object_entries: Option<&'a BTreeMap<PathBuf, WorkspaceEntrySnapshot>>,
    pub(crate) pre_execution_object_modified_times: Option<&'a BTreeMap<PathBuf, SystemTime>>,
    pub(crate) pre_execution_object_entry_identities:
        Option<&'a BTreeMap<PathBuf, FilesystemIdentity>>,
    pub(crate) execution_window: Option<GitExecutionTimeWindow>,
    pub(crate) filesystem_execution_window: Option<FilesystemExecutionTimeWindow>,
}

pub(crate) fn git_forced_case_passed(
    verification: GitForcedVerification<'_>,
    name: &str,
    arguments: &serde_json::Value,
    result: &serde_json::Value,
) -> EvalResult<bool> {
    let GitForcedVerification {
        root,
        seed,
        seed_refs,
        seed_fixture,
        pre_execution_worktree_entries,
        pre_execution_worktree_modified_times,
        pre_execution_worktree_entry_identities,
        pre_execution_worktree_extended_attributes,
        pre_execution_metadata_extended_attributes,
        pre_execution_index_entries,
        pre_execution_metadata_root_modified_time,
        pre_execution_metadata_root_identity,
        pre_execution_metadata_top_level,
        pre_execution_objects,
        pre_execution_object_entries,
        pre_execution_object_modified_times,
        pre_execution_object_entry_identities,
        execution_window,
        filesystem_execution_window,
    } = verification;
    let repository = Repository::open(root)?;
    let head = repository.head()?.peel_to_commit()?;
    let seed_commit = repository.find_commit(seed)?;
    let log_target = seed_commit.parent(0)?;
    let expected_fields: &[&str] = match name {
        GIT_BRANCH_CREATE_NAME | GIT_BRANCH_SWITCH_NAME => &["branch", "head", EVAL_RECEIPT_FIELD],
        GIT_CREATE_COMMIT_NAME => &["commit", "state_cleaned", EVAL_RECEIPT_FIELD],
        GIT_DIFF_NAME => &["patch", "truncated", EVAL_RECEIPT_FIELD],
        GIT_LOG_NAME => &["commits", "truncated", EVAL_RECEIPT_FIELD],
        GIT_STAGE_NAME => &["staged_paths", EVAL_RECEIPT_FIELD],
        GIT_STATUS_NAME => &[
            "branch",
            "branch_truncated",
            "head",
            "entries",
            "truncated",
            EVAL_RECEIPT_FIELD,
        ],
        _ => return Ok(false),
    };
    if !json_object_has_exact_fields(result, expected_fields) {
        return Ok(false);
    }
    let passed = match name {
        GIT_BRANCH_CREATE_NAME => {
            let Some(branch_name) = arguments["name"].as_str() else {
                return Ok(false);
            };
            let Some(_start) = arguments["start"].as_str() else {
                return Ok(false);
            };
            let expected = log_target.id();
            let mut expected_refs = seed_refs.clone();
            expected_refs.insert(
                format!("refs/heads/{branch_name}").into_bytes(),
                GitReferenceTarget::Direct(expected),
            );
            let branch = repository.find_branch(branch_name, BranchType::Local)?;
            let start_branch = repository.find_branch("log-target", BranchType::Local)?;
            let base = repository
                .find_branch(GIT_BASE_BRANCH, BranchType::Local)?
                .into_reference()
                .peel_to_commit()?;
            result["branch"] == branch_name
                && branch.get().target() == Some(expected)
                && start_branch.get().target() == Some(expected)
                && result["head"] == expected.to_string()
                && repository.head()?.shorthand().ok() == Some(GIT_BASE_BRANCH)
                && head.id() == seed
                && base.id() == seed
                && fs::read_to_string(root.join(GIT_SEED_PATH))? == GIT_BASE_CONTENT
                && repository.status_file(Path::new(GIT_SEED_PATH))? == Status::CURRENT
                && git_reference_inventory(&repository)? == expected_refs
                && git_base_status_fixture_unchanged(root, &repository)?
                && git_status_path_states(&repository)? == expected_base_git_statuses()
        }
        GIT_BRANCH_SWITCH_NAME => {
            let Some(branch_name) = arguments["name"].as_str() else {
                return Ok(false);
            };
            let branch = repository.find_branch(branch_name, BranchType::Local)?;
            let base = repository.find_branch(GIT_BASE_BRANCH, BranchType::Local)?;
            let branch_target = branch.get().target();
            result["branch"] == branch_name
                && branch_target == Some(log_target.id())
                && base.get().target() == Some(seed)
                && head.id() == log_target.id()
                && result["head"] == log_target.id().to_string()
                && repository.head()?.shorthand().ok() == Some(branch_name)
                && fs::read_to_string(root.join(GIT_SEED_PATH))? == GIT_SWITCH_CONTENT
                && repository.status_file(Path::new(GIT_SEED_PATH))? == Status::CURRENT
                && git_reference_inventory(&repository)? == *seed_refs
                && git_untracked_fixtures_unchanged(root, &repository)?
                && git_status_path_states(&repository)? == expected_base_git_statuses()
        }
        GIT_CREATE_COMMIT_NAME => {
            let base = repository.find_branch(GIT_BASE_BRANCH, BranchType::Local)?;
            let mut expected_refs = seed_refs.clone();
            expected_refs.insert(
                format!("refs/heads/{GIT_BASE_BRANCH}").into_bytes(),
                GitReferenceTarget::Direct(head.id()),
            );
            result["commit"] == head.id().to_string()
                && result["state_cleaned"] == true
                && repository.head()?.shorthand().ok() == Some(GIT_BASE_BRANCH)
                && base.get().target() == Some(head.id())
                && head.message().ok() == arguments["message"].as_str()
                && head.author().name().ok() == Some(GIT_AUTHOR_NAME)
                && head.author().email().ok() == Some(GIT_AUTHOR_EMAIL)
                && head.committer().name().ok() == Some(GIT_AUTHOR_NAME)
                && head.committer().email().ok() == Some(GIT_AUTHOR_EMAIL)
                && git_commit_times_match_execution(
                    head.author().when(),
                    head.committer().when(),
                    execution_window,
                )
                && commit_adds_exact_fixture(
                    &repository,
                    &head,
                    GIT_COMMIT_PATH,
                    GIT_COMMIT_CONTENT.as_bytes(),
                    2,
                )?
                && head.parent_id(0)? == seed
                && head.parent_id(1)? == log_target.id()
                && git_operation_state_is_clean(&repository)
                && untracked_git_fixture_matches(
                    root,
                    &repository,
                    GIT_STAGE_PATH,
                    GIT_STAGE_CONTENT.as_bytes(),
                )?
                && untracked_git_fixture_matches(
                    root,
                    &repository,
                    GIT_NATURAL_PATH,
                    GIT_NATURAL_CONTENT.as_bytes(),
                )?
                && git_reference_inventory(&repository)? == expected_refs
                && git_status_path_states(&repository)? == expected_commit_git_statuses()
        }
        GIT_DIFF_NAME => {
            result["patch"].as_str() == Some(expected_bounded_git_worktree_patch(root)?.as_str())
                && result["truncated"] == true
                && repository.head()?.shorthand().ok() == Some(GIT_BASE_BRANCH)
                && head.id() == seed
                && git_diff_fixture_unchanged(root, &repository)?
                && git_reference_inventory(&repository)? == *seed_refs
                && git_status_path_states(&repository)? == expected_diff_git_statuses()
        }
        GIT_LOG_NAME => {
            let Some(_revision) = arguments["revision"].as_str() else {
                return Ok(false);
            };
            let Some(max_entries) = arguments["max_entries"].as_u64() else {
                return Ok(false);
            };
            let target_branch = repository.find_branch("log-target", BranchType::Local)?;
            result["commits"].as_array().is_some_and(|commits| {
                u64::try_from(commits.len()).ok() == Some(max_entries)
                    && commits.first().is_some_and(|commit| {
                        json_object_has_exact_fields(
                            commit,
                            &[
                                "commit",
                                "author_name",
                                "author_name_truncated",
                                "author_email",
                                "author_email_truncated",
                                "message",
                                "message_truncated",
                            ],
                        ) && commit["commit"] == log_target.id().to_string()
                            && commit["author_name"]
                                == log_target.author().name().unwrap_or_default()
                            && commit["author_name_truncated"] == false
                            && commit["author_email"]
                                == log_target.author().email().unwrap_or_default()
                            && commit["author_email_truncated"] == false
                            && commit["message"] == log_target.message().unwrap_or_default()
                            && commit["message_truncated"] == false
                    })
            }) && result["truncated"] == true
                && target_branch.get().target() == Some(log_target.id())
                && repository.head()?.shorthand().ok() == Some(GIT_BASE_BRANCH)
                && head.id() == seed
                && git_base_status_fixture_unchanged(root, &repository)?
                && git_reference_inventory(&repository)? == *seed_refs
                && git_status_path_states(&repository)? == expected_base_git_statuses()
        }
        GIT_STAGE_NAME => {
            let Some(paths) = arguments["paths"].as_array() else {
                return Ok(false);
            };
            let base = repository.find_branch(GIT_BASE_BRANCH, BranchType::Local)?;
            result["staged_paths"] == paths.len()
                && paths.iter().all(|path| {
                    path.as_str().is_some_and(|path| {
                        repository.status_file(Path::new(path)).ok() == Some(Status::INDEX_NEW)
                    })
                })
                && staged_blob_matches_fixture(
                    root,
                    &repository,
                    GIT_STAGE_PATH,
                    GIT_STAGE_CONTENT.as_bytes(),
                    seed_fixture
                        .modes
                        .get(Path::new(GIT_STAGE_PATH))
                        .copied()
                        .flatten(),
                )?
                && repository.head()?.shorthand().ok() == Some(GIT_BASE_BRANCH)
                && head.id() == seed
                && base.get().target() == Some(seed)
                && repository.status_file(Path::new(GIT_SEED_PATH))? == Status::CURRENT
                && fs::read(root.join(GIT_SEED_PATH))? == GIT_BASE_CONTENT.as_bytes()
                && untracked_git_fixture_matches(
                    root,
                    &repository,
                    GIT_COMMIT_PATH,
                    GIT_COMMIT_CONTENT.as_bytes(),
                )?
                && untracked_git_fixture_matches(
                    root,
                    &repository,
                    GIT_NATURAL_PATH,
                    GIT_NATURAL_CONTENT.as_bytes(),
                )?
                && git_reference_inventory(&repository)? == *seed_refs
                && git_status_path_states(&repository)? == expected_staged_git_statuses()
        }
        GIT_STATUS_NAME => {
            result["branch"].as_str() == Some(GIT_BASE_BRANCH)
                && result["branch_truncated"] == false
                && result["head"] == seed.to_string()
                && repository.head()?.shorthand().ok() == Some(GIT_BASE_BRANCH)
                && head.id() == seed
                && git_status_entries_match(&result["entries"])
                && result["truncated"] == true
                && git_status_fixture_unchanged(root, &repository)?
                && git_reference_inventory(&repository)? == *seed_refs
                && git_status_path_states(&repository)? == expected_status_git_statuses()
        }
        _ => false,
    };
    Ok(passed
        && git_fixture_snapshot_matches(root, &repository, seed_fixture)?
        && git_forced_index_matches(
            root,
            &repository,
            name,
            seed_fixture,
            pre_execution_index_entries,
        )?
        && git_forced_metadata_root_modified_time_matches(
            root,
            name,
            seed_fixture,
            pre_execution_metadata_root_modified_time,
            pre_execution_metadata_root_identity,
            filesystem_execution_window,
        )?
        && git_forced_metadata_top_level_matches(
            root,
            name,
            seed_fixture,
            pre_execution_metadata_top_level,
            filesystem_execution_window,
        )?
        && git_forced_objects_match(
            &repository,
            name,
            &head,
            seed_fixture,
            pre_execution_objects,
        )?
        && git_forced_object_entries_match(
            root,
            name,
            &head,
            seed_fixture,
            GitObjectEntryVerification {
                pre_execution_entries: pre_execution_object_entries,
                pre_execution_modified_times: pre_execution_object_modified_times,
                pre_execution_entry_identities: pre_execution_object_entry_identities,
                execution_window: filesystem_execution_window,
            },
        )?
        && git_forced_reference_entries_match(
            root,
            name,
            arguments,
            &head,
            seed_fixture,
            filesystem_execution_window,
        )?
        && git_forced_reflogs_match(
            root,
            name,
            seed,
            head.id(),
            seed_fixture,
            execution_window,
            filesystem_execution_window,
        )?
        && git_forced_worktree_matches(root, name, seed_fixture, pre_execution_worktree_entries)?
        && git_forced_worktree_modified_times_match(
            root,
            name,
            seed_fixture,
            pre_execution_worktree_modified_times,
            filesystem_execution_window,
        )?
        && git_forced_worktree_entry_identities_match(
            root,
            name,
            seed_fixture,
            pre_execution_worktree_entry_identities,
            filesystem_execution_window,
        )?
        && git_forced_worktree_extended_attributes_match(
            root,
            seed_fixture,
            pre_execution_worktree_extended_attributes,
        )?
        && git_metadata_extended_attributes_match(
            root,
            seed_fixture,
            pre_execution_metadata_extended_attributes,
        )?)
}

pub(crate) fn commit_adds_exact_fixture(
    repository: &Repository,
    commit: &git2::Commit<'_>,
    path: &str,
    expected: &[u8],
    expected_parent_count: usize,
) -> EvalResult<bool> {
    if commit.parent_count() != expected_parent_count {
        return Ok(false);
    }
    let parent = commit.parent(0)?;
    let parent_tree = parent.tree()?;
    let tree = commit.tree()?;
    let diff = repository.diff_tree_to_tree(Some(&parent_tree), Some(&tree), None)?;
    let mut deltas = diff.deltas();
    let Some(delta) = deltas.next() else {
        return Ok(false);
    };
    let Ok(entry) = tree.get_path(Path::new(path)) else {
        return Ok(false);
    };
    let Ok(blob) = entry
        .to_object(repository)
        .and_then(|object| object.peel_to_blob())
    else {
        return Ok(false);
    };
    Ok(deltas.next().is_none()
        && delta.status() == Delta::Added
        && delta.new_file().path() == Some(Path::new(path))
        && entry.filemode() == GIT_REGULAR_FILE_MODE
        && blob.content() == expected)
}

pub(crate) fn expected_git_worktree_patch(root: &Path) -> EvalResult<String> {
    let mut expected = Vec::new();
    for path in [
        GIT_COMMIT_PATH,
        GIT_NATURAL_PATH,
        GIT_STAGE_PATH,
        GIT_DIFF_OVERFLOW_PATH,
    ] {
        let content = fs::read(root.join(path))?;
        let mut options = DiffOptions::new();
        options.force_text(true);
        let patch = Patch::from_buffers(
            b"",
            None,
            &content,
            Some(Path::new(path)),
            Some(&mut options),
        )?
        .to_buf()?;
        let first_line = patch
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|position| position + 1)
            .ok_or_else(|| io::Error::other("fixture patch has no header"))?;
        let existing_mode_end = patch[first_line..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|position| first_line + position + 1)
            .filter(|end| patch[first_line..*end].starts_with(b"new file mode "))
            .unwrap_or(first_line);
        expected.extend_from_slice(&patch[..first_line]);
        expected.extend_from_slice(b"new file mode 100644\n");
        expected.extend_from_slice(&patch[existing_mode_end..]);
    }
    String::from_utf8(expected).map_err(Into::into)
}

pub(crate) fn expected_bounded_git_worktree_patch(root: &Path) -> EvalResult<String> {
    expected_git_worktree_patch(root)?
        .get(..MAX_DIFF_BYTES)
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("the Git diff fixture does not exceed its bound").into())
}

pub(crate) fn git_diff_overflow_content() -> String {
    GIT_DIFF_OVERFLOW_BYTE
        .to_string()
        .repeat(GIT_DIFF_OVERFLOW_CONTENT_BYTES)
}

pub(crate) fn git_diff_fixture_unchanged(root: &Path, repository: &Repository) -> EvalResult<bool> {
    Ok(
        repository.status_file(Path::new(GIT_STAGE_PATH))? == Status::INDEX_NEW
            && repository.status_file(Path::new(GIT_COMMIT_PATH))? == Status::WT_NEW
            && repository.status_file(Path::new(GIT_NATURAL_PATH))? == Status::WT_NEW
            && repository.status_file(Path::new(GIT_DIFF_OVERFLOW_PATH))? == Status::WT_NEW
            && fs::read(root.join(GIT_STAGE_PATH))? == GIT_STAGE_CONTENT.as_bytes()
            && fs::read(root.join(GIT_COMMIT_PATH))? == GIT_COMMIT_CONTENT.as_bytes()
            && fs::read(root.join(GIT_NATURAL_PATH))? == GIT_NATURAL_CONTENT.as_bytes()
            && fs::read(root.join(GIT_DIFF_OVERFLOW_PATH))?
                == git_diff_overflow_content().as_bytes(),
    )
}

pub(crate) fn git_status_fixture_unchanged(
    root: &Path,
    repository: &Repository,
) -> EvalResult<bool> {
    let base_unchanged = git_base_status_fixture_unchanged(root, repository)?;
    let mut overflow_unchanged = true;
    for index in 0..GIT_STATUS_OVERFLOW_ENTRY_COUNT {
        let path = git_status_overflow_path(index);
        overflow_unchanged &= repository.status_file(Path::new(&path))? == Status::WT_NEW
            && fs::read(root.join(path))? == GIT_STATUS_OVERFLOW_CONTENT.as_bytes();
    }
    Ok(base_unchanged && overflow_unchanged)
}

pub(crate) fn git_status_path_states(
    repository: &Repository,
) -> EvalResult<BTreeMap<PathBuf, Status>> {
    let mut states = BTreeMap::new();
    for entry in repository.statuses(None)?.iter() {
        let path = entry
            .path()
            .map_err(|error| io::Error::other(format!("invalid Git status path: {error}")))?;
        states.insert(PathBuf::from(path), entry.status());
    }
    Ok(states)
}

pub(crate) fn expected_base_git_statuses() -> BTreeMap<PathBuf, Status> {
    BTreeMap::from([
        (PathBuf::from(GIT_COMMIT_PATH), Status::WT_NEW),
        (PathBuf::from(GIT_NATURAL_PATH), Status::WT_NEW),
        (PathBuf::from(GIT_STAGE_PATH), Status::WT_NEW),
    ])
}

pub(crate) fn expected_staged_git_statuses() -> BTreeMap<PathBuf, Status> {
    BTreeMap::from([
        (PathBuf::from(GIT_COMMIT_PATH), Status::WT_NEW),
        (PathBuf::from(GIT_NATURAL_PATH), Status::WT_NEW),
        (PathBuf::from(GIT_STAGE_PATH), Status::INDEX_NEW),
    ])
}

pub(crate) fn expected_commit_git_statuses() -> BTreeMap<PathBuf, Status> {
    BTreeMap::from([
        (PathBuf::from(GIT_NATURAL_PATH), Status::WT_NEW),
        (PathBuf::from(GIT_STAGE_PATH), Status::WT_NEW),
    ])
}

pub(crate) fn expected_diff_git_statuses() -> BTreeMap<PathBuf, Status> {
    BTreeMap::from([
        (PathBuf::from(GIT_COMMIT_PATH), Status::WT_NEW),
        (PathBuf::from(GIT_DIFF_OVERFLOW_PATH), Status::WT_NEW),
        (PathBuf::from(GIT_NATURAL_PATH), Status::WT_NEW),
        (PathBuf::from(GIT_STAGE_PATH), Status::INDEX_NEW),
    ])
}

pub(crate) fn expected_status_git_statuses() -> BTreeMap<PathBuf, Status> {
    let mut states = expected_base_git_statuses();
    for index in 0..GIT_STATUS_OVERFLOW_ENTRY_COUNT {
        states.insert(
            PathBuf::from(git_status_overflow_path(index)),
            Status::WT_NEW,
        );
    }
    states
}

pub(crate) fn git_base_status_fixture_unchanged(
    root: &Path,
    repository: &Repository,
) -> EvalResult<bool> {
    Ok(
        repository.status_file(Path::new(GIT_SEED_PATH))? == Status::CURRENT
            && fs::read(root.join(GIT_SEED_PATH))? == GIT_BASE_CONTENT.as_bytes()
            && git_untracked_fixtures_unchanged(root, repository)?,
    )
}

pub(crate) fn git_untracked_fixtures_unchanged(
    root: &Path,
    repository: &Repository,
) -> EvalResult<bool> {
    Ok(
        repository.status_file(Path::new(GIT_STAGE_PATH))? == Status::WT_NEW
            && repository.status_file(Path::new(GIT_COMMIT_PATH))? == Status::WT_NEW
            && repository.status_file(Path::new(GIT_NATURAL_PATH))? == Status::WT_NEW
            && fs::read(root.join(GIT_STAGE_PATH))? == GIT_STAGE_CONTENT.as_bytes()
            && fs::read(root.join(GIT_COMMIT_PATH))? == GIT_COMMIT_CONTENT.as_bytes()
            && fs::read(root.join(GIT_NATURAL_PATH))? == GIT_NATURAL_CONTENT.as_bytes(),
    )
}

pub(crate) fn git_status_overflow_path(index: usize) -> String {
    format!("{GIT_STATUS_OVERFLOW_DIRECTORY}/{index:03}.txt")
}

pub(crate) fn expected_git_status_paths() -> Vec<String> {
    let mut paths = vec![
        String::from(GIT_COMMIT_PATH),
        String::from(GIT_NATURAL_PATH),
        String::from(GIT_STAGE_PATH),
    ];
    for index in 0..GIT_STATUS_OVERFLOW_ENTRY_COUNT - 1 {
        paths.push(git_status_overflow_path(index));
    }
    paths
}

pub(crate) fn git_status_entries_match(entries: &serde_json::Value) -> bool {
    let Some(entries) = entries.as_array() else {
        return false;
    };
    let expected_paths = expected_git_status_paths();
    entries.len() == MAX_STATUS_ENTRIES
        && entries.iter().zip(expected_paths).all(|(entry, path)| {
            json_object_has_exact_fields(entry, &["path", "previous_path", "index", "worktree"])
                && entry["path"] == path
                && entry["previous_path"].is_null()
                && entry["index"] == "unchanged"
                && entry["worktree"] == "untracked"
        })
}

pub(crate) fn git_status_entries_json() -> Vec<serde_json::Value> {
    expected_git_status_paths()
        .into_iter()
        .map(|path| {
            serde_json::json!({
                "path": path,
                "previous_path": null,
                "index": "unchanged",
                "worktree": "untracked",
            })
        })
        .collect()
}

pub(crate) fn git_operation_state_is_clean(repository: &Repository) -> bool {
    repository.state() == RepositoryState::Clean
        && !repository.path().join(GIT_MERGE_HEAD_PATH).exists()
        && !repository.path().join(GIT_MERGE_MESSAGE_PATH).exists()
        && !repository.path().join(GIT_MERGE_MODE_PATH).exists()
}

pub(crate) fn staged_blob_matches_fixture(
    root: &Path,
    repository: &Repository,
    path: &str,
    expected: &[u8],
    expected_worktree_mode: Option<u32>,
) -> EvalResult<bool> {
    let index = repository.index()?;
    let Some(entry) = index.get_path(Path::new(path), 0) else {
        return Ok(false);
    };
    let blob = repository.find_blob(entry.id)?;
    Ok(entry.mode == GIT_REGULAR_INDEX_FILE_MODE
        && blob.content() == expected
        && fs::read(root.join(path))? == expected
        && worktree_file_mode_matches(&root.join(path), expected_worktree_mode)?)
}

pub(crate) fn worktree_file_mode_matches(path: &Path, expected: Option<u32>) -> EvalResult<bool> {
    Ok(worktree_mode(path)? == expected)
}

pub(crate) fn seed_git_repository(root: &Path) -> EvalResult<Oid> {
    let repository = Repository::init(root)?;
    fs::create_dir_all(repository.path().join(GIT_BRANCHES_DIRECTORY))?;
    fs::write(root.join(GIT_SEED_PATH), "seed\n")?;
    fs::write(root.join(GIT_STAGE_PATH), GIT_STAGE_CONTENT)?;
    fs::write(root.join(GIT_COMMIT_PATH), GIT_COMMIT_CONTENT)?;
    fs::write(root.join(GIT_NATURAL_PATH), GIT_NATURAL_CONTENT)?;
    let mut index = repository.index()?;
    index.add_path(Path::new(GIT_SEED_PATH))?;
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = repository.find_tree(tree_id)?;
    let signature = Signature::now(GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL)?;
    let commit = repository.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "seed tool eval repository",
        &tree,
        &[],
    )?;
    let commit = repository.find_commit(commit)?;
    let second = commit_git_seed_revision(&repository, &commit, GIT_SWITCH_CONTENT, "second seed")?;
    repository.branch("log-target", &second, false)?;
    repository.branch("switch-target", &second, false)?;
    let third = commit_git_seed_revision(&repository, &second, GIT_BASE_CONTENT, "third seed")?;
    repository.branch(GIT_BASE_BRANCH, &third, false)?;
    repository.set_head(&format!("refs/heads/{GIT_BASE_BRANCH}"))?;
    Ok(third.id())
}

pub(crate) fn seed_git_repository_with_refs(
    root: &Path,
) -> EvalResult<(Oid, GitReferenceInventory, GitFixtureSnapshot)> {
    let seed = seed_git_repository(root)?;
    let refs = git_reference_inventory(&Repository::open(root)?)?;
    let fixture = git_fixture_snapshot(root)?;
    Ok((seed, refs, fixture))
}

pub(crate) fn git_reference_inventory(
    repository: &Repository,
) -> EvalResult<GitReferenceInventory> {
    let mut targets = BTreeMap::new();
    for reference in repository.references()? {
        let reference = reference?;
        let target = match (reference.target(), reference.symbolic_target_bytes()) {
            (Some(target), None) => GitReferenceTarget::Direct(target),
            (None, Some(target)) => GitReferenceTarget::Symbolic(target.to_vec()),
            (Some(_), Some(_)) | (None, None) => {
                return Err(io::Error::other("a Git reference has an invalid target shape").into());
            }
        };
        targets.insert(reference.name_bytes().to_vec(), target);
    }
    Ok(targets)
}

pub(crate) fn git_reference_entries(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, WorkspaceEntrySnapshot>> {
    let repository = Repository::open(root)?;
    filesystem_entries(&repository.path().join(GIT_REFS_DIRECTORY), None)
}

pub(crate) fn git_reference_modified_times(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, SystemTime>> {
    let repository = Repository::open(root)?;
    filesystem_file_and_directory_modified_times(&repository.path().join(GIT_REFS_DIRECTORY))
}

pub(crate) fn git_reference_entry_identities(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, FilesystemIdentity>> {
    let repository = Repository::open(root)?;
    filesystem_entry_identities(&repository.path().join(GIT_REFS_DIRECTORY), None)
}

pub(crate) fn admit_modified_time_path_and_ancestors(
    actual: &BTreeMap<PathBuf, SystemTime>,
    expected: &mut BTreeMap<PathBuf, SystemTime>,
    actual_identities: &BTreeMap<PathBuf, FilesystemIdentity>,
    path: &Path,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> bool {
    let mut current = Some(path);
    while let Some(candidate) = current {
        let Some(modified) = actual.get(candidate) else {
            return false;
        };
        let Some(identity) = actual_identities.get(candidate) else {
            return false;
        };
        if expected.get(candidate) != Some(modified)
            && !execution_window
                .is_some_and(|window| window.contains_git_modified(*modified, *identity))
        {
            return false;
        }
        expected.insert(candidate.to_path_buf(), *modified);
        current = candidate.parent();
    }
    true
}

pub(crate) fn admit_filesystem_identity_path(
    actual: &BTreeMap<PathBuf, FilesystemIdentity>,
    expected: &mut BTreeMap<PathBuf, FilesystemIdentity>,
    path: &Path,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> bool {
    let Some(actual_identity) = actual.get(path) else {
        return false;
    };
    let Some(expected_identity) = expected.get_mut(path) else {
        return false;
    };
    if !filesystem_ownership_matches(Some(actual_identity), Some(expected_identity))
        || actual_identity.device != expected_identity.device
        || !execution_window.is_some_and(|window| window.contains_change_time(*actual_identity))
    {
        return false;
    }
    expected_identity.inode = actual_identity.inode;
    admit_filesystem_change_time(expected_identity, *actual_identity);
    let mut ancestor = path.parent();
    while let Some(candidate) = ancestor {
        let Some(actual_identity) = actual.get(candidate) else {
            return false;
        };
        let Some(expected_identity) = expected.get_mut(candidate) else {
            return false;
        };
        if *actual_identity != *expected_identity
            && !execution_window.is_some_and(|window| window.contains_change_time(*actual_identity))
        {
            return false;
        }
        admit_filesystem_change_time(expected_identity, *actual_identity);
        ancestor = candidate.parent();
    }
    true
}

pub(crate) fn direct_git_reference_entry(
    template: &WorkspaceEntrySnapshot,
    target: Oid,
) -> Option<WorkspaceEntrySnapshot> {
    let WorkspaceEntrySnapshot::File { mode, links, .. } = template else {
        return None;
    };
    Some(WorkspaceEntrySnapshot::File {
        content: format!("{target}\n").into_bytes(),
        mode: *mode,
        links: *links,
    })
}

pub(crate) fn git_forced_reference_entries_match(
    root: &Path,
    case_name: &str,
    arguments: &serde_json::Value,
    head: &git2::Commit<'_>,
    seed_fixture: &GitFixtureSnapshot,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> EvalResult<bool> {
    let repository = Repository::open(root)?;
    let mut expected = seed_fixture.reference_entries.clone();
    let actual_modified_times = git_reference_modified_times(root)?;
    let mut expected_modified_times = seed_fixture.reference_modified_times.clone();
    let actual_entry_identities = git_reference_entry_identities(root)?;
    let mut expected_entry_identities = seed_fixture.reference_entry_identities.clone();
    let base_path = Path::new("heads").join(GIT_BASE_BRANCH);
    match case_name {
        GIT_BRANCH_CREATE_NAME => {
            let Some(branch_name) = arguments["name"].as_str() else {
                return Ok(false);
            };
            let reference = repository.find_reference(&format!("refs/heads/{branch_name}"))?;
            let Some(target) = reference.target() else {
                return Ok(false);
            };
            let Some(template) = expected.get(&base_path) else {
                return Ok(false);
            };
            let Some(entry) = direct_git_reference_entry(template, target) else {
                return Ok(false);
            };
            let path = Path::new("heads").join(branch_name);
            if !admit_modified_time_path_and_ancestors(
                &actual_modified_times,
                &mut expected_modified_times,
                &actual_entry_identities,
                &path,
                execution_window,
            ) || !admit_new_filesystem_identity_path_and_ancestors(
                &actual_entry_identities,
                &mut expected_entry_identities,
                &path,
                execution_window,
            ) {
                return Ok(false);
            }
            expected.insert(path.clone(), entry);
        }
        GIT_CREATE_COMMIT_NAME => {
            let Some(template) = expected.get(&base_path) else {
                return Ok(false);
            };
            let Some(entry) = direct_git_reference_entry(template, head.id()) else {
                return Ok(false);
            };
            expected.insert(base_path.clone(), entry);
            if !admit_modified_time_path_and_ancestors(
                &actual_modified_times,
                &mut expected_modified_times,
                &actual_entry_identities,
                &base_path,
                execution_window,
            ) || !admit_filesystem_identity_path(
                &actual_entry_identities,
                &mut expected_entry_identities,
                &base_path,
                execution_window,
            ) {
                return Ok(false);
            }
        }
        _ => {}
    }
    Ok(git_reference_entries(root)? == expected
        && actual_modified_times == expected_modified_times
        && actual_entry_identities == expected_entry_identities)
}

pub(crate) fn git_fixture_modes(root: &Path) -> EvalResult<BTreeMap<PathBuf, Option<u32>>> {
    let mut modes = BTreeMap::new();
    for path in [
        GIT_SEED_PATH,
        GIT_STAGE_PATH,
        GIT_COMMIT_PATH,
        GIT_NATURAL_PATH,
    ] {
        modes.insert(PathBuf::from(path), worktree_mode(&root.join(path))?);
    }
    Ok(modes)
}

pub(crate) fn git_fixture_snapshot(root: &Path) -> EvalResult<GitFixtureSnapshot> {
    let repository = Repository::open(root)?;
    Ok(GitFixtureSnapshot {
        modes: git_fixture_modes(root)?,
        config: fs::read(repository.path().join(GIT_CONFIG_PATH))?,
        worktree_entries: git_worktree_entries(root)?,
        worktree_modified_times: git_worktree_modified_times(root)?,
        worktree_entry_identities: git_worktree_entry_identities(root)?,
        worktree_extended_attributes: git_worktree_extended_attributes(root)?,
        metadata_extended_attributes: git_metadata_extended_attributes(root)?,
        metadata_root_kind: git_metadata_root_kind(root)?,
        metadata_root_mode: worktree_mode(repository.path())?,
        metadata_root_modified_time: Some(git_metadata_root_modified_time(root)?),
        metadata_root_identity: git_metadata_root_identity(root)?,
        metadata_top_level: git_metadata_top_level(root)?,
        index_entries: git_index_entries(&repository)?,
        index_complete_entries: git_index_complete_entries(&repository)?,
        index_extensions: git_index_extensions(&repository)?,
        static_metadata_entries: git_static_metadata_entries(root)?,
        static_metadata_modified_times: git_static_metadata_modified_times(root)?,
        static_metadata_entry_identities: git_static_metadata_entry_identities(root)?,
        reflog_entries: git_reflog_entries(root)?,
        reflog_modified_times: git_reflog_modified_times(root)?,
        reflog_entry_identities: git_reflog_entry_identities(root)?,
        reference_entries: git_reference_entries(root)?,
        reference_modified_times: git_reference_modified_times(root)?,
        reference_entry_identities: git_reference_entry_identities(root)?,
        objects: git_object_inventory(&repository)?,
        object_entries: git_object_entries(root)?,
        object_modified_times: git_object_modified_times(root)?,
        object_entry_identities: git_object_entry_identities(root)?,
    })
}

pub(crate) fn git_metadata_root_kind(root: &Path) -> EvalResult<GitMetadataEntryKind> {
    let file_type = fs::symlink_metadata(root.join(".git"))?.file_type();
    Ok(if file_type.is_dir() {
        GitMetadataEntryKind::Directory
    } else if file_type.is_file() {
        GitMetadataEntryKind::File
    } else if file_type.is_symlink() {
        GitMetadataEntryKind::Symlink
    } else {
        GitMetadataEntryKind::Other
    })
}

pub(crate) fn git_metadata_extended_attributes(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, ExtendedAttributeSnapshot>> {
    let repository = Repository::open(root)?;
    let mut attributes = filesystem_extended_attributes(repository.path(), None)?;
    attributes.insert(PathBuf::new(), extended_attributes(repository.path())?);
    Ok(attributes)
}

pub(crate) fn git_metadata_root_modified_time(root: &Path) -> EvalResult<SystemTime> {
    Ok(fs::symlink_metadata(root.join(".git"))?.modified()?)
}

pub(crate) fn git_metadata_root_identity(root: &Path) -> EvalResult<Option<FilesystemIdentity>> {
    Ok(filesystem_identity(&fs::metadata(root.join(".git"))?))
}

pub(crate) fn git_index_entries(repository: &Repository) -> EvalResult<Vec<GitIndexEntrySnapshot>> {
    Ok(repository
        .index()?
        .iter()
        .map(|entry| GitIndexEntrySnapshot {
            path: entry.path,
            id: entry.id,
            mode: entry.mode,
            flags: entry.flags,
            flags_extended: entry.flags_extended,
        })
        .collect())
}

pub(crate) fn git_index_complete_entries(
    repository: &Repository,
) -> EvalResult<Vec<GitIndexCompleteEntrySnapshot>> {
    Ok(repository
        .index()?
        .iter()
        .map(|entry| GitIndexCompleteEntrySnapshot {
            semantic: GitIndexEntrySnapshot {
                path: entry.path,
                id: entry.id,
                mode: entry.mode,
                flags: entry.flags,
                flags_extended: entry.flags_extended,
            },
            ctime: entry.ctime,
            mtime: entry.mtime,
            dev: entry.dev,
            ino: entry.ino,
            uid: entry.uid,
            gid: entry.gid,
            file_size: entry.file_size,
        })
        .collect())
}

pub(crate) fn git_index_extensions(
    repository: &Repository,
) -> EvalResult<Vec<GitIndexExtensionSnapshot>> {
    let bytes = fs::read(repository.path().join("index"))?;
    git_index_extension_records(&bytes)
}

pub(crate) fn git_index_extension_records(
    bytes: &[u8],
) -> EvalResult<Vec<GitIndexExtensionSnapshot>> {
    let invalid_index = || io::Error::new(io::ErrorKind::InvalidData, "invalid Git index");
    let extension_end = bytes
        .len()
        .checked_sub(GIT_INDEX_OBJECT_ID_BYTES)
        .filter(|end| *end >= GIT_INDEX_HEADER_BYTES)
        .ok_or_else(invalid_index)?;
    if bytes.get(..4) != Some(b"DIRC") {
        return Err(invalid_index().into());
    }
    let version = u32::from_be_bytes(bytes.get(4..8).ok_or_else(invalid_index)?.try_into()?);
    if !(2..=4).contains(&version) {
        return Err(invalid_index().into());
    }
    let entry_count = usize::try_from(u32::from_be_bytes(
        bytes
            .get(8..GIT_INDEX_HEADER_BYTES)
            .ok_or_else(invalid_index)?
            .try_into()?,
    ))?;
    let mut cursor = GIT_INDEX_HEADER_BYTES;
    for _entry in 0..entry_count {
        let entry_start = cursor;
        let flags_offset = cursor
            .checked_add(GIT_INDEX_ENTRY_FIELDS_BEFORE_ID_BYTES + GIT_INDEX_OBJECT_ID_BYTES)
            .filter(|offset| offset.saturating_add(GIT_INDEX_ENTRY_FLAGS_BYTES) <= extension_end)
            .ok_or_else(invalid_index)?;
        let flags = u16::from_be_bytes(
            bytes
                .get(flags_offset..flags_offset + GIT_INDEX_ENTRY_FLAGS_BYTES)
                .ok_or_else(invalid_index)?
                .try_into()?,
        );
        cursor = flags_offset + GIT_INDEX_ENTRY_FLAGS_BYTES;
        if flags & GIT_INDEX_EXTENDED_FLAG != 0 {
            cursor = cursor
                .checked_add(GIT_INDEX_EXTENDED_FLAGS_BYTES)
                .filter(|cursor| *cursor <= extension_end)
                .ok_or_else(invalid_index)?;
        }
        if version == 4 {
            let mut prefix_bytes = 0_usize;
            loop {
                let byte = *bytes.get(cursor).ok_or_else(invalid_index)?;
                cursor += 1;
                prefix_bytes += 1;
                if byte & 0x80 == 0 {
                    break;
                }
                if prefix_bytes == 10 {
                    return Err(invalid_index().into());
                }
            }
            let suffix = bytes.get(cursor..extension_end).ok_or_else(invalid_index)?;
            let nul = suffix
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(invalid_index)?;
            cursor = cursor.checked_add(nul + 1).ok_or_else(invalid_index)?;
        } else {
            let stated_path_bytes = usize::from(flags & 0x0fff);
            if stated_path_bytes < 0x0fff {
                let nul = cursor
                    .checked_add(stated_path_bytes)
                    .filter(|nul| bytes.get(*nul) == Some(&0))
                    .ok_or_else(invalid_index)?;
                cursor = nul + 1;
            } else {
                let path = bytes.get(cursor..extension_end).ok_or_else(invalid_index)?;
                let nul = path
                    .iter()
                    .position(|byte| *byte == 0)
                    .ok_or_else(invalid_index)?;
                cursor = cursor.checked_add(nul + 1).ok_or_else(invalid_index)?;
            }
            let entry_bytes = cursor.checked_sub(entry_start).ok_or_else(invalid_index)?;
            cursor = entry_start
                .checked_add((entry_bytes + 7) & !7)
                .filter(|cursor| *cursor <= extension_end)
                .ok_or_else(invalid_index)?;
        }
    }
    let mut extensions = Vec::new();
    while cursor < extension_end {
        let header_end = cursor
            .checked_add(GIT_INDEX_EXTENSION_HEADER_BYTES)
            .filter(|end| *end <= extension_end)
            .ok_or_else(invalid_index)?;
        let signature = bytes
            .get(cursor..cursor + 4)
            .ok_or_else(invalid_index)?
            .try_into()?;
        let content_bytes = usize::try_from(u32::from_be_bytes(
            bytes
                .get(cursor + 4..header_end)
                .ok_or_else(invalid_index)?
                .try_into()?,
        ))?;
        let content_end = header_end
            .checked_add(content_bytes)
            .filter(|end| *end <= extension_end)
            .ok_or_else(invalid_index)?;
        extensions.push(GitIndexExtensionSnapshot {
            signature,
            content: bytes[header_end..content_end].to_vec(),
        });
        cursor = content_end;
    }
    Ok(extensions)
}

pub(crate) fn append_synthetic_git_index_extension(repository: &Repository) -> EvalResult {
    let path = repository.path().join("index");
    let mut bytes = fs::read(&path)?;
    let checksum_start = bytes
        .len()
        .checked_sub(GIT_INDEX_OBJECT_ID_BYTES)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid Git index"))?;
    bytes.truncate(checksum_start);
    bytes.extend_from_slice(&SYNTHETIC_GIT_INDEX_EXTENSION_SIGNATURE);
    bytes.extend_from_slice(
        &u32::try_from(SYNTHETIC_GIT_INDEX_EXTENSION_CONTENT.len())?.to_be_bytes(),
    );
    bytes.extend_from_slice(SYNTHETIC_GIT_INDEX_EXTENSION_CONTENT);
    let checksum = Sha1::digest(&bytes);
    bytes.extend_from_slice(&checksum);
    fs::write(path, bytes)?;
    Ok(())
}

pub(crate) fn expected_git_index_entry(
    path: &str,
    content: &[u8],
) -> EvalResult<GitIndexEntrySnapshot> {
    let path_bytes = path.as_bytes().to_vec();
    Ok(GitIndexEntrySnapshot {
        flags: u16::try_from(path_bytes.len().min(0x0fff))?,
        path: path_bytes,
        id: Oid::hash_object(ObjectType::Blob, content)?,
        mode: GIT_REGULAR_INDEX_FILE_MODE,
        flags_extended: 0,
    })
}

pub(crate) fn git_index_with_expected_file(
    seed_fixture: &GitFixtureSnapshot,
    path: &str,
    content: &[u8],
) -> EvalResult<Vec<GitIndexEntrySnapshot>> {
    let mut expected = seed_fixture.index_entries.clone();
    expected.retain(|entry| entry.path != path.as_bytes());
    expected.push(expected_git_index_entry(path, content)?);
    expected.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(expected)
}

pub(crate) fn git_index_complete_entries_match(
    root: &Path,
    repository: &Repository,
    baseline: &[GitIndexCompleteEntrySnapshot],
    mutable_path: Option<&str>,
) -> EvalResult<bool> {
    let actual = git_index_complete_entries(repository)?;
    let Some(mutable_path) = mutable_path else {
        return Ok(actual == baseline);
    };
    let mutable_path_bytes = mutable_path.as_bytes();
    let unchanged_actual = actual
        .iter()
        .filter(|entry| entry.semantic.path != mutable_path_bytes)
        .collect::<Vec<_>>();
    let unchanged_baseline = baseline
        .iter()
        .filter(|entry| entry.semantic.path != mutable_path_bytes)
        .collect::<Vec<_>>();
    if unchanged_actual != unchanged_baseline {
        return Ok(false);
    }
    let Some(target) = actual
        .iter()
        .find(|entry| entry.semantic.path == mutable_path_bytes)
    else {
        return Ok(false);
    };
    git_index_entry_matches_worktree(root, mutable_path, target)
}

#[cfg(unix)]
pub(crate) fn git_index_entry_matches_worktree(
    root: &Path,
    path: &str,
    entry: &GitIndexCompleteEntrySnapshot,
) -> EvalResult<bool> {
    let metadata = fs::symlink_metadata(root.join(path))?;
    let dev = u32::try_from(metadata.dev() & u64::from(u32::MAX)).ok();
    let ino = u32::try_from(metadata.ino() & u64::from(u32::MAX)).ok();
    Ok(metadata.file_type().is_file()
        && i64::from(entry.ctime.seconds()) == metadata.ctime()
        && i64::from(entry.ctime.nanoseconds()) == metadata.ctime_nsec()
        && i64::from(entry.mtime.seconds()) == metadata.mtime()
        && i64::from(entry.mtime.nanoseconds()) == metadata.mtime_nsec()
        && (entry.dev == 0 || Some(entry.dev) == dev)
        && Some(entry.ino) == ino
        && entry.uid == metadata.uid()
        && entry.gid == metadata.gid()
        && u64::from(entry.file_size) == metadata.size())
}

#[cfg(not(unix))]
pub(crate) fn git_index_entry_matches_worktree(
    root: &Path,
    path: &str,
    entry: &GitIndexCompleteEntrySnapshot,
) -> EvalResult<bool> {
    let metadata = fs::symlink_metadata(root.join(path))?;
    Ok(metadata.is_file() && u64::from(entry.file_size) == metadata.len())
}

pub(crate) fn git_forced_index_matches(
    root: &Path,
    repository: &Repository,
    case_name: &str,
    seed_fixture: &GitFixtureSnapshot,
    pre_execution_index_entries: Option<&[GitIndexCompleteEntrySnapshot]>,
) -> EvalResult<bool> {
    let expected = match case_name {
        GIT_BRANCH_SWITCH_NAME => git_index_with_expected_file(
            seed_fixture,
            GIT_SEED_PATH,
            GIT_SWITCH_CONTENT.as_bytes(),
        )?,
        GIT_CREATE_COMMIT_NAME => git_index_with_expected_file(
            seed_fixture,
            GIT_COMMIT_PATH,
            GIT_COMMIT_CONTENT.as_bytes(),
        )?,
        GIT_DIFF_NAME | GIT_STAGE_NAME => git_index_with_expected_file(
            seed_fixture,
            GIT_STAGE_PATH,
            GIT_STAGE_CONTENT.as_bytes(),
        )?,
        _ => seed_fixture.index_entries.clone(),
    };
    let baseline = pre_execution_index_entries.unwrap_or(&seed_fixture.index_complete_entries);
    let mutable_path = match case_name {
        GIT_BRANCH_SWITCH_NAME => Some(GIT_SEED_PATH),
        GIT_CREATE_COMMIT_NAME => Some(GIT_COMMIT_PATH),
        GIT_STAGE_NAME => Some(GIT_STAGE_PATH),
        GIT_BRANCH_CREATE_NAME | GIT_DIFF_NAME | GIT_LOG_NAME | GIT_STATUS_NAME => None,
        _ => return Ok(false),
    };
    let complete_entries_match =
        git_index_complete_entries_match(root, repository, baseline, mutable_path)?;
    Ok(git_index_entries(repository)? == expected
        && git_index_extensions(repository)? == seed_fixture.index_extensions
        && complete_entries_match)
}

pub(crate) fn git_object_inventory(repository: &Repository) -> EvalResult<GitObjectInventory> {
    let database = repository.odb()?;
    let mut ids = BTreeSet::new();
    database.foreach(|id| {
        ids.insert(*id);
        true
    })?;
    ids.into_iter()
        .map(|id| {
            let object = database.read(id)?;
            if Oid::hash_object(object.kind(), object.data())? != id {
                return Err(io::Error::other("a Git object does not match its object ID").into());
            }
            Ok((
                id,
                GitObjectSnapshot {
                    kind: object.kind(),
                    content: object.data().to_vec(),
                },
            ))
        })
        .collect()
}

pub(crate) fn git_object_entries(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, WorkspaceEntrySnapshot>> {
    let repository = Repository::open(root)?;
    filesystem_entries(&repository.path().join(GIT_OBJECTS_DIRECTORY), None)
}

pub(crate) fn git_object_modified_times(root: &Path) -> EvalResult<BTreeMap<PathBuf, SystemTime>> {
    let repository = Repository::open(root)?;
    filesystem_file_and_directory_modified_times(&repository.path().join(GIT_OBJECTS_DIRECTORY))
}

pub(crate) fn git_object_entry_identities(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, FilesystemIdentity>> {
    let repository = Repository::open(root)?;
    filesystem_entry_identities(&repository.path().join(GIT_OBJECTS_DIRECTORY), None)
}

pub(crate) fn git_loose_object_relative_path(id: Oid) -> PathBuf {
    let id = id.to_string();
    Path::new(&id[..2]).join(&id[2..])
}

pub(crate) fn publish_git_object_pack_for_test(
    repository: &Repository,
    ids: &[Oid],
    baseline: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
) -> EvalResult {
    let mut builder = repository.packbuilder()?;
    for id in ids {
        builder.insert_object(*id, None)?;
    }
    let mut buffer = git2::Buf::new();
    builder.write_buf(&mut buffer)?;
    let object_database = repository.odb()?;
    let pack_directory = repository.path().join(GIT_OBJECTS_DIRECTORY).join("pack");
    let pack_mode = git_pack_file_mode(baseline)
        .ok_or_else(|| io::Error::other("the Git fixture has no pack directory mode"))?
        .unwrap_or_default();
    let mut indexer = git2::Indexer::new_ext(
        Some(&object_database),
        &pack_directory,
        pack_mode,
        true,
        git2::ObjectFormat::Sha1,
    )?;
    std::io::Write::write_all(&mut indexer, &buffer)?;
    indexer.commit()?;
    let mut removable_parents = BTreeSet::new();
    for id in ids {
        let relative = git_loose_object_relative_path(*id);
        fs::remove_file(
            repository
                .path()
                .join(GIT_OBJECTS_DIRECTORY)
                .join(&relative),
        )?;
        let parent = relative
            .parent()
            .ok_or_else(|| io::Error::other("a loose object has no fanout directory"))?;
        if !baseline.contains_key(parent) {
            removable_parents.insert(parent.to_path_buf());
        }
    }
    for parent in removable_parents {
        fs::remove_dir(repository.path().join(GIT_OBJECTS_DIRECTORY).join(parent))?;
    }
    Ok(())
}

pub(crate) fn git_pack_publication_parts(path: &Path) -> Option<(&str, &str)> {
    if path.parent() != Some(Path::new("pack")) {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    let (stem, extension) = name.rsplit_once('.')?;
    let checksum = stem.strip_prefix("pack-")?;
    matches!(extension, "idx" | "pack")
        .then_some(())
        .filter(|()| checksum.len() == 40 && checksum.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(|()| (stem, extension))
}

pub(crate) fn git_pack_index_object_ids(content: &[u8]) -> Option<BTreeSet<Oid>> {
    const HEADER_BYTES: usize = 8;
    const FANOUT_ENTRIES: usize = 256;
    const FANOUT_ENTRY_BYTES: usize = 4;
    const SHA1_BYTES: usize = 20;
    const INDEX_MAGIC: [u8; 4] = [0xff, b't', b'O', b'c'];
    const INDEX_VERSION: [u8; 4] = 2_u32.to_be_bytes();
    let fanout_bytes = FANOUT_ENTRIES.checked_mul(FANOUT_ENTRY_BYTES)?;
    let names_offset = HEADER_BYTES.checked_add(fanout_bytes)?;
    if content.get(..4)? != INDEX_MAGIC || content.get(4..HEADER_BYTES)? != INDEX_VERSION {
        return None;
    }
    let count_offset = names_offset.checked_sub(FANOUT_ENTRY_BYTES)?;
    let count = u32::from_be_bytes(content.get(count_offset..names_offset)?.try_into().ok()?);
    let count = usize::try_from(count).ok()?;
    let names_bytes = count.checked_mul(SHA1_BYTES)?;
    let names = content.get(names_offset..names_offset.checked_add(names_bytes)?)?;
    names
        .as_chunks::<SHA1_BYTES>()
        .0
        .iter()
        .map(|bytes| Oid::from_bytes(bytes).ok())
        .collect()
}

pub(crate) fn git_pack_file_mode(
    baseline: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
) -> Option<Option<u32>> {
    let WorkspaceEntrySnapshot::Directory { mode } = baseline.get(Path::new("pack"))? else {
        return None;
    };
    Some(mode.map(|mode| (mode & 0o666) | 0o600))
}

pub(crate) struct GitObjectEntryInventory<'a> {
    pub(crate) actual: &'a BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    pub(crate) actual_modified_times: &'a BTreeMap<PathBuf, SystemTime>,
    pub(crate) actual_entry_identities: &'a BTreeMap<PathBuf, FilesystemIdentity>,
    pub(crate) expected: &'a mut BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    pub(crate) expected_modified_times: &'a mut BTreeMap<PathBuf, SystemTime>,
    pub(crate) expected_entry_identities: &'a mut BTreeMap<PathBuf, FilesystemIdentity>,
    pub(crate) execution_window: Option<FilesystemExecutionTimeWindow>,
}

#[derive(Clone, Copy)]
pub(crate) struct GitObjectEntrySnapshots<'a> {
    pub(crate) entries: &'a BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    pub(crate) modified_times: &'a BTreeMap<PathBuf, SystemTime>,
    pub(crate) entry_identities: &'a BTreeMap<PathBuf, FilesystemIdentity>,
}

pub(crate) fn admit_git_pack_publications(
    inventory: &mut GitObjectEntryInventory<'_>,
    allowed_ids: &BTreeSet<Oid>,
    published_ids: &mut BTreeSet<Oid>,
    file_links: Option<u64>,
) -> bool {
    let Some(file_mode) = git_pack_file_mode(inventory.expected) else {
        return false;
    };
    let new_paths = inventory
        .actual
        .keys()
        .filter(|path| !inventory.expected.contains_key(*path))
        .cloned()
        .collect::<Vec<_>>();
    let mut publications = BTreeMap::<String, BTreeSet<String>>::new();
    for path in &new_paths {
        let Some((stem, extension)) = git_pack_publication_parts(path) else {
            return false;
        };
        publications
            .entry(stem.to_owned())
            .or_default()
            .insert(extension.to_owned());
    }
    let expected_extensions = BTreeSet::from([String::from("idx"), String::from("pack")]);
    if publications
        .values()
        .any(|extensions| *extensions != expected_extensions)
    {
        return false;
    }
    for stem in publications.keys() {
        let index_path = Path::new("pack").join(format!("{stem}.idx"));
        let Some(WorkspaceEntrySnapshot::File { content, .. }) = inventory.actual.get(&index_path)
        else {
            return false;
        };
        let Some(index_ids) = git_pack_index_object_ids(content) else {
            return false;
        };
        for id in index_ids {
            if !allowed_ids.contains(&id) || !published_ids.insert(id) {
                return false;
            }
        }
    }
    for path in new_paths {
        let Some(WorkspaceEntrySnapshot::File { content, .. }) = inventory.actual.get(&path) else {
            return false;
        };
        if !admit_modified_time_path_and_ancestors(
            inventory.actual_modified_times,
            inventory.expected_modified_times,
            inventory.actual_entry_identities,
            &path,
            inventory.execution_window,
        ) || !admit_new_filesystem_identity_path_and_ancestors(
            inventory.actual_entry_identities,
            inventory.expected_entry_identities,
            &path,
            inventory.execution_window,
        ) {
            return false;
        }
        inventory.expected.insert(
            path,
            WorkspaceEntrySnapshot::File {
                content: content.clone(),
                mode: file_mode,
                links: file_links,
            },
        );
    }
    true
}

pub(crate) fn git_object_entry_inventory_matches(
    root: &Path,
    baseline: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    baseline_modified_times: &BTreeMap<PathBuf, SystemTime>,
    baseline_entry_identities: &BTreeMap<PathBuf, FilesystemIdentity>,
    allowed_ids: &[Oid],
    seed_fixture: &GitFixtureSnapshot,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> EvalResult<bool> {
    let actual = git_object_entries(root)?;
    let actual_modified_times = git_object_modified_times(root)?;
    let actual_entry_identities = git_object_entry_identities(root)?;
    git_object_entry_inventory_snapshots_match(
        GitObjectEntrySnapshots {
            entries: &actual,
            modified_times: &actual_modified_times,
            entry_identities: &actual_entry_identities,
        },
        GitObjectEntrySnapshots {
            entries: baseline,
            modified_times: baseline_modified_times,
            entry_identities: baseline_entry_identities,
        },
        allowed_ids,
        seed_fixture,
        execution_window,
    )
}

pub(crate) fn git_object_entry_inventory_snapshots_match(
    actual: GitObjectEntrySnapshots<'_>,
    baseline: GitObjectEntrySnapshots<'_>,
    allowed_ids: &[Oid],
    seed_fixture: &GitFixtureSnapshot,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> EvalResult<bool> {
    let mut expected_modified_times = (*baseline.modified_times).clone();
    let mut expected_entry_identities = (*baseline.entry_identities).clone();
    let Some((file_mode, file_links)) = seed_fixture.object_entries.values().find_map(|entry| {
        if let WorkspaceEntrySnapshot::File { mode, links, .. } = entry {
            Some((*mode, *links))
        } else {
            None
        }
    }) else {
        return Ok(false);
    };
    let Some(directory_mode) = seed_fixture.object_entries.values().find_map(|entry| {
        if let WorkspaceEntrySnapshot::Directory { mode } = entry {
            Some(*mode)
        } else {
            None
        }
    }) else {
        return Ok(false);
    };
    let mut expected = (*baseline.entries).clone();
    let allowed_ids = allowed_ids.iter().copied().collect::<BTreeSet<_>>();
    let mut published_ids = BTreeSet::new();
    for id in &allowed_ids {
        let relative = git_loose_object_relative_path(*id);
        if expected.contains_key(&relative) {
            published_ids.insert(*id);
            continue;
        }
        if !actual.entries.contains_key(&relative) {
            continue;
        }
        if !admit_modified_time_path_and_ancestors(
            actual.modified_times,
            &mut expected_modified_times,
            actual.entry_identities,
            &relative,
            execution_window,
        ) {
            return Ok(false);
        }
        let Some(parent) = relative.parent() else {
            return Ok(false);
        };
        expected
            .entry(parent.to_path_buf())
            .or_insert(WorkspaceEntrySnapshot::Directory {
                mode: directory_mode,
            });
        let Some(WorkspaceEntrySnapshot::File { content, .. }) = actual.entries.get(&relative)
        else {
            return Ok(false);
        };
        expected.insert(
            relative.clone(),
            WorkspaceEntrySnapshot::File {
                content: content.clone(),
                mode: file_mode,
                links: file_links,
            },
        );
        if !admit_new_filesystem_identity_path_and_ancestors(
            actual.entry_identities,
            &mut expected_entry_identities,
            &relative,
            execution_window,
        ) {
            return Ok(false);
        }
        published_ids.insert(*id);
    }
    let mut inventory = GitObjectEntryInventory {
        actual: actual.entries,
        actual_modified_times: actual.modified_times,
        actual_entry_identities: actual.entry_identities,
        expected: &mut expected,
        expected_modified_times: &mut expected_modified_times,
        expected_entry_identities: &mut expected_entry_identities,
        execution_window,
    };
    if !admit_git_pack_publications(&mut inventory, &allowed_ids, &mut published_ids, file_links)
        || published_ids != allowed_ids
    {
        return Ok(false);
    }
    Ok(*actual.entries == expected
        && *actual.modified_times == expected_modified_times
        && *actual.entry_identities == expected_entry_identities)
}

pub(crate) fn git_forced_objects_match(
    repository: &Repository,
    case_name: &str,
    head: &git2::Commit<'_>,
    seed_fixture: &GitFixtureSnapshot,
    pre_execution: Option<&GitObjectInventory>,
) -> EvalResult<bool> {
    let mut expected = pre_execution
        .cloned()
        .unwrap_or_else(|| seed_fixture.objects.clone());
    match case_name {
        GIT_STAGE_NAME => {
            let id = Oid::hash_object(ObjectType::Blob, GIT_STAGE_CONTENT.as_bytes())?;
            expected.insert(
                id,
                GitObjectSnapshot {
                    kind: ObjectType::Blob,
                    content: GIT_STAGE_CONTENT.as_bytes().to_vec(),
                },
            );
        }
        GIT_CREATE_COMMIT_NAME => {
            let actual = match git_object_inventory(repository) {
                Ok(actual) => actual,
                Err(_) => return Ok(false),
            };
            let Some(commit) = actual.get(&head.id()) else {
                return Ok(false);
            };
            let Some(tree) = actual.get(&head.tree_id()) else {
                return Ok(false);
            };
            expected.insert(head.id(), commit.clone());
            expected.insert(head.tree_id(), tree.clone());
        }
        _ => {}
    }
    Ok(git_object_inventory(repository).is_ok_and(|actual| actual == expected))
}

pub(crate) struct GitObjectEntryVerification<'a> {
    pub(crate) pre_execution_entries: Option<&'a BTreeMap<PathBuf, WorkspaceEntrySnapshot>>,
    pub(crate) pre_execution_modified_times: Option<&'a BTreeMap<PathBuf, SystemTime>>,
    pub(crate) pre_execution_entry_identities: Option<&'a BTreeMap<PathBuf, FilesystemIdentity>>,
    pub(crate) execution_window: Option<FilesystemExecutionTimeWindow>,
}

pub(crate) fn git_forced_object_entries_match(
    root: &Path,
    case_name: &str,
    head: &git2::Commit<'_>,
    seed_fixture: &GitFixtureSnapshot,
    verification: GitObjectEntryVerification<'_>,
) -> EvalResult<bool> {
    let GitObjectEntryVerification {
        pre_execution_entries,
        pre_execution_modified_times,
        pre_execution_entry_identities,
        execution_window,
    } = verification;
    let allowed = match case_name {
        GIT_STAGE_NAME => vec![Oid::hash_object(
            ObjectType::Blob,
            GIT_STAGE_CONTENT.as_bytes(),
        )?],
        GIT_CREATE_COMMIT_NAME => vec![head.id(), head.tree_id()],
        _ => Vec::new(),
    };
    git_object_entry_inventory_matches(
        root,
        pre_execution_entries.unwrap_or(&seed_fixture.object_entries),
        pre_execution_modified_times.unwrap_or(&seed_fixture.object_modified_times),
        pre_execution_entry_identities.unwrap_or(&seed_fixture.object_entry_identities),
        &allowed,
        seed_fixture,
        execution_window,
    )
}

pub(crate) fn git_reflog_entries(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, WorkspaceEntrySnapshot>> {
    let repository = Repository::open(root)?;
    filesystem_entries(&repository.path().join(GIT_LOGS_DIRECTORY), None)
}

pub(crate) fn git_reflog_modified_times(root: &Path) -> EvalResult<BTreeMap<PathBuf, SystemTime>> {
    let repository = Repository::open(root)?;
    filesystem_file_and_directory_modified_times(&repository.path().join(GIT_LOGS_DIRECTORY))
}

pub(crate) fn git_reflog_entry_identities(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, FilesystemIdentity>> {
    let repository = Repository::open(root)?;
    filesystem_entry_identities(&repository.path().join(GIT_LOGS_DIRECTORY), None)
}

pub(crate) fn git_forced_reflogs_match(
    root: &Path,
    case_name: &str,
    seed: Oid,
    head: Oid,
    seed_fixture: &GitFixtureSnapshot,
    execution_window: Option<GitExecutionTimeWindow>,
    filesystem_execution_window: Option<FilesystemExecutionTimeWindow>,
) -> EvalResult<bool> {
    match case_name {
        GIT_CREATE_COMMIT_NAME => {
            let branch_reference = format!("refs/heads/{GIT_BASE_BRANCH}");
            git_reflog_updates_match(
                root,
                &["HEAD", branch_reference.as_str()],
                seed_fixture,
                GitReflogUpdateExpectation {
                    old: seed,
                    new: head,
                    message: GIT_COMMIT_REFLOG_MESSAGE,
                    execution_window: None,
                    filesystem_execution_window,
                },
            )
        }
        GIT_BRANCH_SWITCH_NAME => {
            let repository = Repository::open(root)?;
            let target = repository
                .find_branch("switch-target", BranchType::Local)?
                .into_reference()
                .peel_to_commit()?
                .id();
            git_reflog_updates_match(
                root,
                &["HEAD"],
                seed_fixture,
                GitReflogUpdateExpectation {
                    old: seed,
                    new: target,
                    message: GIT_SWITCH_REFLOG_MESSAGE,
                    execution_window,
                    filesystem_execution_window,
                },
            )
        }
        _ => Ok(git_reflog_entries(root)? == seed_fixture.reflog_entries
            && git_reflog_modified_times(root)? == seed_fixture.reflog_modified_times
            && git_reflog_entry_identities(root)? == seed_fixture.reflog_entry_identities),
    }
}

pub(crate) fn git_reflog_updates_match(
    root: &Path,
    references: &[&str],
    seed_fixture: &GitFixtureSnapshot,
    expectation: GitReflogUpdateExpectation<'_>,
) -> EvalResult<bool> {
    let repository = Repository::open(root)?;
    let actual_entries = git_reflog_entries(root)?;
    let mut expected_entries = seed_fixture.reflog_entries.clone();
    let actual_modified_times = git_reflog_modified_times(root)?;
    let mut expected_modified_times = seed_fixture.reflog_modified_times.clone();
    let actual_entry_identities = git_reflog_entry_identities(root)?;
    let mut expected_entry_identities = seed_fixture.reflog_entry_identities.clone();
    for reference in references {
        if !replace_expected_reflog_update(
            &repository,
            &actual_entries,
            &mut expected_entries,
            reference,
            expectation,
        )? {
            return Ok(false);
        }
        let path = Path::new(reference);
        if !admit_modified_time_path_and_ancestors(
            &actual_modified_times,
            &mut expected_modified_times,
            &actual_entry_identities,
            path,
            expectation.filesystem_execution_window,
        ) || !admit_filesystem_identity_path(
            &actual_entry_identities,
            &mut expected_entry_identities,
            path,
            expectation.filesystem_execution_window,
        ) {
            return Ok(false);
        }
    }
    Ok(actual_entries == expected_entries
        && actual_modified_times == expected_modified_times
        && actual_entry_identities == expected_entry_identities)
}

#[derive(Clone, Copy)]
pub(crate) struct GitReflogUpdateExpectation<'a> {
    pub(crate) old: Oid,
    pub(crate) new: Oid,
    pub(crate) message: &'a str,
    pub(crate) execution_window: Option<GitExecutionTimeWindow>,
    pub(crate) filesystem_execution_window: Option<FilesystemExecutionTimeWindow>,
}

pub(crate) fn replace_expected_reflog_update(
    repository: &Repository,
    actual_entries: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    expected_entries: &mut BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    reference: &str,
    expectation: GitReflogUpdateExpectation<'_>,
) -> EvalResult<bool> {
    let GitReflogUpdateExpectation {
        old,
        new,
        message,
        execution_window,
        filesystem_execution_window: _,
    } = expectation;
    let path = Path::new(reference);
    let Some(WorkspaceEntrySnapshot::File {
        content: seed_content,
        mode: seed_mode,
        links: seed_links,
    }) = expected_entries.get(path)
    else {
        return Ok(false);
    };
    let Some(WorkspaceEntrySnapshot::File {
        content: actual_content,
        mode: actual_mode,
        links: actual_links,
    }) = actual_entries.get(path)
    else {
        return Ok(false);
    };
    let Some(appended) = actual_content.strip_prefix(seed_content.as_slice()) else {
        return Ok(false);
    };
    let Some(record) = appended.strip_suffix(b"\n") else {
        return Ok(false);
    };
    if record.is_empty()
        || record.contains(&b'\n')
        || actual_mode != seed_mode
        || actual_links != seed_links
    {
        return Ok(false);
    }
    let reflog = repository.reflog(reference)?;
    let Some(latest) = reflog.get(0) else {
        return Ok(false);
    };
    let committer = latest.committer();
    let committer_time = committer.when();
    let recorded_time_matches = if message == GIT_COMMIT_REFLOG_MESSAGE {
        let commit_time = repository.find_commit(new)?.committer().when();
        committer_time.seconds() == commit_time.seconds()
            && committer_time.offset_minutes() == commit_time.offset_minutes()
    } else if message == GIT_SWITCH_REFLOG_MESSAGE {
        execution_window.is_some_and(|window| window.contains(committer_time))
    } else {
        true
    };
    if latest.id_old() != old
        || latest.id_new() != new
        || latest.message().ok().flatten() != Some(message)
        || committer.name().ok() != Some(GIT_AUTHOR_NAME)
        || committer.email().ok() != Some(GIT_AUTHOR_EMAIL)
        || !recorded_time_matches
    {
        return Ok(false);
    }
    expected_entries.insert(path.to_path_buf(), actual_entries[path].clone());
    Ok(true)
}

pub(crate) fn git_metadata_top_level(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, GitMetadataEntrySnapshot>> {
    let repository = Repository::open(root)?;
    let mut entries = BTreeMap::new();
    for entry in fs::read_dir(repository.path())? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        let file_type = metadata.file_type();
        let kind = if file_type.is_dir() {
            GitMetadataEntryKind::Directory
        } else if file_type.is_file() {
            GitMetadataEntryKind::File
        } else if file_type.is_symlink() {
            GitMetadataEntryKind::Symlink
        } else {
            GitMetadataEntryKind::Other
        };
        let mode = if kind == GitMetadataEntryKind::Symlink {
            None
        } else {
            worktree_mode(&entry.path())?
        };
        let (links, content) = if kind == GitMetadataEntryKind::File {
            (
                worktree_link_count(&entry.path())?,
                Some(fs::read(entry.path())?),
            )
        } else {
            (None, None)
        };
        let modified = matches!(
            kind,
            GitMetadataEntryKind::Directory | GitMetadataEntryKind::File
        )
        .then(|| metadata.modified())
        .transpose()?;
        let identity = matches!(
            kind,
            GitMetadataEntryKind::Directory | GitMetadataEntryKind::File
        )
        .then(|| filesystem_identity(&metadata))
        .flatten();
        entries.insert(
            PathBuf::from(entry.file_name()),
            GitMetadataEntrySnapshot {
                kind,
                mode,
                links,
                content,
                modified,
                identity,
            },
        );
    }
    Ok(entries)
}

pub(crate) fn git_static_metadata_entries(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, WorkspaceEntrySnapshot>> {
    let repository = Repository::open(root)?;
    let metadata_root = repository.path();
    let mut entries = BTreeMap::new();
    for (directory, snapshot) in git_metadata_top_level(root)? {
        if snapshot.kind != GitMetadataEntryKind::Directory
            || matches!(
                directory.to_str(),
                Some(GIT_OBJECTS_DIRECTORY | GIT_LOGS_DIRECTORY | GIT_REFS_DIRECTORY)
            )
        {
            continue;
        }
        for (relative, snapshot) in filesystem_entries(&metadata_root.join(&directory), None)? {
            entries.insert(directory.join(relative), snapshot);
        }
    }
    let description = metadata_root.join(GIT_DESCRIPTION_PATH);
    entries.insert(
        PathBuf::from(GIT_DESCRIPTION_PATH),
        WorkspaceEntrySnapshot::File {
            content: fs::read(&description)?,
            mode: worktree_mode(&description)?,
            links: worktree_link_count(&description)?,
        },
    );
    Ok(entries)
}

pub(crate) fn git_static_metadata_modified_times(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, SystemTime>> {
    let repository = Repository::open(root)?;
    git_static_metadata_entries(root)?
        .into_iter()
        .filter_map(|(relative, snapshot)| {
            matches!(
                snapshot,
                WorkspaceEntrySnapshot::Directory { .. } | WorkspaceEntrySnapshot::File { .. }
            )
            .then_some(relative)
        })
        .map(|relative| {
            let modified = fs::symlink_metadata(repository.path().join(&relative))?.modified()?;
            Ok((relative, modified))
        })
        .collect()
}

pub(crate) fn git_static_metadata_entry_identities(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, FilesystemIdentity>> {
    #[cfg(unix)]
    {
        let repository = Repository::open(root)?;
        return git_static_metadata_entries(root)?
            .into_iter()
            .filter_map(|(relative, snapshot)| {
                matches!(
                    snapshot,
                    WorkspaceEntrySnapshot::Directory { .. } | WorkspaceEntrySnapshot::File { .. }
                )
                .then_some(relative)
            })
            .map(|relative| {
                let metadata = fs::metadata(repository.path().join(&relative))?;
                Ok((
                    relative,
                    FilesystemIdentity {
                        device: metadata.dev(),
                        inode: metadata.ino(),
                        user_id: metadata.uid(),
                        group_id: metadata.gid(),
                        change_time_seconds: metadata.ctime(),
                        change_time_nanoseconds: metadata.ctime_nsec(),
                    },
                ))
            })
            .collect();
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        Ok(BTreeMap::new())
    }
}

pub(crate) fn git_fixture_modes_match(
    root: &Path,
    expected: &BTreeMap<PathBuf, Option<u32>>,
) -> EvalResult<bool> {
    for (path, expected_mode) in expected {
        let path = root.join(path);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_file() || worktree_mode(&path)? != *expected_mode {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn git_fixture_snapshot_matches(
    root: &Path,
    repository: &Repository,
    expected: &GitFixtureSnapshot,
) -> EvalResult<bool> {
    let config = match fs::read(repository.path().join(GIT_CONFIG_PATH)) {
        Ok(config) => config,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    Ok(git_fixture_modes_match(root, &expected.modes)?
        && config == expected.config
        && git_metadata_root_kind(root)? == expected.metadata_root_kind
        && worktree_mode(repository.path())? == expected.metadata_root_mode
        && filesystem_identity_matches_without_change_time(
            git_metadata_root_identity(root)?,
            expected.metadata_root_identity,
        )
        && git_static_metadata_entries(root)? == expected.static_metadata_entries
        && git_static_metadata_modified_times(root)? == expected.static_metadata_modified_times
        && git_static_metadata_entry_identities(root)? == expected.static_metadata_entry_identities)
}

pub(crate) fn git_forced_metadata_root_modified_time_matches(
    root: &Path,
    case_name: &str,
    seed_fixture: &GitFixtureSnapshot,
    pre_execution: Option<SystemTime>,
    pre_execution_identity: Option<FilesystemIdentity>,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> EvalResult<bool> {
    let actual_identity = git_metadata_root_identity(root)?;
    let expected_identity = pre_execution_identity.or(seed_fixture.metadata_root_identity);
    if matches!(
        case_name,
        GIT_BRANCH_SWITCH_NAME | GIT_CREATE_COMMIT_NAME | GIT_STAGE_NAME
    ) {
        let actual_modified = git_metadata_root_modified_time(root)?;
        return Ok(git_mutated_metadata_root_times_match(
            actual_modified,
            actual_identity,
            expected_identity,
            execution_window,
        ));
    }
    let Some(expected) = pre_execution.or(seed_fixture.metadata_root_modified_time) else {
        return Ok(false);
    };
    Ok(git_metadata_root_modified_time(root)? == expected && actual_identity == expected_identity)
}

pub(crate) fn git_natural_metadata_root_times_match(
    root: &Path,
    seed_fixture: &GitFixtureSnapshot,
    stage_execution_window: Option<FilesystemExecutionTimeWindow>,
    commit_execution_window: Option<FilesystemExecutionTimeWindow>,
) -> EvalResult<bool> {
    let actual_modified = git_metadata_root_modified_time(root)?;
    let actual_identity = git_metadata_root_identity(root)?;
    Ok(git_mutated_metadata_root_times_match(
        actual_modified,
        actual_identity,
        seed_fixture.metadata_root_identity,
        stage_execution_window,
    ) || git_mutated_metadata_root_times_match(
        actual_modified,
        actual_identity,
        seed_fixture.metadata_root_identity,
        commit_execution_window,
    ))
}

pub(crate) fn git_mutated_metadata_root_times_match(
    actual_modified: SystemTime,
    actual_identity: Option<FilesystemIdentity>,
    expected_identity: Option<FilesystemIdentity>,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> bool {
    filesystem_identity_matches_without_change_time(actual_identity, expected_identity)
        && execution_window.is_some_and(|window| {
            window.contains_modified(actual_modified)
                && actual_identity.is_some_and(|identity| window.contains_change_time(identity))
        })
}

pub(crate) fn git_forced_metadata_top_level_matches(
    root: &Path,
    case_name: &str,
    seed_fixture: &GitFixtureSnapshot,
    pre_execution: Option<&BTreeMap<PathBuf, GitMetadataEntrySnapshot>>,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> EvalResult<bool> {
    let actual = git_metadata_top_level(root)?;
    let mut expected = pre_execution
        .cloned()
        .unwrap_or_else(|| seed_fixture.metadata_top_level.clone());
    match case_name {
        GIT_BRANCH_SWITCH_NAME => {
            if !admit_git_metadata_file_mutation(
                &actual,
                &mut expected,
                Path::new(GIT_HEAD_PATH),
                execution_window,
            ) || !admit_git_metadata_file_mutation(
                &actual,
                &mut expected,
                Path::new(GIT_INDEX_PATH),
                execution_window,
            ) {
                return Ok(false);
            }
        }
        GIT_CREATE_COMMIT_NAME => {
            if !admit_git_metadata_modified_time(
                &actual,
                &mut expected,
                Path::new(GIT_OBJECTS_DIRECTORY),
                execution_window,
            ) || !admit_git_metadata_modified_time(
                &actual,
                &mut expected,
                Path::new(GIT_LOGS_DIRECTORY),
                execution_window,
            ) {
                return Ok(false);
            }
            expected.remove(Path::new(GIT_MERGE_HEAD_PATH));
            expected.remove(Path::new(GIT_MERGE_MESSAGE_PATH));
            expected.remove(Path::new(GIT_MERGE_MODE_PATH));
        }
        GIT_STAGE_NAME => {
            if !admit_git_metadata_file_mutation(
                &actual,
                &mut expected,
                Path::new(GIT_INDEX_PATH),
                execution_window,
            ) || !admit_git_metadata_modified_time(
                &actual,
                &mut expected,
                Path::new(GIT_OBJECTS_DIRECTORY),
                execution_window,
            ) {
                return Ok(false);
            }
        }
        GIT_BRANCH_CREATE_NAME | GIT_DIFF_NAME | GIT_LOG_NAME | GIT_STATUS_NAME => {}
        _ => return Ok(false),
    }
    Ok(actual == expected)
}

pub(crate) fn git_natural_metadata_top_level_matches(
    root: &Path,
    seed_fixture: &GitFixtureSnapshot,
    stage_execution_window: Option<FilesystemExecutionTimeWindow>,
    commit_execution_window: Option<FilesystemExecutionTimeWindow>,
) -> EvalResult<bool> {
    let actual = git_metadata_top_level(root)?;
    let mut expected = seed_fixture.metadata_top_level.clone();
    if !admit_git_metadata_file_mutation(
        &actual,
        &mut expected,
        Path::new(GIT_INDEX_PATH),
        stage_execution_window,
    ) {
        return Ok(false);
    }
    if !admit_git_metadata_modified_time(
        &actual,
        &mut expected,
        Path::new(GIT_OBJECTS_DIRECTORY),
        commit_execution_window,
    ) {
        return Ok(false);
    }
    if !admit_git_metadata_modified_time(
        &actual,
        &mut expected,
        Path::new(GIT_LOGS_DIRECTORY),
        commit_execution_window,
    ) {
        return Ok(false);
    }
    Ok(actual == expected)
}

pub(crate) fn admit_git_metadata_file_mutation(
    actual: &BTreeMap<PathBuf, GitMetadataEntrySnapshot>,
    expected: &mut BTreeMap<PathBuf, GitMetadataEntrySnapshot>,
    path: &Path,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> bool {
    let Some(actual) = actual.get(path) else {
        return false;
    };
    let Some(expected) = expected.get_mut(path) else {
        return false;
    };
    if !filesystem_ownership_matches(actual.identity.as_ref(), expected.identity.as_ref())
        || !git_metadata_times_match_execution(actual, execution_window)
    {
        return false;
    }
    expected.content.clone_from(&actual.content);
    expected.modified = actual.modified;
    expected.identity = actual.identity;
    true
}

pub(crate) fn admit_git_metadata_modified_time(
    actual: &BTreeMap<PathBuf, GitMetadataEntrySnapshot>,
    expected: &mut BTreeMap<PathBuf, GitMetadataEntrySnapshot>,
    path: &Path,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> bool {
    let Some(actual) = actual.get(path) else {
        return false;
    };
    let Some(expected) = expected.get_mut(path) else {
        return false;
    };
    if actual.modified != expected.modified
        && !actual.modified.is_some_and(|modified| {
            execution_window.is_some_and(|window| window.contains_modified(modified))
        })
    {
        return false;
    }
    expected.modified = actual.modified;
    match (expected.identity.as_mut(), actual.identity) {
        (Some(expected_identity), Some(actual_identity)) => {
            if actual_identity != *expected_identity
                && !filesystem_identity_matches_without_change_time(
                    Some(actual_identity),
                    Some(*expected_identity),
                )
            {
                return false;
            }
            if actual_identity != *expected_identity
                && !execution_window
                    .is_some_and(|window| window.contains_change_time(actual_identity))
            {
                return false;
            }
            admit_filesystem_change_time(expected_identity, actual_identity);
        }
        (None, None) => {}
        _ => return false,
    }
    true
}

pub(crate) fn git_metadata_times_match_execution(
    snapshot: &GitMetadataEntrySnapshot,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> bool {
    snapshot.modified.is_some_and(|modified| {
        execution_window.is_some_and(|window| window.contains_modified(modified))
    }) && snapshot.identity.is_some_and(|identity| {
        execution_window.is_some_and(|window| window.contains_change_time(identity))
    })
}

pub(crate) fn git_forced_worktree_modified_times_match(
    root: &Path,
    case_name: &str,
    seed_fixture: &GitFixtureSnapshot,
    pre_execution: Option<&BTreeMap<PathBuf, SystemTime>>,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> EvalResult<bool> {
    let mut actual = git_worktree_modified_times(root)?;
    let mut expected = pre_execution
        .cloned()
        .unwrap_or_else(|| seed_fixture.worktree_modified_times.clone());
    match case_name {
        GIT_BRANCH_SWITCH_NAME => {
            let target = Path::new(GIT_SEED_PATH);
            let Some(actual_modified) = actual.remove(target) else {
                return Ok(false);
            };
            if !execution_window.is_some_and(|window| window.contains_modified(actual_modified)) {
                return Ok(false);
            }
            expected.remove(target);
        }
        GIT_BRANCH_CREATE_NAME
        | GIT_CREATE_COMMIT_NAME
        | GIT_DIFF_NAME
        | GIT_LOG_NAME
        | GIT_STAGE_NAME
        | GIT_STATUS_NAME => {}
        _ => return Ok(false),
    }
    Ok(actual == expected)
}

pub(crate) fn git_forced_worktree_entry_identities_match(
    root: &Path,
    case_name: &str,
    seed_fixture: &GitFixtureSnapshot,
    pre_execution: Option<&BTreeMap<PathBuf, FilesystemIdentity>>,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> EvalResult<bool> {
    let mut actual = git_worktree_entry_identities(root)?;
    let mut expected = pre_execution
        .cloned()
        .unwrap_or_else(|| seed_fixture.worktree_entry_identities.clone());
    match case_name {
        GIT_BRANCH_SWITCH_NAME => {
            let target = Path::new(GIT_SEED_PATH);
            if !git_branch_switch_target_identity_matches(
                actual.get(target),
                expected.get(target),
                execution_window,
            ) {
                return Ok(false);
            }
            actual.remove(target);
            expected.remove(target);
        }
        GIT_BRANCH_CREATE_NAME
        | GIT_CREATE_COMMIT_NAME
        | GIT_DIFF_NAME
        | GIT_LOG_NAME
        | GIT_STAGE_NAME
        | GIT_STATUS_NAME => {}
        _ => return Ok(false),
    }
    Ok(actual == expected)
}

pub(crate) fn git_branch_switch_target_identity_matches(
    actual: Option<&FilesystemIdentity>,
    expected: Option<&FilesystemIdentity>,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> bool {
    filesystem_ownership_matches(actual, expected)
        && actual.is_some_and(|identity| {
            execution_window.is_some_and(|window| window.contains_change_time(*identity))
        })
}

pub(crate) fn git_forced_worktree_extended_attributes_match(
    root: &Path,
    seed_fixture: &GitFixtureSnapshot,
    pre_execution: Option<&BTreeMap<PathBuf, ExtendedAttributeSnapshot>>,
) -> EvalResult<bool> {
    let expected = pre_execution
        .cloned()
        .unwrap_or_else(|| seed_fixture.worktree_extended_attributes.clone());
    Ok(git_worktree_extended_attributes(root)? == expected)
}

pub(crate) fn git_metadata_extended_attributes_match(
    root: &Path,
    seed_fixture: &GitFixtureSnapshot,
    pre_execution: Option<&BTreeMap<PathBuf, ExtendedAttributeSnapshot>>,
) -> EvalResult<bool> {
    let actual = git_metadata_extended_attributes(root)?;
    let baseline = pre_execution.unwrap_or(&seed_fixture.metadata_extended_attributes);
    let creation_attributes = creation_extended_attributes(baseline);
    let expected = actual
        .keys()
        .map(|path| {
            (
                path.clone(),
                baseline
                    .get(path)
                    .cloned()
                    .unwrap_or_else(|| creation_attributes.clone()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    Ok(actual == expected)
}

pub(crate) fn git_forced_worktree_matches(
    root: &Path,
    case_name: &str,
    seed_fixture: &GitFixtureSnapshot,
    pre_execution_worktree_entries: Option<&BTreeMap<PathBuf, WorkspaceEntrySnapshot>>,
) -> EvalResult<bool> {
    let actual = git_worktree_entries(root)?;
    let mut expected = pre_execution_worktree_entries
        .cloned()
        .unwrap_or_else(|| seed_fixture.worktree_entries.clone());
    match case_name {
        GIT_BRANCH_SWITCH_NAME => {
            let Some(WorkspaceEntrySnapshot::File { mode, links, .. }) =
                expected.get(Path::new(GIT_SEED_PATH))
            else {
                return Ok(false);
            };
            let mode = *mode;
            let links = *links;
            expected.insert(
                PathBuf::from(GIT_SEED_PATH),
                WorkspaceEntrySnapshot::File {
                    content: GIT_SWITCH_CONTENT.as_bytes().to_vec(),
                    mode,
                    links,
                },
            );
        }
        GIT_DIFF_NAME if pre_execution_worktree_entries.is_none() => {
            if !insert_expected_file_with_observed_mode(
                &actual,
                &mut expected,
                Path::new(GIT_DIFF_OVERFLOW_PATH),
                git_diff_overflow_content().as_bytes(),
            ) {
                return Ok(false);
            }
        }
        GIT_STATUS_NAME if pre_execution_worktree_entries.is_none() => {
            let directory = Path::new(GIT_STATUS_OVERFLOW_DIRECTORY);
            let Some(WorkspaceEntrySnapshot::Directory { mode }) = actual.get(directory) else {
                return Ok(false);
            };
            expected.insert(
                directory.to_path_buf(),
                WorkspaceEntrySnapshot::Directory { mode: *mode },
            );
            for index in 0..GIT_STATUS_OVERFLOW_ENTRY_COUNT {
                if !insert_expected_file_with_observed_mode(
                    &actual,
                    &mut expected,
                    Path::new(&git_status_overflow_path(index)),
                    GIT_STATUS_OVERFLOW_CONTENT.as_bytes(),
                ) {
                    return Ok(false);
                }
            }
        }
        _ => {}
    }
    Ok(actual == expected)
}

pub(crate) fn insert_expected_file_with_observed_mode(
    actual: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    expected: &mut BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    path: &Path,
    content: &[u8],
) -> bool {
    let Some(WorkspaceEntrySnapshot::File { mode, links, .. }) = actual.get(path) else {
        return false;
    };
    expected.insert(
        path.to_path_buf(),
        WorkspaceEntrySnapshot::File {
            content: content.to_vec(),
            mode: *mode,
            links: *links,
        },
    );
    true
}

pub(crate) fn commit_git_seed_revision<'repository>(
    repository: &'repository Repository,
    parent: &git2::Commit<'repository>,
    content: &str,
    message: &str,
) -> EvalResult<git2::Commit<'repository>> {
    let root = repository
        .workdir()
        .ok_or_else(|| io::Error::other("the Git eval repository has no worktree"))?;
    fs::write(root.join(GIT_SEED_PATH), content)?;
    let mut index = repository.index()?;
    index.add_path(Path::new(GIT_SEED_PATH))?;
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = repository.find_tree(tree_id)?;
    let signature = Signature::now(GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL)?;
    let commit = repository.commit(
        Some("HEAD"),
        &signature,
        &signature,
        message,
        &tree,
        &[parent],
    )?;
    repository.find_commit(commit).map_err(Into::into)
}

pub(crate) fn stage_path(root: &Path, path: &str) -> EvalResult {
    let repository = Repository::open(root)?;
    let mut index = repository.index()?;
    index.add_all([path], IndexAddOption::DEFAULT, None)?;
    index.write()?;
    Ok(())
}

pub(crate) fn drift_git_index_ctime(root: &Path, path: &str) -> EvalResult {
    let repository = Repository::open(root)?;
    let mut index = repository.index()?;
    let mut entry = index
        .get_path(Path::new(path), 0)
        .ok_or_else(|| io::Error::other("the Git index drift fixture is missing"))?;
    entry.ctime = IndexTime::new(
        entry.ctime.seconds().wrapping_add(1),
        entry.ctime.nanoseconds(),
    );
    index.add(&entry)?;
    index.write()?;
    Ok(())
}

pub(crate) fn install_git_merge_state(root: &Path, seed: Oid) -> EvalResult {
    let repository = Repository::open(root)?;
    let merge_parent = repository.find_commit(seed)?.parent_id(0)?;
    fs::write(
        repository.path().join(GIT_MERGE_HEAD_PATH),
        format!("{merge_parent}\n"),
    )?;
    fs::write(
        repository.path().join(GIT_MERGE_MESSAGE_PATH),
        GIT_MERGE_MESSAGE,
    )?;
    fs::write(repository.path().join(GIT_MERGE_MODE_PATH), GIT_MERGE_MODE)?;
    Ok(())
}

pub(crate) fn commit_staged_paths(root: &Path, message: &str) -> EvalResult {
    commit_staged_paths_with_identity(root, message, GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL)
}

pub(crate) fn commit_staged_paths_with_identity(
    root: &Path,
    message: &str,
    author_name: &str,
    author_email: &str,
) -> EvalResult {
    let repository = Repository::open(root)?;
    let mut index = repository.index()?;
    let tree_id = index.write_tree()?;
    let tree = repository.find_tree(tree_id)?;
    let parent = repository.head()?.peel_to_commit()?;
    let merge_parent = fs::read_to_string(repository.path().join(GIT_MERGE_HEAD_PATH))
        .ok()
        .map(|value| Oid::from_str(value.trim()))
        .transpose()?
        .map(|oid| repository.find_commit(oid))
        .transpose()?;
    let parents = merge_parent
        .as_ref()
        .map_or_else(|| vec![&parent], |merge_parent| vec![&parent, merge_parent]);
    let signature = Signature::now(author_name, author_email)?;
    let commit = repository.commit(
        Some("HEAD"),
        &signature,
        &signature,
        message,
        &tree,
        &parents,
    )?;
    normalize_latest_reflog_message(&repository, "HEAD", commit, &signature)?;
    normalize_latest_reflog_message(
        &repository,
        &format!("refs/heads/{GIT_BASE_BRANCH}"),
        commit,
        &signature,
    )?;
    for state_path in [
        GIT_MERGE_HEAD_PATH,
        GIT_MERGE_MESSAGE_PATH,
        GIT_MERGE_MODE_PATH,
    ] {
        match fs::remove_file(repository.path().join(state_path)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(crate) fn normalize_latest_reflog_message(
    repository: &Repository,
    reference: &str,
    commit: Oid,
    signature: &Signature<'_>,
) -> EvalResult {
    let mut reflog = repository.reflog(reference)?;
    reflog.remove(0, false)?;
    reflog.append(commit, signature, Some(GIT_COMMIT_REFLOG_MESSAGE))?;
    reflog.write()?;
    Ok(())
}

pub(crate) fn replace_latest_reflog_signature(
    repository: &Repository,
    reference: &str,
    signature: &Signature<'_>,
) -> EvalResult {
    let mut reflog = repository.reflog(reference)?;
    let latest = reflog
        .get(0)
        .ok_or_else(|| io::Error::other("the Git fixture reflog has no latest entry"))?;
    let commit = latest.id_new();
    let message = latest
        .message()
        .ok()
        .flatten()
        .ok_or_else(|| io::Error::other("the Git fixture reflog message is not valid UTF-8"))?
        .to_owned();
    reflog.remove(0, false)?;
    reflog.append(commit, signature, Some(&message))?;
    reflog.write()?;
    Ok(())
}

pub(crate) fn git_natural_state_passed(
    root: &Path,
    seed: Oid,
    seed_refs: &GitReferenceInventory,
    seed_fixture: &GitFixtureSnapshot,
) -> EvalResult<bool> {
    let repository = Repository::open(root)?;
    let head = repository.head()?.peel_to_commit()?;
    let recorded = GitRecordedTime::from(head.author().when());
    git_natural_state_passed_in_window(
        root,
        seed,
        seed_refs,
        seed_fixture,
        GitNaturalExecutionVerification {
            execution_window: Some(GitExecutionTimeWindow {
                started: recorded,
                finished: recorded,
            }),
            stage_filesystem_execution_window: None,
            commit_filesystem_execution_window: None,
            pre_commit_object_entries: None,
            pre_commit_object_modified_times: None,
            pre_commit_object_entry_identities: None,
        },
    )
}

pub(crate) struct GitNaturalExecutionVerification<'a> {
    pub(crate) execution_window: Option<GitExecutionTimeWindow>,
    pub(crate) stage_filesystem_execution_window: Option<FilesystemExecutionTimeWindow>,
    pub(crate) commit_filesystem_execution_window: Option<FilesystemExecutionTimeWindow>,
    pub(crate) pre_commit_object_entries: Option<&'a BTreeMap<PathBuf, WorkspaceEntrySnapshot>>,
    pub(crate) pre_commit_object_modified_times: Option<&'a BTreeMap<PathBuf, SystemTime>>,
    pub(crate) pre_commit_object_entry_identities:
        Option<&'a BTreeMap<PathBuf, FilesystemIdentity>>,
}

pub(crate) fn git_natural_state_passed_in_window(
    root: &Path,
    seed: Oid,
    seed_refs: &GitReferenceInventory,
    seed_fixture: &GitFixtureSnapshot,
    verification: GitNaturalExecutionVerification<'_>,
) -> EvalResult<bool> {
    let GitNaturalExecutionVerification {
        execution_window,
        stage_filesystem_execution_window,
        commit_filesystem_execution_window,
        pre_commit_object_entries,
        pre_commit_object_modified_times,
        pre_commit_object_entry_identities,
    } = verification;
    let repository = Repository::open(root)?;
    let head_reference = repository.head()?;
    let head_remains_on_seeded_branch = head_reference.shorthand().ok() == Some(GIT_BASE_BRANCH);
    let head = head_reference.peel_to_commit()?;
    let base = repository.find_branch(GIT_BASE_BRANCH, BranchType::Local)?;
    let seeded_branch_advanced = base.get().target() == Some(head.id());
    let message_matches = head.message()? == GIT_NATURAL_MESSAGE;
    let identity_matches = head.author().name().ok() == Some(GIT_AUTHOR_NAME)
        && head.author().email().ok() == Some(GIT_AUTHOR_EMAIL)
        && head.committer().name().ok() == Some(GIT_AUTHOR_NAME)
        && head.committer().email().ok() == Some(GIT_AUTHOR_EMAIL);
    let signature_times_match = git_commit_times_match_execution(
        head.author().when(),
        head.committer().when(),
        execution_window,
    );
    let Ok(parent) = head.parent(0) else {
        return Ok(false);
    };
    let exactly_one_descendant_commit = parent.id() == seed;
    let parent_tree = parent.tree()?;
    let head_tree = head.tree()?;
    let diff = repository.diff_tree_to_tree(Some(&parent_tree), Some(&head_tree), None)?;
    let changed_paths = diff
        .deltas()
        .filter_map(|delta| delta.new_file().path())
        .collect::<Vec<_>>();
    let commit_changes_only_natural_path = changed_paths == [Path::new(GIT_NATURAL_PATH)];
    let natural_path_is_clean =
        repository.status_file(Path::new(GIT_NATURAL_PATH))? == Status::CURRENT;
    let index_matches = git_index_entries(&repository)?
        == git_index_with_expected_file(
            seed_fixture,
            GIT_NATURAL_PATH,
            GIT_NATURAL_CONTENT.as_bytes(),
        )?
        && git_index_extensions(&repository)? == seed_fixture.index_extensions
        && git_index_complete_entries_match(
            root,
            &repository,
            &seed_fixture.index_complete_entries,
            Some(GIT_NATURAL_PATH),
        )?;
    let commit_adds_expected_natural_fixture = commit_adds_exact_fixture(
        &repository,
        &head,
        GIT_NATURAL_PATH,
        GIT_NATURAL_CONTENT.as_bytes(),
        1,
    )?;
    let unrelated_fixtures_unchanged = repository.status_file(Path::new(GIT_SEED_PATH))?
        == Status::CURRENT
        && fs::read(root.join(GIT_SEED_PATH))? == GIT_BASE_CONTENT.as_bytes()
        && untracked_git_fixture_matches(
            root,
            &repository,
            GIT_STAGE_PATH,
            GIT_STAGE_CONTENT.as_bytes(),
        )?
        && untracked_git_fixture_matches(
            root,
            &repository,
            GIT_COMMIT_PATH,
            GIT_COMMIT_CONTENT.as_bytes(),
        )?;
    let complete_status_matches = git_natural_status_matches(&repository)?;
    let operation_state_is_clean = git_operation_state_is_clean(&repository);
    let mut expected_refs = seed_refs.clone();
    expected_refs.insert(
        format!("refs/heads/{GIT_BASE_BRANCH}").into_bytes(),
        GitReferenceTarget::Direct(head.id()),
    );
    let complete_ref_inventory_matches = git_reference_inventory(&repository)? == expected_refs;
    let complete_reference_entry_inventory_matches = git_natural_reference_entries_match(
        root,
        &head,
        seed_fixture,
        commit_filesystem_execution_window,
    )?;
    let complete_object_inventory_matches =
        git_natural_objects_match(&repository, &head, seed_fixture)?;
    let complete_object_entry_inventory_matches = git_natural_object_entries_match(
        root,
        &head,
        seed_fixture,
        stage_filesystem_execution_window,
        GitObjectEntryVerification {
            pre_execution_entries: pre_commit_object_entries,
            pre_execution_modified_times: pre_commit_object_modified_times,
            pre_execution_entry_identities: pre_commit_object_entry_identities,
            execution_window: commit_filesystem_execution_window,
        },
    )?;
    let fixture_matches = git_fixture_snapshot_matches(root, &repository, seed_fixture)?;
    let metadata_root_times_match = git_natural_metadata_root_times_match(
        root,
        seed_fixture,
        stage_filesystem_execution_window,
        commit_filesystem_execution_window,
    )?;
    let reflogs_match = git_reflog_updates_match(
        root,
        &["HEAD", format!("refs/heads/{GIT_BASE_BRANCH}").as_str()],
        seed_fixture,
        GitReflogUpdateExpectation {
            old: seed,
            new: head.id(),
            message: GIT_COMMIT_REFLOG_MESSAGE,
            execution_window: None,
            filesystem_execution_window: commit_filesystem_execution_window,
        },
    )?;
    let complete_worktree_inventory_matches =
        git_worktree_entries(root)? == seed_fixture.worktree_entries;
    let complete_worktree_time_inventory_matches =
        git_worktree_modified_times(root)? == seed_fixture.worktree_modified_times;
    let complete_worktree_identity_inventory_matches =
        git_natural_worktree_entry_identities_match(root, seed_fixture)?;
    let complete_worktree_attribute_inventory_matches =
        git_worktree_extended_attributes(root)? == seed_fixture.worktree_extended_attributes;
    let metadata_top_level_matches = git_natural_metadata_top_level_matches(
        root,
        seed_fixture,
        stage_filesystem_execution_window,
        commit_filesystem_execution_window,
    )?;
    let metadata_extended_attributes_match =
        git_metadata_extended_attributes_match(root, seed_fixture, None)?;
    Ok(head_remains_on_seeded_branch
        && seeded_branch_advanced
        && message_matches
        && identity_matches
        && signature_times_match
        && exactly_one_descendant_commit
        && commit_changes_only_natural_path
        && natural_path_is_clean
        && index_matches
        && commit_adds_expected_natural_fixture
        && unrelated_fixtures_unchanged
        && complete_status_matches
        && operation_state_is_clean
        && complete_ref_inventory_matches
        && complete_reference_entry_inventory_matches
        && complete_object_inventory_matches
        && complete_object_entry_inventory_matches
        && fixture_matches
        && metadata_root_times_match
        && reflogs_match
        && complete_worktree_inventory_matches
        && complete_worktree_time_inventory_matches
        && complete_worktree_identity_inventory_matches
        && complete_worktree_attribute_inventory_matches
        && metadata_top_level_matches
        && metadata_extended_attributes_match)
}

pub(crate) fn git_natural_worktree_entry_identities_match(
    root: &Path,
    seed_fixture: &GitFixtureSnapshot,
) -> EvalResult<bool> {
    Ok(git_worktree_entry_identities(root)? == seed_fixture.worktree_entry_identities)
}

pub(crate) fn git_natural_objects_match(
    repository: &Repository,
    head: &git2::Commit<'_>,
    seed_fixture: &GitFixtureSnapshot,
) -> EvalResult<bool> {
    let actual = match git_object_inventory(repository) {
        Ok(actual) => actual,
        Err(_) => return Ok(false),
    };
    let mut expected = seed_fixture.objects.clone();
    let blob_id = Oid::hash_object(ObjectType::Blob, GIT_NATURAL_CONTENT.as_bytes())?;
    let Some(blob) = actual.get(&blob_id) else {
        return Ok(false);
    };
    let Some(tree) = actual.get(&head.tree_id()) else {
        return Ok(false);
    };
    let Some(commit) = actual.get(&head.id()) else {
        return Ok(false);
    };
    expected.insert(blob_id, blob.clone());
    expected.insert(head.tree_id(), tree.clone());
    expected.insert(head.id(), commit.clone());
    Ok(actual == expected)
}

pub(crate) fn git_natural_object_entries_match(
    root: &Path,
    head: &git2::Commit<'_>,
    seed_fixture: &GitFixtureSnapshot,
    stage_execution_window: Option<FilesystemExecutionTimeWindow>,
    verification: GitObjectEntryVerification<'_>,
) -> EvalResult<bool> {
    let GitObjectEntryVerification {
        pre_execution_entries: pre_commit_entries,
        pre_execution_modified_times: pre_commit_modified_times,
        pre_execution_entry_identities: pre_commit_entry_identities,
        execution_window: commit_execution_window,
    } = verification;
    let Some(pre_commit_entries) = pre_commit_entries else {
        return Ok(false);
    };
    let Some(pre_commit_modified_times) = pre_commit_modified_times else {
        return Ok(false);
    };
    let Some(pre_commit_entry_identities) = pre_commit_entry_identities else {
        return Ok(false);
    };
    let staged_blob = Oid::hash_object(ObjectType::Blob, GIT_NATURAL_CONTENT.as_bytes())?;
    if !git_object_entry_inventory_snapshots_match(
        GitObjectEntrySnapshots {
            entries: pre_commit_entries,
            modified_times: pre_commit_modified_times,
            entry_identities: pre_commit_entry_identities,
        },
        GitObjectEntrySnapshots {
            entries: &seed_fixture.object_entries,
            modified_times: &seed_fixture.object_modified_times,
            entry_identities: &seed_fixture.object_entry_identities,
        },
        &[staged_blob],
        seed_fixture,
        stage_execution_window,
    )? {
        return Ok(false);
    }
    git_object_entry_inventory_matches(
        root,
        pre_commit_entries,
        pre_commit_modified_times,
        pre_commit_entry_identities,
        &[head.tree_id(), head.id()],
        seed_fixture,
        commit_execution_window,
    )
}

pub(crate) fn git_natural_reference_entries_match(
    root: &Path,
    head: &git2::Commit<'_>,
    seed_fixture: &GitFixtureSnapshot,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> EvalResult<bool> {
    let mut expected = seed_fixture.reference_entries.clone();
    let actual_modified_times = git_reference_modified_times(root)?;
    let mut expected_modified_times = seed_fixture.reference_modified_times.clone();
    let actual_entry_identities = git_reference_entry_identities(root)?;
    let mut expected_entry_identities = seed_fixture.reference_entry_identities.clone();
    let base_path = Path::new("heads").join(GIT_BASE_BRANCH);
    let Some(template) = expected.get(&base_path) else {
        return Ok(false);
    };
    let Some(entry) = direct_git_reference_entry(template, head.id()) else {
        return Ok(false);
    };
    if !admit_modified_time_path_and_ancestors(
        &actual_modified_times,
        &mut expected_modified_times,
        &actual_entry_identities,
        &base_path,
        execution_window,
    ) || !admit_filesystem_identity_path(
        &actual_entry_identities,
        &mut expected_entry_identities,
        &base_path,
        execution_window,
    ) {
        return Ok(false);
    }
    expected.insert(base_path.clone(), entry);
    Ok(git_reference_entries(root)? == expected
        && actual_modified_times == expected_modified_times
        && actual_entry_identities == expected_entry_identities)
}

pub(crate) fn git_natural_status_matches(repository: &Repository) -> EvalResult<bool> {
    let statuses = repository.statuses(None)?;
    let mut actual = BTreeMap::new();
    for entry in statuses.iter() {
        let path = entry
            .path()
            .map_err(|_| io::Error::other("a Git status path is not valid UTF-8"))?;
        actual.insert(path.to_owned(), entry.status());
    }
    let expected = BTreeMap::from([
        (String::from(GIT_COMMIT_PATH), Status::WT_NEW),
        (String::from(GIT_STAGE_PATH), Status::WT_NEW),
    ]);
    Ok(actual == expected)
}

pub(crate) fn git_natural_result_payloads_passed(
    root: &Path,
    snapshot: &CaseSnapshot,
    tracker: &OperationTracker,
) -> EvalResult<bool> {
    let Some(stage) = snapshot
        .requests
        .iter()
        .find(|request| request.name == GIT_STAGE_NAME)
    else {
        return Ok(false);
    };
    let Some(commit) = snapshot
        .requests
        .iter()
        .find(|request| request.name == GIT_CREATE_COMMIT_NAME)
    else {
        return Ok(false);
    };
    let Some(stage_content) = tracker.result_content(stage.request_id) else {
        return Ok(false);
    };
    let Some(commit_content) = tracker.result_content(commit.request_id) else {
        return Ok(false);
    };
    let Ok(stage_result) = serde_json::from_str::<serde_json::Value>(&stage_content) else {
        return Ok(false);
    };
    let Ok(commit_result) = serde_json::from_str::<serde_json::Value>(&commit_content) else {
        return Ok(false);
    };
    let head = Repository::open(root)?.head()?.peel_to_commit()?.id();
    Ok(
        json_object_has_exact_fields(&stage_result, &["staged_paths", EVAL_RECEIPT_FIELD])
            && json_object_has_exact_fields(
                &commit_result,
                &["commit", "state_cleaned", EVAL_RECEIPT_FIELD],
            )
            && stage_result["staged_paths"] == GIT_NATURAL_STAGED_PATH_COUNT
            && commit_result["commit"] == head.to_string()
            && commit_result["state_cleaned"] == true,
    )
}

pub(crate) fn untracked_git_fixture_matches(
    root: &Path,
    repository: &Repository,
    path: &str,
    expected: &[u8],
) -> EvalResult<bool> {
    let metadata = match fs::symlink_metadata(root.join(path)) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() {
        return Ok(false);
    }
    match fs::read(root.join(path)) {
        Ok(bytes) => {
            Ok(bytes == expected && repository.status_file(Path::new(path))? == Status::WT_NEW)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[derive(Clone)]
pub(crate) struct GitObjectCapture {
    pub(crate) root: PathBuf,
    pub(crate) entries: Arc<StdMutex<Option<BTreeMap<PathBuf, WorkspaceEntrySnapshot>>>>,
    pub(crate) modified_times: Arc<StdMutex<Option<BTreeMap<PathBuf, SystemTime>>>>,
    pub(crate) entry_identities: Arc<StdMutex<Option<BTreeMap<PathBuf, FilesystemIdentity>>>>,
}

pub(crate) struct GitNaturalFilesystemFixture {
    pub(crate) suite: FamilySuite,
    pub(crate) stage_window: FilesystemExecutionTimeWindow,
    pub(crate) commit_window: FilesystemExecutionTimeWindow,
}

pub(crate) fn git_natural_filesystem_fixture() -> EvalResult<GitNaturalFilesystemFixture> {
    let suite = FamilySuite::git()?;
    let stage_started = current_filesystem_recorded_time()?;
    stage_path(suite.workspace.path(), GIT_NATURAL_PATH)?;
    let stage_window = FilesystemExecutionTimeWindow {
        started: stage_started,
        finished: current_filesystem_recorded_time()?,
    };
    suite
        .executor
        .capture_git_objects_before_commit(GIT_CREATE_COMMIT_NAME)?;
    let commit_started = current_filesystem_recorded_time()?;
    commit_staged_paths(suite.workspace.path(), GIT_NATURAL_MESSAGE)?;
    let commit_window = FilesystemExecutionTimeWindow {
        started: commit_started,
        finished: current_filesystem_recorded_time()?,
    };
    Ok(GitNaturalFilesystemFixture {
        suite,
        stage_window,
        commit_window,
    })
}

#[cfg(unix)]
pub(crate) struct GitBranchSwitchTimestampFixture {
    pub(crate) suite: FamilySuite,
    pub(crate) pre_worktree_modified_times: BTreeMap<PathBuf, SystemTime>,
    pub(crate) pre_worktree_entry_identities: BTreeMap<PathBuf, FilesystemIdentity>,
    pub(crate) pre_metadata_root_modified_time: SystemTime,
    pub(crate) pre_metadata_root_identity: FilesystemIdentity,
    pub(crate) execution_window: FilesystemExecutionTimeWindow,
}

#[cfg(unix)]
pub(crate) fn git_branch_switch_timestamp_fixture() -> EvalResult<GitBranchSwitchTimestampFixture> {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_BRANCH_SWITCH_NAME)?;
    let pre_worktree_modified_times = suite
        .git_pre_execution_worktree_modified_times
        .lock()
        .expect("Git pre-execution worktree-time lock is available")
        .clone()
        .expect("the Git branch-switch fixture has captured worktree times");
    let pre_worktree_entry_identities = suite
        .git_pre_execution_worktree_entry_identities
        .lock()
        .expect("Git pre-execution worktree-identity lock is available")
        .clone()
        .expect("the Git branch-switch fixture has captured worktree identities");
    let pre_metadata_root_modified_time = suite
        .git_pre_execution_metadata_root_modified_time
        .lock()
        .expect("Git pre-execution metadata-root-time lock is available")
        .expect("the Git branch-switch fixture has a captured metadata-root time");
    let pre_metadata_root_identity = suite
        .git_pre_execution_metadata_root_identity
        .lock()
        .expect("Git pre-execution metadata-root-identity lock is available")
        .expect("the Git branch-switch fixture has a captured metadata-root identity");
    let started = current_filesystem_recorded_time()?;
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("switch-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    repository.checkout_tree(target.as_object(), Some(CheckoutBuilder::new().force()))?;
    repository.set_head("refs/heads/switch-target")?;
    let execution_window = FilesystemExecutionTimeWindow {
        started,
        finished: current_filesystem_recorded_time()?,
    };

    Ok(GitBranchSwitchTimestampFixture {
        suite,
        pre_worktree_modified_times,
        pre_worktree_entry_identities,
        pre_metadata_root_modified_time,
        pre_metadata_root_identity,
        execution_window,
    })
}

pub(crate) fn forced_git_log_result(suite: &FamilySuite) -> EvalResult<String> {
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    Ok(serde_json::json!({
        "commits": [{
            "commit": target.id().to_string(),
            "author_name": target.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string())
}

#[cfg(unix)]
pub(crate) fn replace_git_metadata_file_byte_identically(
    target: &Path,
    target_modified: SystemTime,
    parent_modified: SystemTime,
) -> EvalResult {
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("the Git metadata fixture has no parent"))?;
    let replacement = parent.join("identity-replacement-fixture");
    let content = fs::read(target)?;
    let permissions = fs::metadata(target)?.permissions();
    fs::write(&replacement, content)?;
    fs::set_permissions(&replacement, permissions)?;
    fs::rename(&replacement, target)?;
    fs::File::open(target)?.set_times(fs::FileTimes::new().set_modified(target_modified))?;
    fs::File::open(parent)?.set_times(fs::FileTimes::new().set_modified(parent_modified))?;
    Ok(())
}

pub(crate) fn successful_git_natural_snapshot() -> EvalResult<CaseSnapshot> {
    Ok(CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
                )?,
                entry_index: GIT_NATURAL_STAGE_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(GIT_NATURAL_STAGE_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                )?,
                entry_index: GIT_NATURAL_COMMIT_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(GIT_NATURAL_COMMIT_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    })
}

pub(crate) struct GitNaturalResultFixture<'a> {
    pub(crate) staged_paths: usize,
    pub(crate) commit: &'a str,
    pub(crate) state_cleaned: bool,
}

pub(crate) fn record_git_natural_results(
    tracker: &OperationTracker,
    fixture: GitNaturalResultFixture<'_>,
) {
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &serde_json::json!({
            "staged_paths": fixture.staged_paths,
            EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
        })
        .to_string(),
    );
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
        &serde_json::json!({
            "commit": fixture.commit,
            "state_cleaned": fixture.state_cleaned,
            EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
        })
        .to_string(),
    );
}

pub(crate) fn prepared_git_natural_result_case() -> EvalResult<(FamilySuite, CaseSnapshot, String)>
{
    let suite = FamilySuite::git()?;
    stage_path(suite.workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(suite.workspace.path(), GIT_NATURAL_MESSAGE)?;
    let head = Repository::open(suite.workspace.path())?
        .head()?
        .peel_to_commit()?
        .id()
        .to_string();
    Ok((suite, successful_git_natural_snapshot()?, head))
}
