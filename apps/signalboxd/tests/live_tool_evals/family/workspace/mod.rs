//! Workspace evaluation fixtures and verification.

use crate::*;

mod tests;

pub(crate) const WORKSPACE_SEED_PATH: &str = "brief.txt";
pub(crate) const WORKSPACE_SEED: &str = "alpha\nbeta fixture\nalpha\n";
pub(crate) const WORKSPACE_EDITED_SEED: &str = "beta\nbeta fixture\nbeta\n";
pub(crate) const WORKSPACE_GLOB_DIRECTORY: &str = "glob-scope";
pub(crate) const WORKSPACE_GLOB_PATH: &str = "glob-scope/zz-glob.txt";
pub(crate) const WORKSPACE_GLOB_CONTENT: &str = "beta glob fixture\n";
pub(crate) const WORKSPACE_GLOB_OVERFLOW_PATH: &str = "glob-scope/zz-overflow.txt";
pub(crate) const WORKSPACE_GLOB_NONMATCHING_PATH: &str = "glob-scope/aa-nonmatching.md";
pub(crate) const WORKSPACE_GLOB_NONMATCHING_CONTENT: &str = "nonmatching glob fixture\n";
pub(crate) const WORKSPACE_DRIFTED_GLOB_CONTENT: &str = "drifted glob fixture\n";
pub(crate) const WORKSPACE_SEARCH_DIRECTORY: &str = "search-scope";
pub(crate) const WORKSPACE_SEARCH_PATH: &str = "search-scope/match.txt";
pub(crate) const WORKSPACE_SEARCH_CONTENT: &str =
    "search prelude\nbeta search fixture\nbeta search overflow\n";
pub(crate) const WORKSPACE_FORCED_READ_MAX_BYTES: usize = 6;
pub(crate) const WORKSPACE_DRIFTED_SEED: &str = "alpha\nbeta fixturE\nalpha\n";
#[cfg(unix)]
pub(crate) const WORKSPACE_PRIVATE_CREATION_MODE: u32 = 0o600;
#[cfg(unix)]
pub(crate) const WORKSPACE_INSECURE_CREATION_MODE: u32 = 0o777;
#[cfg(unix)]
pub(crate) const WORKSPACE_CREATED_FILE_MODE: Option<u32> = Some(WORKSPACE_PRIVATE_CREATION_MODE);
#[cfg(not(unix))]
pub(crate) const WORKSPACE_CREATED_FILE_MODE: Option<u32> = None;
#[cfg(unix)]
pub(crate) const WORKSPACE_CREATED_FILE_LINKS: Option<u64> = Some(1);
#[cfg(not(unix))]
pub(crate) const WORKSPACE_CREATED_FILE_LINKS: Option<u64> = None;
pub(crate) const WORKSPACE_LIST_PATH: &str = "nested-list";
pub(crate) const WORKSPACE_LIST_MAX_RESULTS: usize = 20;
pub(crate) const WORKSPACE_LIST_ENTRY_COUNT: usize = WORKSPACE_LIST_MAX_RESULTS + 1;
pub(crate) const WORKSPACE_GLOB_MAX_RESULTS: usize = 1;
pub(crate) const WORKSPACE_SEARCH_MAX_RESULTS: usize = 1;
pub(crate) const WORKSPACE_NONMATCHING_COUNT: usize = WORKSPACE_LIST_MAX_RESULTS;
pub(crate) const WORKSPACE_ANSWER_PATH: &str = "answer.txt";
pub(crate) const WORKSPACE_ANSWER: &str = "model loop observed\n";
pub(crate) const WORKSPACE_COLLATERAL_DIRECTORY: &str = "collateral-directory";
pub(crate) const SYNTHETIC_NO_FILE_EDITED_REPORT: &str = "No file was edited; done.";
pub(crate) const WORKSPACE_CASES: &[ForcedCase] = &[
    ForcedCase {
        name: APPLY_PATCH_NAME,
        expected_arguments: r#"{"patch":"*** Begin Patch\n*** Add File: patched.txt\n+patched by eval\n*** End Patch"}"#,
        prompt: "Call apply_patch with exactly {\"patch\":\"*** Begin Patch\\n*** Add File: patched.txt\\n+patched by eval\\n*** End Patch\"}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: EDIT_FILE_NAME,
        expected_arguments: r#"{"path":"brief.txt","old_string":"alpha","new_string":"beta","replace_all":true}"#,
        prompt: "Call edit_file with exactly {\"path\":\"brief.txt\",\"old_string\":\"alpha\",\"new_string\":\"beta\",\"replace_all\":true}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: WRITE_FILE_NAME,
        expected_arguments: r#"{"path":"written.txt","content":"written by eval\n"}"#,
        prompt: "Call write_file with exactly {\"path\":\"written.txt\",\"content\":\"written by eval\\n\"}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: READ_FILE_NAME,
        expected_arguments: r#"{"path":"brief.txt","max_bytes":6}"#,
        prompt: "Call read_file with exactly {\"path\":\"brief.txt\",\"max_bytes\":6}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: LIST_DIRECTORY_NAME,
        expected_arguments: r#"{"path":"nested-list","max_results":20}"#,
        prompt: "Call list_directory with exactly {\"path\":\"nested-list\",\"max_results\":20}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: GLOB_FILES_NAME,
        expected_arguments: r#"{"path":"glob-scope","pattern":"*.txt","max_results":1}"#,
        prompt: "Call glob_files with exactly {\"path\":\"glob-scope\",\"pattern\":\"*.txt\",\"max_results\":1}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: SEARCH_FILES_NAME,
        expected_arguments: r#"{"path":"search-scope","pattern":"beta","max_results":1}"#,
        prompt: "Call search_files with exactly {\"path\":\"search-scope\",\"pattern\":\"beta\",\"max_results\":1}. After its result, answer done without another tool call.",
    },
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceEntrySnapshot {
    Directory {
        mode: Option<u32>,
    },
    File {
        content: Vec<u8>,
        mode: Option<u32>,
        links: Option<u64>,
    },
    Symlink,
    Other,
}

impl FamilySuite {
    pub(crate) fn workspace() -> EvalResult<Self> {
        let workspace = tempfile::tempdir()?;
        fs::write(workspace.path().join(WORKSPACE_SEED_PATH), WORKSPACE_SEED)?;
        fs::create_dir(workspace.path().join(WORKSPACE_GLOB_DIRECTORY))?;
        fs::write(
            workspace.path().join(WORKSPACE_GLOB_PATH),
            WORKSPACE_GLOB_CONTENT,
        )?;
        fs::write(
            workspace.path().join(WORKSPACE_GLOB_OVERFLOW_PATH),
            WORKSPACE_GLOB_CONTENT,
        )?;
        fs::write(
            workspace.path().join(WORKSPACE_GLOB_NONMATCHING_PATH),
            WORKSPACE_GLOB_NONMATCHING_CONTENT,
        )?;
        fs::create_dir(workspace.path().join(WORKSPACE_SEARCH_DIRECTORY))?;
        fs::write(
            workspace.path().join(WORKSPACE_SEARCH_PATH),
            WORKSPACE_SEARCH_CONTENT,
        )?;
        fs::create_dir(workspace.path().join(WORKSPACE_LIST_PATH))?;
        for index in 0..WORKSPACE_LIST_ENTRY_COUNT {
            fs::write(
                workspace.path().join(workspace_list_entry_path(index)),
                "nested list fixture\n",
            )?;
        }
        for index in 0..WORKSPACE_NONMATCHING_COUNT {
            fs::write(
                workspace.path().join(workspace_nonmatching_path(index)),
                "nonmatching fixture\n",
            )?;
        }
        let workspace_seed_entries = workspace_entries(workspace.path())?;
        let workspace_seed_modified_times = workspace_modified_times(workspace.path())?;
        let workspace_seed_entry_identities = workspace_entry_identities(workspace.path())?;
        let workspace_seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
        let workspace_seed_inode_flags = workspace_inode_flags(workspace.path())?;
        let reads = WorkspaceReadTools::try_new(LocalWorkspaceFileSystem, workspace.path())?;
        let mutations =
            WorkspaceMutationTools::try_new(LocalWorkspaceFileSystem, workspace.path())?;
        let (read_catalog, read_executor) = reads.into_parts();
        let (mutation_catalog, mutation_executor) = mutations.into_parts();
        Ok(Self {
            family: EvalFamily::Workspace,
            workspace,
            git_seed: None,
            git_seed_refs: BTreeMap::new(),
            git_seed_fixture: GitFixtureSnapshot::default(),
            catalog: MergedCatalog::try_new([read_catalog, mutation_catalog])?,
            executor: SharedFamilyExecutor::new(FamilyExecutor::Workspace {
                read: read_executor,
                mutation: mutation_executor,
            }),
            workspace_seed_entries,
            workspace_seed_modified_times,
            workspace_seed_entry_identities,
            workspace_seed_extended_attributes,
            workspace_seed_inode_flags,
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
            git_pre_execution_object_entries: Arc::new(StdMutex::new(None)),
            git_pre_execution_object_modified_times: Arc::new(StdMutex::new(None)),
            git_pre_execution_object_entry_identities: Arc::new(StdMutex::new(None)),
        })
    }

    pub(crate) fn workspace_natural_entries_match(&self) -> EvalResult<bool> {
        let answer_path = self.workspace.path().join(WORKSPACE_ANSWER_PATH);
        match fs::read(&answer_path) {
            Ok(bytes) if bytes == WORKSPACE_ANSWER.as_bytes() => {}
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        let mut actual = workspace_entries(self.workspace.path())?;
        let mut actual_modified_times = workspace_modified_times(self.workspace.path())?;
        let answer = actual.remove(Path::new(WORKSPACE_ANSWER_PATH));
        actual_modified_times.remove(Path::new(WORKSPACE_ANSWER_PATH));
        actual_modified_times.remove(Path::new(""));
        let expected_answer = WorkspaceEntrySnapshot::File {
            content: WORKSPACE_ANSWER.as_bytes().to_vec(),
            mode: WORKSPACE_CREATED_FILE_MODE,
            links: WORKSPACE_CREATED_FILE_LINKS,
        };
        let mut expected_modified_times = self.workspace_seed_modified_times.clone();
        expected_modified_times.remove(Path::new(""));
        Ok(answer == Some(expected_answer)
            && actual == self.workspace_seed_entries
            && actual_modified_times == expected_modified_times
            && workspace_mutation_entry_times_match(
                self.workspace.path(),
                Path::new(WORKSPACE_ANSWER_PATH),
                self.executor.filesystem_execution_window(WRITE_FILE_NAME),
            )?
            && workspace_mutation_entry_times_match(
                self.workspace.path(),
                Path::new(""),
                self.executor.filesystem_execution_window(WRITE_FILE_NAME),
            )?
            && workspace_extended_attributes_match_for_mutation(
                self.workspace.path(),
                &self.workspace_seed_extended_attributes,
                Path::new(WORKSPACE_ANSWER_PATH),
            )?
            && workspace_inode_flags_match_for_mutation(
                self.workspace.path(),
                &self.workspace_seed_inode_flags,
                Path::new(WORKSPACE_ANSWER_PATH),
            )?
            && workspace_entry_identities_match_except(
                self.workspace.path(),
                &self.workspace_seed_entry_identities,
                &[Path::new(WORKSPACE_ANSWER_PATH)],
            )?)
    }
}

pub(crate) fn workspace_contains_oversized_regular_file(
    root: &Path,
    maximum_bytes: usize,
) -> EvalResult<bool> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() && entry.metadata()?.len() > maximum_bytes as u64 {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(crate) fn workspace_entries(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, WorkspaceEntrySnapshot>> {
    filesystem_entries(root, None)
}

pub(crate) fn filesystem_entries(
    root: &Path,
    ignored_root_entry: Option<&Path>,
) -> EvalResult<BTreeMap<PathBuf, WorkspaceEntrySnapshot>> {
    let root_metadata = fs::symlink_metadata(root)?;
    let root_file_type = root_metadata.file_type();
    let root_snapshot = if root_file_type.is_dir() {
        WorkspaceEntrySnapshot::Directory {
            mode: worktree_mode(root)?,
        }
    } else if root_file_type.is_file() {
        WorkspaceEntrySnapshot::File {
            content: fs::read(root)?,
            mode: worktree_mode(root)?,
            links: worktree_link_count(root)?,
        }
    } else if root_file_type.is_symlink() {
        WorkspaceEntrySnapshot::Symlink
    } else {
        WorkspaceEntrySnapshot::Other
    };
    let mut pending = if root_file_type.is_dir() {
        vec![root.to_path_buf()]
    } else {
        Vec::new()
    };
    let mut entries = BTreeMap::from([(PathBuf::new(), root_snapshot)]);
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(|_| io::Error::other("workspace fixture escaped its root"))?
                .to_path_buf();
            if ignored_root_entry.is_some_and(|ignored| relative == ignored) {
                continue;
            }
            if file_type.is_dir() {
                pending.push(entry.path());
                entries.insert(
                    relative,
                    WorkspaceEntrySnapshot::Directory {
                        mode: worktree_mode(&entry.path())?,
                    },
                );
            } else if file_type.is_file() {
                let content = fs::read(entry.path())?;
                let mode = worktree_mode(&entry.path())?;
                let links = worktree_link_count(&entry.path())?;
                entries.insert(
                    relative,
                    WorkspaceEntrySnapshot::File {
                        content,
                        mode,
                        links,
                    },
                );
            } else if file_type.is_symlink() {
                entries.insert(relative, WorkspaceEntrySnapshot::Symlink);
            } else {
                entries.insert(relative, WorkspaceEntrySnapshot::Other);
            }
        }
    }
    Ok(entries)
}

pub(crate) fn workspace_modified_times(root: &Path) -> EvalResult<BTreeMap<PathBuf, SystemTime>> {
    filesystem_modified_times(root, None)
}

pub(crate) fn workspace_entry_identities(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, FilesystemIdentity>> {
    filesystem_entry_identities(root, None)
}

pub(crate) fn workspace_extended_attributes(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, ExtendedAttributeSnapshot>> {
    filesystem_extended_attributes(root, None)
}

pub(crate) struct WorkspaceForcedVerification<'a> {
    pub(crate) root: &'a Path,
    pub(crate) seed_entries: &'a BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    pub(crate) seed_modified_times: &'a BTreeMap<PathBuf, SystemTime>,
    pub(crate) seed_entry_identities: &'a BTreeMap<PathBuf, FilesystemIdentity>,
    pub(crate) seed_extended_attributes: &'a BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
    pub(crate) seed_inode_flags: &'a BTreeMap<PathBuf, u32>,
    pub(crate) execution_window: Option<FilesystemExecutionTimeWindow>,
}

pub(crate) fn workspace_forced_case_passed(
    verification: WorkspaceForcedVerification<'_>,
    name: &str,
    arguments: &serde_json::Value,
    result: &serde_json::Value,
) -> EvalResult<bool> {
    let WorkspaceForcedVerification {
        root,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    } = verification;
    let expected_fields: &[&str] = match name {
        APPLY_PATCH_NAME => &["operations_applied", EVAL_RECEIPT_FIELD],
        EDIT_FILE_NAME => &["path", "replacements", "bytes_written", EVAL_RECEIPT_FIELD],
        WRITE_FILE_NAME => &["path", "bytes_written", "created", EVAL_RECEIPT_FIELD],
        READ_FILE_NAME => &[
            "path",
            "content",
            "offset",
            "bytes_read",
            "next_offset",
            "total_bytes",
            "truncated",
            EVAL_RECEIPT_FIELD,
        ],
        LIST_DIRECTORY_NAME => &["entries", "truncated", EVAL_RECEIPT_FIELD],
        GLOB_FILES_NAME | SEARCH_FILES_NAME => &["matches", "truncated", EVAL_RECEIPT_FIELD],
        _ => return Ok(false),
    };
    if !json_object_has_exact_fields(result, expected_fields) {
        return Ok(false);
    }
    let passed = match name {
        APPLY_PATCH_NAME => {
            result["operations_applied"] == 1
                && fs::read_to_string(root.join("patched.txt"))? == "patched by eval\n"
                && fs::read(root.join(WORKSPACE_SEED_PATH))? == WORKSPACE_SEED.as_bytes()
                && workspace_mutation_entries_match(
                    root,
                    seed_entries,
                    Path::new("patched.txt"),
                    b"patched by eval\n",
                )?
        }
        EDIT_FILE_NAME => {
            let Some(path) = arguments["path"].as_str() else {
                return Ok(false);
            };
            let old = arguments["old_string"].as_str().unwrap_or_default();
            let new = arguments["new_string"].as_str().unwrap_or_default();
            let replace_all = arguments["replace_all"].as_bool().unwrap_or_default();
            let replacements = if replace_all {
                WORKSPACE_SEED.match_indices(old).count()
            } else {
                usize::from(WORKSPACE_SEED.contains(old))
            };
            let expected = if replace_all {
                WORKSPACE_SEED.replace(old, new)
            } else {
                WORKSPACE_SEED.replacen(old, new, 1)
            };
            result["path"] == path
                && result["replacements"] == replacements
                && result["bytes_written"] == expected.len()
                && fs::read_to_string(root.join(path))? == expected
                && fs::read(root.join(WORKSPACE_GLOB_PATH))? == WORKSPACE_GLOB_CONTENT.as_bytes()
                && workspace_mutation_entries_match(
                    root,
                    seed_entries,
                    Path::new(path),
                    expected.as_bytes(),
                )?
        }
        WRITE_FILE_NAME => {
            let Some(path) = arguments["path"].as_str() else {
                return Ok(false);
            };
            let Some(expected) = arguments["content"].as_str() else {
                return Ok(false);
            };
            result["path"] == path
                && result["bytes_written"] == expected.len()
                && result["created"] == true
                && fs::read_to_string(root.join(path))? == expected
                && fs::read(root.join(WORKSPACE_SEED_PATH))? == WORKSPACE_SEED.as_bytes()
                && workspace_mutation_entries_match(
                    root,
                    seed_entries,
                    Path::new(path),
                    expected.as_bytes(),
                )?
        }
        READ_FILE_NAME => {
            let Some(path) = arguments["path"].as_str() else {
                return Ok(false);
            };
            let Some(max_bytes) = arguments["max_bytes"]
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
            else {
                return Ok(false);
            };
            let Some(expected) = WORKSPACE_SEED.get(..max_bytes) else {
                return Ok(false);
            };
            result["path"] == path
                && max_bytes == WORKSPACE_FORCED_READ_MAX_BYTES
                && result["content"] == expected
                && result["offset"] == 0
                && result["bytes_read"] == expected.len()
                && result["next_offset"] == expected.len()
                && result["total_bytes"] == WORKSPACE_SEED.len()
                && result["truncated"] == true
                && fs::read(root.join(path))? == WORKSPACE_SEED.as_bytes()
        }
        LIST_DIRECTORY_NAME => {
            result_entries_equal(&result["entries"], &expected_workspace_listing())
                && result["truncated"] == true
        }
        GLOB_FILES_NAME => {
            let expected = [(String::from(WORKSPACE_GLOB_PATH), "file")];
            arguments["max_results"] == WORKSPACE_GLOB_MAX_RESULTS
                && result_entries_equal(&result["matches"], &expected)
                && result["truncated"] == true
        }
        SEARCH_FILES_NAME => {
            let Some(pattern) = arguments["pattern"].as_str() else {
                return Ok(false);
            };
            let Some((line_index, line)) = WORKSPACE_SEARCH_CONTENT
                .lines()
                .enumerate()
                .find(|(_, line)| line.contains(pattern))
            else {
                return Ok(false);
            };
            let Some(column) = line.find(pattern).map(|column| column + 1) else {
                return Ok(false);
            };
            result["matches"].as_array().is_some_and(|matches| {
                matches.as_slice().first().is_some_and(|matched| {
                    matches.len() == 1
                        && json_object_has_exact_fields(
                            matched,
                            &[
                                "path",
                                "line",
                                "column",
                                "text_start_column",
                                "text",
                                "line_truncated",
                            ],
                        )
                        && matched["path"] == WORKSPACE_SEARCH_PATH
                        && matched["line"] == line_index + 1
                        && matched["column"] == column
                        && matched["text_start_column"] == 1
                        && matched["text"] == line
                        && matched["line_truncated"] == false
                })
            }) && arguments["max_results"] == WORKSPACE_SEARCH_MAX_RESULTS
                && result["truncated"] == true
        }
        _ => false,
    };
    if !passed {
        return Ok(false);
    }
    match name {
        READ_FILE_NAME | LIST_DIRECTORY_NAME | GLOB_FILES_NAME | SEARCH_FILES_NAME => {
            Ok(workspace_entries(root)? == *seed_entries
                && workspace_modified_times(root)? == *seed_modified_times
                && workspace_entry_identities(root)? == *seed_entry_identities
                && workspace_extended_attributes(root)? == *seed_extended_attributes
                && workspace_inode_flags(root)? == *seed_inode_flags)
        }
        APPLY_PATCH_NAME => {
            Ok(workspace_modified_times_match_except(
                root,
                seed_modified_times,
                &[Path::new(""), Path::new("patched.txt")],
            )? && workspace_entry_identities_match_except(
                root,
                seed_entry_identities,
                &[Path::new("patched.txt")],
            )? && workspace_extended_attributes_match_for_mutation(
                root,
                seed_extended_attributes,
                Path::new("patched.txt"),
            )? && workspace_inode_flags_match_for_mutation(
                root,
                seed_inode_flags,
                Path::new("patched.txt"),
            )? && workspace_mutation_entry_times_match(
                root,
                Path::new("patched.txt"),
                execution_window,
            )? && workspace_mutation_entry_times_match(root, Path::new(""), execution_window)?)
        }
        EDIT_FILE_NAME => {
            let Some(path) = arguments["path"].as_str() else {
                return Ok(false);
            };
            let path = Path::new(path);
            let Some(parent) = path.parent() else {
                return Ok(false);
            };
            Ok(
                workspace_modified_times_match_except(root, seed_modified_times, &[path, parent])?
                    && workspace_entry_identities_match_except(
                        root,
                        seed_entry_identities,
                        &[path],
                    )?
                    && workspace_extended_attributes_match_for_mutation(
                        root,
                        seed_extended_attributes,
                        path,
                    )?
                    && workspace_inode_flags_match_for_mutation(root, seed_inode_flags, path)?
                    && workspace_mutation_entry_times_match(root, path, execution_window)?
                    && workspace_mutation_entry_times_match(root, parent, execution_window)?,
            )
        }
        WRITE_FILE_NAME => {
            let Some(path) = arguments["path"].as_str() else {
                return Ok(false);
            };
            Ok(workspace_modified_times_match_except(
                root,
                seed_modified_times,
                &[Path::new(""), Path::new(path)],
            )? && workspace_entry_identities_match_except(
                root,
                seed_entry_identities,
                &[Path::new(path)],
            )? && workspace_extended_attributes_match_for_mutation(
                root,
                seed_extended_attributes,
                Path::new(path),
            )? && workspace_inode_flags_match_for_mutation(
                root,
                seed_inode_flags,
                Path::new(path),
            )? && workspace_mutation_entry_times_match(root, Path::new(path), execution_window)?
                && workspace_mutation_entry_times_match(root, Path::new(""), execution_window)?)
        }
        _ => Ok(false),
    }
}

pub(crate) fn workspace_mutation_entry_times_match(
    root: &Path,
    target: &Path,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> EvalResult<bool> {
    let metadata = fs::metadata(root.join(target))?;
    let modified = metadata.modified()?;
    let identity = filesystem_identity(&metadata);
    Ok(execution_window.is_some_and(|window| {
        window.contains_modified(modified)
            && identity.is_some_and(|identity| window.contains_change_time(identity))
    }))
}

pub(crate) fn workspace_modified_times_match_except(
    root: &Path,
    expected: &BTreeMap<PathBuf, SystemTime>,
    allowed_paths: &[&Path],
) -> EvalResult<bool> {
    let mut actual = workspace_modified_times(root)?;
    let mut expected = expected.clone();
    for path in allowed_paths {
        actual.remove(*path);
        expected.remove(*path);
    }
    Ok(actual == expected)
}

pub(crate) fn workspace_entry_identities_match_except(
    root: &Path,
    expected: &BTreeMap<PathBuf, FilesystemIdentity>,
    allowed_paths: &[&Path],
) -> EvalResult<bool> {
    Ok(entry_identities_match_except(
        workspace_entry_identities(root)?,
        expected,
        allowed_paths,
    ))
}

pub(crate) fn workspace_extended_attributes_match_for_mutation(
    root: &Path,
    expected: &BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
    target: &Path,
) -> EvalResult<bool> {
    let creation_attributes = creation_extended_attributes(expected);
    let mut expected = expected.clone();
    expected
        .entry(target.to_path_buf())
        .or_insert(creation_attributes);
    Ok(workspace_extended_attributes(root)? == expected)
}

#[cfg(target_os = "linux")]
pub(crate) fn workspace_inode_flags(root: &Path) -> EvalResult<BTreeMap<PathBuf, u32>> {
    workspace_entries(root)?
        .into_iter()
        .filter_map(|(path, entry)| {
            matches!(
                entry,
                WorkspaceEntrySnapshot::Directory { .. } | WorkspaceEntrySnapshot::File { .. }
            )
            .then_some(path)
        })
        .map(|path| {
            let file = fs::File::open(root.join(&path))?;
            Ok((path, rustix::fs::ioctl_getflags(file)?.bits()))
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn workspace_inode_flags(root: &Path) -> EvalResult<BTreeMap<PathBuf, u32>> {
    let _ = root;
    Ok(BTreeMap::new())
}

pub(crate) fn workspace_inode_flags_match_for_mutation(
    root: &Path,
    expected: &BTreeMap<PathBuf, u32>,
    target: &Path,
) -> EvalResult<bool> {
    workspace_inode_flags_match_for_mutation_with_reference(
        root,
        expected,
        target,
        Path::new(WORKSPACE_SEED_PATH),
    )
}

pub(crate) fn workspace_inode_flags_match_for_mutation_with_reference(
    root: &Path,
    expected: &BTreeMap<PathBuf, u32>,
    target: &Path,
    creation_reference: &Path,
) -> EvalResult<bool> {
    Ok(inode_flag_snapshots_match_for_mutation(
        workspace_inode_flags(root)?,
        expected,
        target,
        creation_reference,
    ))
}

#[cfg(unix)]
pub(crate) fn created_entry_identity_matches_workspace(
    actual: &BTreeMap<PathBuf, FilesystemIdentity>,
    expected: &BTreeMap<PathBuf, FilesystemIdentity>,
    created_path: &Path,
) -> bool {
    let created = actual.get(created_path);
    let workspace = expected.get(Path::new(""));
    created.is_some_and(|created| {
        workspace.is_some_and(|workspace| created.device == workspace.device)
    }) && filesystem_ownership_matches(created, workspace)
}

#[cfg(not(unix))]
pub(crate) fn created_entry_identity_matches_workspace(
    actual: &BTreeMap<PathBuf, FilesystemIdentity>,
    expected: &BTreeMap<PathBuf, FilesystemIdentity>,
    created_path: &Path,
) -> bool {
    filesystem_ownership_matches(actual.get(created_path), expected.get(Path::new("")))
}

pub(crate) fn workspace_mutation_entries_match(
    root: &Path,
    seed_entries: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    target: &Path,
    expected: &[u8],
) -> EvalResult<bool> {
    let actual_entries = workspace_entries(root)?;
    let (mode, links) = match (seed_entries.get(target), actual_entries.get(target)) {
        (Some(WorkspaceEntrySnapshot::File { mode, links, .. }), _) => (*mode, *links),
        (None, Some(WorkspaceEntrySnapshot::File { .. })) => {
            (WORKSPACE_CREATED_FILE_MODE, WORKSPACE_CREATED_FILE_LINKS)
        }
        _ => return Ok(false),
    };
    let mut expected_entries = seed_entries.clone();
    expected_entries.insert(
        target.to_path_buf(),
        WorkspaceEntrySnapshot::File {
            content: expected.to_vec(),
            mode,
            links,
        },
    );
    Ok(actual_entries == expected_entries)
}

pub(crate) fn result_entries_equal(
    value: &serde_json::Value,
    expected: &[(String, &'static str)],
) -> bool {
    value.as_array().is_some_and(|entries| {
        entries.len() == expected.len()
            && entries
                .iter()
                .filter_map(|entry| {
                    if !json_object_has_exact_fields(entry, &["path", "kind"]) {
                        return None;
                    }
                    Some((entry["path"].as_str()?, entry["kind"].as_str()?))
                })
                .eq(expected.iter().map(|(path, kind)| (path.as_str(), *kind)))
    })
}

pub(crate) fn workspace_nonmatching_path(index: usize) -> String {
    format!("zz-extra-{index:02}.bin")
}

pub(crate) fn workspace_listing(entry_count: usize) -> Vec<(String, &'static str)> {
    (0..entry_count)
        .map(|index| (workspace_list_entry_path(index), "file"))
        .collect()
}

pub(crate) fn workspace_list_entry_path(index: usize) -> String {
    format!("{WORKSPACE_LIST_PATH}/entry-{index:02}.txt")
}

pub(crate) fn expected_workspace_listing() -> Vec<(String, &'static str)> {
    workspace_listing(WORKSPACE_LIST_MAX_RESULTS)
}

pub(crate) fn complete_workspace_listing() -> Vec<(String, &'static str)> {
    workspace_listing(WORKSPACE_LIST_ENTRY_COUNT)
}

pub(crate) fn workspace_listing_json(
    entries: Vec<(String, &'static str)>,
) -> Vec<serde_json::Value> {
    entries
        .into_iter()
        .map(|(path, kind)| serde_json::json!({"path": path, "kind": kind}))
        .collect()
}

pub(crate) fn admit_new_filesystem_identity_path_and_ancestors(
    actual: &BTreeMap<PathBuf, FilesystemIdentity>,
    expected: &mut BTreeMap<PathBuf, FilesystemIdentity>,
    path: &Path,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> bool {
    let mut current = Some(path);
    while let Some(candidate) = current {
        let Some(identity) = actual.get(candidate) else {
            return false;
        };
        if let Some(expected_identity) = expected.get_mut(candidate) {
            if !filesystem_ownership_matches(Some(identity), Some(expected_identity))
                || identity.device != expected_identity.device
                || identity.inode != expected_identity.inode
                || (*identity != *expected_identity
                    && !execution_window
                        .is_some_and(|window| window.contains_change_time(*identity)))
            {
                return false;
            }
            admit_filesystem_change_time(expected_identity, *identity);
        } else {
            let Some(root_identity) = expected.get(Path::new("")) else {
                return false;
            };
            if !filesystem_ownership_matches(Some(identity), Some(root_identity))
                || identity.device != root_identity.device
                || !execution_window.is_some_and(|window| window.contains_change_time(*identity))
            {
                return false;
            }
            expected.insert(candidate.to_path_buf(), *identity);
        }
        current = candidate.parent();
    }
    true
}

pub(crate) fn admit_filesystem_change_time(
    expected: &mut FilesystemIdentity,
    actual: FilesystemIdentity,
) {
    expected.change_time_seconds = actual.change_time_seconds;
    expected.change_time_nanoseconds = actual.change_time_nanoseconds;
}

pub(crate) fn filesystem_identity_matches_without_change_time(
    actual: Option<FilesystemIdentity>,
    expected: Option<FilesystemIdentity>,
) -> bool {
    match (actual, expected) {
        (Some(actual), Some(expected)) => {
            actual.device == expected.device
                && actual.inode == expected.inode
                && actual.user_id == expected.user_id
                && actual.group_id == expected.group_id
        }
        (None, None) => true,
        _ => false,
    }
}

pub(crate) fn workspace_read_covers_seed(arguments: &serde_json::Value) -> bool {
    let Ok(arguments) = serde_json::from_value::<ReadFileArguments>(arguments.clone()) else {
        return false;
    };
    arguments.path == WORKSPACE_SEED_PATH
        && arguments.max_bytes >= WORKSPACE_SEED.len()
        && arguments.max_bytes <= MAX_WORKSPACE_READ_BYTES
}

pub(crate) fn workspace_mutation_could_alter_seed(request: &RequestSnapshot) -> bool {
    let Some(arguments) = request.arguments() else {
        return false;
    };
    match request.name.as_str() {
        WRITE_FILE_NAME => serde_json::from_value::<WriteFileArguments>(arguments)
            .is_ok_and(|arguments| arguments.path == WORKSPACE_SEED_PATH),
        EDIT_FILE_NAME => serde_json::from_value::<EditFileArguments>(arguments)
            .is_ok_and(|arguments| arguments.path == WORKSPACE_SEED_PATH),
        APPLY_PATCH_NAME => serde_json::from_value::<ApplyPatchArguments>(arguments)
            .ok()
            .and_then(|arguments| WorkspacePatch::parse(&arguments.patch).ok())
            .is_some_and(|patch| {
                patch
                    .operations()
                    .iter()
                    .any(|operation| operation.path() == WORKSPACE_SEED_PATH)
            }),
        _ => false,
    }
}

pub(crate) fn workspace_natural_read_result_passed(
    snapshot: &CaseSnapshot,
    tracker: &OperationTracker,
) -> bool {
    let Some(request) = snapshot.requests.iter().find(|request| {
        request.name == READ_FILE_NAME
            && request.attempt_succeeded
            && request
                .arguments()
                .is_some_and(|arguments| workspace_read_covers_seed(&arguments))
    }) else {
        return false;
    };
    let Some(content) = tracker.result_content(request.request_id) else {
        return false;
    };
    let Ok(result) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    json_object_has_exact_fields(
        &result,
        &[
            "path",
            "content",
            "offset",
            "bytes_read",
            "next_offset",
            "total_bytes",
            "truncated",
            EVAL_RECEIPT_FIELD,
        ],
    ) && result["path"] == WORKSPACE_SEED_PATH
        && result["content"] == WORKSPACE_SEED
        && result["offset"] == 0
        && result["bytes_read"] == WORKSPACE_SEED.len()
        && result["next_offset"] == WORKSPACE_SEED.len()
        && result["total_bytes"] == WORKSPACE_SEED.len()
        && result["truncated"] == false
}

pub(crate) fn workspace_natural_write_result_passed(
    snapshot: &CaseSnapshot,
    tracker: &OperationTracker,
) -> bool {
    let Some(request) = snapshot.requests.iter().find(|request| {
        request.name == WRITE_FILE_NAME
            && request.attempt_succeeded
            && request.arguments().is_some_and(|arguments| {
                arguments
                    == serde_json::json!({
                        "path": WORKSPACE_ANSWER_PATH,
                        "content": WORKSPACE_ANSWER,
                    })
            })
    }) else {
        return false;
    };
    let Some(content) = tracker.result_content(request.request_id) else {
        return false;
    };
    let Ok(result) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    json_object_has_exact_fields(
        &result,
        &["path", "bytes_written", "created", EVAL_RECEIPT_FIELD],
    ) && result["path"] == WORKSPACE_ANSWER_PATH
        && result["bytes_written"] == WORKSPACE_ANSWER.len()
        && result["created"] == true
}

pub(crate) fn workspace_natural_result_payloads_passed(
    snapshot: &CaseSnapshot,
    tracker: &OperationTracker,
) -> bool {
    workspace_natural_read_result_passed(snapshot, tracker)
        && workspace_natural_write_result_passed(snapshot, tracker)
}

pub(crate) fn round_tripped_fixture_result(request_id: Uuid) -> TrackedToolResult {
    TrackedToolResult {
        request_id,
        content: String::from("fixture result"),
        is_error: false,
        round_tripped: true,
    }
}

pub(crate) fn bounded_workspace_read_arguments() -> serde_json::Value {
    serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "max_bytes": WORKSPACE_SEED.len(),
    })
}
