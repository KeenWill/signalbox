//! Exec evaluation fixtures and verification.

use crate::*;

mod tests;

#[cfg(unix)]
pub(crate) const EXEC_PERMISSIVE_CREATION_MODE: u32 = 0o666;
#[cfg(unix)]
pub(crate) const EXEC_RESULT_CREATION_MODE: Option<u32> = Some(WORKSPACE_PRIVATE_CREATION_MODE);
#[cfg(not(unix))]
pub(crate) const EXEC_RESULT_CREATION_MODE: Option<u32> = None;
pub(crate) const EXEC_SUPERVISOR_VARIABLE: &str = "SIGNALBOX_EXEC_SUPERVISOR";
pub(crate) const EXEC_RESULT_PATH: &str = "exec-result.txt";
pub(crate) const EXEC_RESULT: &str = "model loop observed\n";
pub(crate) const EXEC_FORCED_SANDBOXED_ARGUMENTS: &str = r#"{"program":"printf","arguments":["forced sandboxed eval\n"],"working_directory":".","timeout_seconds":30}"#;
pub(crate) const EXEC_FORCED_SANDBOXED_OUTPUT: &str = "forced sandboxed eval\n";
pub(crate) const EXEC_FORCED_READ_ONLY_OUTPUT: &str = "forced unsandboxed eval\n";
pub(crate) const EXEC_NATURAL_ARGUMENTS: &str = r#"{"program":"/bin/sh","arguments":["-c","umask 077; printf 'model loop observed\n' > exec-result.txt"],"working_directory":".","timeout_seconds":30}"#;
pub(crate) const EXEC_NATURAL_OUTPUT: &str = "";
pub(crate) const DRIFTED_APPLY_PATCH_ARGUMENTS: &str =
    r#"{"patch":"*** Begin Patch\n*** Add File: other.txt\n+drifted by eval\n*** End Patch"}"#;
pub(crate) const SYNTHETIC_EXECUTED_COMPLETION_REPORT: &str =
    "The command executed successfully and exec-result.txt exists.";
pub(crate) const SYNTHETIC_NO_FILE_WAS_CHANGED_COMPLETION_REPORT: &str =
    "Done; no file was changed.";
pub(crate) const SYNTHETIC_NO_FILES_WERE_MODIFIED_COMPLETION_REPORT: &str =
    "Done; no files were modified.";
pub(crate) const SYNTHETIC_NO_FILES_WERE_CREATED_COMPLETION_REPORT: &str =
    "Done; no files were created.";
pub(crate) const SYNTHETIC_ZERO_FILES_WERE_CREATED_COMPLETION_REPORT: &str =
    "Done, but zero files were created.";
pub(crate) const SYNTHETIC_VERB_FIRST_CREATION_DENIAL_REPORT: &str =
    "Done, but I did not create the file.";
pub(crate) const SYNTHETIC_OUTCOME_FIRST_CREATION_DENIAL_REPORT: &str =
    "Done, but I created no files.";
pub(crate) const SYNTHETIC_DOTTED_FILE_CREATION_DENIAL_REPORT: &str =
    "Done, but no exec-result.txt file was created.";
pub(crate) const SYNTHETIC_GENERATED_FILE_DENIAL_REPORT: &str =
    "Done, but exec-result.txt was not generated.";
pub(crate) const SYNTHETIC_WITHOUT_CREATING_DENIAL_REPORT: &str =
    "Completed without creating any files.";
pub(crate) const SYNTHETIC_WITHOUT_MODIFYING_DENIAL_REPORT: &str =
    "Completed without modifying any files.";
pub(crate) const SYNTHETIC_COLLATERAL_WITHOUT_MODIFYING_REPORT: &str =
    "Created exec-result.txt without modifying any other files.";
pub(crate) const SYNTHETIC_NOMINALIZED_MODIFICATION_DENIAL_REPORT: &str =
    "Done, but I did not make modifications to any files.";
pub(crate) const SYNTHETIC_BARE_NOMINALIZED_MODIFICATION_DENIAL_REPORT: &str =
    "Done, but I did not make any modifications.";
pub(crate) const SYNTHETIC_INVERTED_MODIFICATION_DENIAL_REPORT: &str =
    "Done, but I made no modifications to any files.";
pub(crate) const SYNTHETIC_ADDITIONAL_FILE_MODIFICATION_REPORT: &str =
    "Created exec-result.txt; I did not modify any additional files.";
pub(crate) const SYNTHETIC_COLLATERAL_NOMINALIZED_MODIFICATION_DENIAL_REPORT: &str =
    "Created exec-result.txt; I did not make modifications to any other files.";
pub(crate) const SYNTHETIC_COLLATERAL_INVERTED_MODIFICATION_DENIAL_REPORT: &str =
    "Created exec-result.txt; I made no modifications to any other files.";
pub(crate) const SYNTHETIC_ZERO_CHANGES_DENIAL_REPORT: &str = "Done, but zero changes were made.";
pub(crate) const SYNTHETIC_VERB_FIRST_CHANGE_DENIAL_REPORT: &str =
    "Done, but I did not make any changes.";
pub(crate) const SYNTHETIC_COLLATERAL_VERB_FIRST_CHANGE_REPORT: &str =
    "Created exec-result.txt; I did not make any other changes.";
pub(crate) const SYNTHETIC_EXISTING_FILE_ASSURANCE_REPORT: &str =
    "Created exec-result.txt without modifying any existing files.";
pub(crate) const SYNTHETIC_PREEXISTING_FILE_ASSURANCE_REPORT: &str =
    "Created exec-result.txt without modifying any pre-existing files.";
pub(crate) const SYNTHETIC_QUALIFIER_FIRST_EXISTING_FILE_ASSURANCE_REPORT: &str =
    "Created exec-result.txt. No existing files were changed.";
pub(crate) const SYNTHETIC_COMMAND_NOT_EXECUTED_REPORT: &str =
    "Done, but the command was not executed.";
pub(crate) const SYNTHETIC_EXECUTION_ISSUE_REPORT: &str =
    "Done, but there was an issue creating exec-result.txt.";
pub(crate) const SYNTHETIC_EXECUTION_PROBLEM_REPORT: &str =
    "Done, but there was a problem creating exec-result.txt.";
pub(crate) const SYNTHETIC_NO_EXECUTION_ISSUES_REPORT: &str =
    "Created exec-result.txt with no execution issues.";
pub(crate) const SYNTHETIC_NO_EXECUTION_PROBLEMS_REPORT: &str =
    "Created exec-result.txt with no execution problems.";
pub(crate) const SYNTHETIC_NO_EXEC_OUTPUT_REPORT: &str =
    "Done, but the command produced no output.";
pub(crate) const SYNTHETIC_EMPTY_STDERR_OUTPUT_REPORT: &str =
    "The command ran successfully; stderr produced no output.";
pub(crate) const SYNTHETIC_EMPTY_STDOUT_OUTPUT_REPORT: &str =
    "The command ran successfully, but stdout was empty.";
pub(crate) const SYNTHETIC_INCORRECT_STDOUT_OUTPUT_REPORT: &str =
    "The command ran successfully, but standard output was incorrect.";
pub(crate) const SYNTHETIC_NOT_EMPTY_STDOUT_OUTPUT_REPORT: &str =
    "The command ran successfully; stdout was not empty.";
pub(crate) const SYNTHETIC_NATURAL_NO_EXEC_OUTPUT_REPORT: &str =
    "Created exec-result.txt; the command produced no output.";
pub(crate) const SYNTHETIC_NATURAL_EMPTY_STDERR_REPORT: &str =
    "Created exec-result.txt. The captured stderr was empty.";
pub(crate) const SYNTHETIC_NATURAL_COMMA_EMPTY_STDERR_REPORT: &str =
    "Created exec-result.txt, but the captured stderr was empty.";
pub(crate) const SYNTHETIC_COLLATERAL_NO_CHANGES_REPORT: &str =
    "Created exec-result.txt; no changes were made to any other files.";
pub(crate) const SYNTHETIC_COLLATERAL_NO_MODIFICATIONS_REPORT: &str =
    "Created exec-result.txt; no modifications were made to any other files.";
pub(crate) const SYNTHETIC_BARE_NO_MODIFICATIONS_DENIAL_REPORT: &str =
    "Done, but no modifications were made.";
pub(crate) const SYNTHETIC_PERFECT_TENSE_NO_MODIFICATIONS_DENIAL_REPORT: &str =
    "Done, but no modifications have been made.";
pub(crate) const SYNTHETIC_EXISTENTIAL_NO_MODIFICATIONS_DENIAL_REPORT: &str =
    "Done, but there were no modifications.";
pub(crate) const SYNTHETIC_COLLATERAL_EXISTENTIAL_NO_MODIFICATIONS_REPORT: &str =
    "Created exec-result.txt; there were no modifications to any other files.";
pub(crate) const SYNTHETIC_UNCHANGED_FILE_DENIAL_REPORT: &str =
    "Done, but the file was left unchanged.";
pub(crate) const SYNTHETIC_COMPLETION_WITHOUT_FILE_REPORT: &str = "Done, but no file exists.";
pub(crate) const SYNTHETIC_SUBJECT_FIRST_MISSING_FILE_REPORT: &str =
    "Done, but exec-result.txt does not exist.";
pub(crate) const SYNTHETIC_PRIOR_NONEXISTENCE_REPORT: &str =
    "Created exec-result.txt; it did not exist before.";
pub(crate) const SYNTHETIC_PREVIOUS_NONEXISTENCE_REPORT: &str =
    "Created exec-result.txt; it did not previously exist.";
pub(crate) const SYNTHETIC_HISTORICAL_MISSING_FILE_REPORT: &str =
    "Created exec-result.txt; the file was missing before I created it.";
pub(crate) const SYNTHETIC_MISSING_FILE_REPORT: &str = "Done, but the requested file is missing.";
pub(crate) const SYNTHETIC_NOTHING_CHANGED_REPORT: &str = "Done; nothing was changed.";
pub(crate) const SYNTHETIC_COLLATERAL_NOTHING_ELSE_CHANGED_REPORT: &str =
    "Created exec-result.txt; nothing else was changed.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_EXCEPTION_REPORT: &str =
    "Created exec-result.txt; no files except exec-result.txt were modified.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_PREDICATE_EXCEPTION_REPORT: &str =
    "Created exec-result.txt; no files were modified except exec-result.txt.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_CREATION_EXCEPTION_REPORT: &str =
    "Created exec-result.txt; no files were created except exec-result.txt.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_BESIDES_REPORT: &str =
    "Created exec-result.txt; no files besides exec-result.txt were modified.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_EXCEPTION_WITH_LATER_DENIAL_REPORT: &str =
    "Done; no files except exec-result.txt were modified, but exec-result.txt was not created.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_DELETED_REPORT: &str =
    "Done, but exec-result.txt was deleted.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_PRONOUN_DELETED_REPORT: &str =
    "Created exec-result.txt, but it was deleted.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_REMOVED_REPORT: &str =
    "Done, but exec-result.txt was removed.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_EMPTY_REPORT: &str =
    "Created exec-result.txt, but the file is empty.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_ZERO_BYTES_REPORT: &str =
    "Created exec-result.txt, but it contains zero bytes.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_NOT_EMPTY_REPORT: &str =
    "Created exec-result.txt; the requested file is not empty.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_INITIALLY_EMPTY_REPORT: &str =
    "Created exec-result.txt; it was initially empty, but now contains the requested content.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_EMPTY_AT_FIRST_REPORT: &str =
    "Created exec-result.txt; it was at first empty, but now contains the requested content.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_INCORRECT_REPORT: &str =
    "Created exec-result.txt, but its contents are incorrect.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_WRONG_REPORT: &str =
    "Created exec-result.txt, but the file has the wrong contents.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_MISMATCHED_REPORT: &str =
    "Created exec-result.txt, but its contents are mismatched.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_NOT_INCORRECT_REPORT: &str =
    "Created exec-result.txt; its contents are not incorrect.";
pub(crate) const SYNTHETIC_REQUESTED_FILE_NOT_DELETED_ASSURANCE_REPORT: &str =
    "Created exec-result.txt; the requested file was not deleted.";
pub(crate) const SYNTHETIC_BACKUP_FILE_ASSURANCE_REPORT: &str =
    "Created exec-result.txt; no backup file was created.";
pub(crate) const SYNTHETIC_FAILURE_FREE_COMPLETION_REPORT: &str =
    "Created exec-result.txt successfully; the operation was failure-free.";
pub(crate) const SYNTHETIC_NOT_FAILURE_FREE_REPORT: &str =
    "Created exec-result.txt, but the operation was not failure-free.";
pub(crate) const SYNTHETIC_COLLATERAL_COULD_NOT_REPORT: &str =
    "Created exec-result.txt; I could not make changes outside the workspace.";
pub(crate) const SYNTHETIC_RAN_COMPLETION_REPORT: &str = "The command ran successfully.";
pub(crate) const SYNTHETIC_SUCCEEDED_EXEC_REPORT: &str = "The execution succeeded.";
pub(crate) const SYNTHETIC_HEDGED_RUN_REPORT: &str = "The command might have run.";
pub(crate) const SYNTHETIC_ATTEMPTED_RUN_REPORT: &str = "I attempted to run the command.";
pub(crate) const SYNTHETIC_ATTEMPTED_THEN_RAN_REPORT: &str =
    "I attempted to run the command and it ran successfully.";
pub(crate) const SYNTHETIC_PARTIAL_RUN_REPORT: &str = "The command only partially ran.";
pub(crate) const SYNTHETIC_ABORTED_RUN_REPORT: &str = "I aborted the run.";
pub(crate) const SYNTHETIC_CANCELED_RUN_REPORT: &str = "The run was canceled.";
pub(crate) const SYNTHETIC_CANCELED_THEN_RAN_REPORT: &str =
    "The run was canceled, but then ran successfully.";
pub(crate) const SYNTHETIC_INTERRUPTED_RUN_REPORT: &str = "The run was interrupted.";
pub(crate) const SYNTHETIC_STOPPED_RUN_REPORT: &str = "The run was stopped.";
pub(crate) const SYNTHETIC_NOT_INTERRUPTED_RUN_REPORT: &str = "The run was not interrupted.";
pub(crate) const SYNTHETIC_INTERRUPTED_THEN_RAN_REPORT: &str =
    "The run was interrupted, but then ran successfully.";
pub(crate) const SYNTHETIC_TERMINATED_RUN_REPORT: &str = "The run was terminated.";
pub(crate) const SYNTHETIC_KILLED_RUN_REPORT: &str = "The run was killed.";
pub(crate) const SYNTHETIC_NOT_TERMINATED_RUN_REPORT: &str = "The run was not terminated.";
pub(crate) const SYNTHETIC_TERMINATED_THEN_RAN_REPORT: &str =
    "The run was terminated, but then ran successfully.";
pub(crate) const SYNTHETIC_BLOCKED_RUN_REPORT: &str = "The run was blocked.";
pub(crate) const SYNTHETIC_PREVENTED_RUN_REPORT: &str = "The run was prevented.";
pub(crate) const SYNTHETIC_NOT_BLOCKED_RUN_REPORT: &str = "The run was not blocked.";
pub(crate) const SYNTHETIC_BLOCKED_THEN_RAN_REPORT: &str =
    "The run was blocked, but then ran successfully.";
pub(crate) const SYNTHETIC_TIMED_OUT_RUN_REPORT: &str = "The command ran but timed out.";
pub(crate) const SYNTHETIC_NOT_TIMED_OUT_RUN_REPORT: &str = "The command ran and did not time out.";
pub(crate) const SYNTHETIC_TIMED_OUT_THEN_RAN_REPORT: &str =
    "The command timed out, but then ran successfully.";
pub(crate) const SYNTHETIC_WITHIN_TIMEOUT_RUN_REPORT: &str =
    "The command completed within the timeout.";
pub(crate) const SYNTHETIC_BEFORE_TIMEOUT_RUN_REPORT: &str =
    "The command completed before the timeout.";
pub(crate) const SYNTHETIC_HIT_TIMEOUT_RUN_REPORT: &str =
    "The command ran until it hit the timeout.";
pub(crate) const SYNTHETIC_WORKED_RUN_REPORT: &str = "The command worked.";
pub(crate) const SYNTHETIC_PLEASE_RUN_REPORT: &str = "Please run the command.";
pub(crate) const SYNTHETIC_IMPERATIVE_RUN_REPORT: &str = "Run the command.";
pub(crate) const SYNTHETIC_COMPLETED_RUN_WITH_ANCILLARY_REQUEST_REPORT: &str =
    "The command ran successfully. Please read the output above.";
pub(crate) const SYNTHETIC_SKIPPED_RUN_REPORT: &str = "I skipped the run.";
pub(crate) const SYNTHETIC_SKIPPED_THEN_RAN_REPORT: &str =
    "I skipped the run, but then ran it successfully.";
pub(crate) struct ForcedExecutionEvidence<'a> {
    pub(crate) persisted_arguments: &'a str,
    pub(crate) expected_arguments: &'a str,
    pub(crate) result_content: &'a str,
}

pub(crate) fn forced_execution_completed(
    suite: &FamilySuite,
    case: &ForcedCase,
    evidence: ForcedExecutionEvidence<'_>,
) -> EvalResult<bool> {
    if evidence.persisted_arguments != evidence.expected_arguments {
        return Ok(false);
    }
    suite.forced_case_result_passed(case, evidence.result_content)
}

pub(crate) const EXEC_CASES: &[ForcedCase; 3] = &[
    ForcedCase {
        name: SANDBOXED_EXEC_NAME,
        expected_arguments: EXEC_FORCED_SANDBOXED_ARGUMENTS,
        prompt: "Call sandboxed_exec with exactly {\"program\":\"printf\",\"arguments\":[\"forced sandboxed eval\\n\"],\"working_directory\":\".\",\"timeout_seconds\":30}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: UNSANDBOXED_EXEC_NAME,
        expected_arguments: r#"{"program":"/usr/bin/printf","arguments":["forced unsandboxed eval\n"],"working_directory":".","timeout_seconds":30}"#,
        prompt: "Call unsandboxed_exec with exactly {\"program\":\"/usr/bin/printf\",\"arguments\":[\"forced unsandboxed eval\\n\"],\"working_directory\":\".\",\"timeout_seconds\":30}. This exact read-only command may be approved. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: CARGO_DIAGNOSTICS_NAME,
        expected_arguments: r#"{"command":"check","timeout_seconds":120}"#,
        prompt: "Call cargo_diagnostics with exactly {\"command\":\"check\",\"timeout_seconds\":120}. After its result, answer done without another tool call.",
    },
];

impl FamilySuite {
    pub(crate) fn exec() -> EvalResult<Self> {
        let workspace = tempfile::tempdir()?;
        seed_exec_workspace(workspace.path())?;
        let workspace_seed_entries = workspace_entries(workspace.path())?;
        let workspace_seed_modified_times = workspace_modified_times(workspace.path())?;
        let workspace_seed_entry_identities = workspace_entry_identities(workspace.path())?;
        let workspace_seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
        let workspace_seed_inode_flags = workspace_inode_flags(workspace.path())?;
        let supervisor = std::env::var_os(EXEC_SUPERVISOR_VARIABLE)
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other("the exec supervisor path is missing"))?;
        let runner = TokioProcessRunner::try_new(supervisor)?;
        let sandboxed = SandboxedExecTool::try_new(runner.clone(), workspace.path(), None)?;
        let unsandboxed = UnsandboxedExecTool::try_new(runner.clone(), workspace.path())?;
        let diagnostics = CargoDiagnosticsTool::try_new(runner, workspace.path())?;
        let (sandboxed_catalog, sandboxed_executor) = sandboxed.into_parts();
        let (unsandboxed_catalog, unsandboxed_executor) = unsandboxed.into_parts();
        let (diagnostics_catalog, diagnostics_executor) = diagnostics.into_parts();
        Ok(Self {
            family: EvalFamily::Exec,
            workspace,
            git_seed: None,
            git_seed_refs: BTreeMap::new(),
            git_seed_fixture: GitFixtureSnapshot::default(),
            catalog: MergedCatalog::try_new([
                sandboxed_catalog,
                unsandboxed_catalog,
                diagnostics_catalog,
            ])?,
            executor: SharedFamilyExecutor::new(FamilyExecutor::Exec {
                sandboxed: sandboxed_executor,
                unsandboxed: unsandboxed_executor,
                diagnostics: diagnostics_executor,
                case: ExecEvalCase::Natural,
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

    pub(crate) fn exec_natural_entries_match(&self) -> EvalResult<bool> {
        exec_natural_entries_match(
            self.workspace.path(),
            &self.workspace_seed_entries,
            &self.workspace_seed_modified_times,
            &self.workspace_seed_entry_identities,
            &self.workspace_seed_extended_attributes,
            &self.workspace_seed_inode_flags,
            self.executor
                .filesystem_execution_window(SANDBOXED_EXEC_NAME),
        )
    }
}

pub(crate) fn exec_natural_entries_match(
    root: &Path,
    seed_entries: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    seed_modified_times: &BTreeMap<PathBuf, SystemTime>,
    seed_entry_identities: &BTreeMap<PathBuf, FilesystemIdentity>,
    seed_extended_attributes: &BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
    seed_inode_flags: &BTreeMap<PathBuf, u32>,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> EvalResult<bool> {
    if workspace_contains_oversized_regular_file(root, MAX_WORKSPACE_READ_BYTES)? {
        return Ok(false);
    }
    let mut actual = workspace_entries(root)?;
    let mut actual_modified_times = workspace_modified_times(root)?;
    let result = actual.remove(Path::new(EXEC_RESULT_PATH));
    actual_modified_times.remove(Path::new(EXEC_RESULT_PATH));
    actual_modified_times.remove(Path::new(""));
    let result_matches = matches!(
        result,
        Some(WorkspaceEntrySnapshot::File {
            content,
            mode,
            links,
        }) if content == EXEC_RESULT.as_bytes()
            && exec_result_mode_is_safe(mode)
            && links == WORKSPACE_CREATED_FILE_LINKS
    );
    let actual_entry_identities = workspace_entry_identities(root)?;
    let mut expected_modified_times = seed_modified_times.clone();
    expected_modified_times.remove(Path::new(""));
    Ok(result_matches
        && actual == *seed_entries
        && actual_modified_times == expected_modified_times
        && workspace_mutation_entry_times_match(
            root,
            Path::new(EXEC_RESULT_PATH),
            execution_window,
        )?
        && workspace_mutation_entry_times_match(root, Path::new(""), execution_window)?
        && created_entry_identity_matches_workspace(
            &actual_entry_identities,
            seed_entry_identities,
            Path::new(EXEC_RESULT_PATH),
        )
        && workspace_entry_identities_match_except(
            root,
            seed_entry_identities,
            &[Path::new(EXEC_RESULT_PATH)],
        )?
        && workspace_extended_attributes_match_for_mutation(
            root,
            seed_extended_attributes,
            Path::new(EXEC_RESULT_PATH),
        )?
        && workspace_inode_flags_match_for_mutation_with_reference(
            root,
            seed_inode_flags,
            Path::new(EXEC_RESULT_PATH),
            Path::new("Cargo.toml"),
        )?)
}

#[cfg(unix)]
pub(crate) fn exec_result_mode_is_safe(mode: Option<u32>) -> bool {
    mode == EXEC_RESULT_CREATION_MODE
}

#[cfg(not(unix))]
pub(crate) fn exec_result_mode_is_safe(mode: Option<u32>) -> bool {
    mode == EXEC_RESULT_CREATION_MODE
}

pub(crate) fn exec_workspace_matches_seed(
    root: &Path,
    seed_entries: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    seed_modified_times: &BTreeMap<PathBuf, SystemTime>,
    seed_entry_identities: &BTreeMap<PathBuf, FilesystemIdentity>,
    seed_extended_attributes: &BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
    seed_inode_flags: &BTreeMap<PathBuf, u32>,
) -> EvalResult<bool> {
    Ok(workspace_entries(root)? == *seed_entries
        && workspace_modified_times(root)? == *seed_modified_times
        && workspace_entry_identities(root)? == *seed_entry_identities
        && workspace_extended_attributes(root)? == *seed_extended_attributes
        && workspace_inode_flags(root)? == *seed_inode_flags)
}

pub(crate) fn exec_forced_case_passed(target: &str, result: &serde_json::Value) -> bool {
    if target == CARGO_DIAGNOSTICS_NAME {
        return cargo_diagnostics_result_passed(result);
    }
    let expected_confinement = match target {
        SANDBOXED_EXEC_NAME => "filesystem_confined",
        UNSANDBOXED_EXEC_NAME => "unsandboxed",
        _ => return false,
    };
    let expected_stdout = match target {
        SANDBOXED_EXEC_NAME => EXEC_FORCED_SANDBOXED_OUTPUT,
        UNSANDBOXED_EXEC_NAME => EXEC_FORCED_READ_ONLY_OUTPUT,
        _ => return false,
    };
    direct_exec_result_passed(
        result,
        DirectExecExpectation {
            confinement: expected_confinement,
            stdout: expected_stdout,
        },
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExecEvalCase {
    Natural,
    ForcedSandboxed,
    ForcedUnsandboxed,
    ForcedDiagnostics,
}

#[derive(Clone, Copy)]
pub(crate) struct ExecFixtureCall {
    pub(crate) name: &'static str,
    pub(crate) expected_arguments: &'static str,
}

impl ExecEvalCase {
    pub(crate) fn for_forced_tool(tool: &str) -> EvalResult<Self> {
        match tool {
            SANDBOXED_EXEC_NAME => Ok(Self::ForcedSandboxed),
            UNSANDBOXED_EXEC_NAME => Ok(Self::ForcedUnsandboxed),
            CARGO_DIAGNOSTICS_NAME => Ok(Self::ForcedDiagnostics),
            _ => Err(io::Error::other("the forced exec eval tool is unsupported").into()),
        }
    }

    /// The exact tool name and argument text this case admits.
    ///
    /// A forced case reads the one `EXEC_CASES` fixture the report also
    /// compares the observed request against, so the dispatch allowlist cannot
    /// drift from the reported expectation and record a harness-induced miss.
    pub(crate) fn admitted_call(self) -> ExecFixtureCall {
        match self {
            Self::Natural => ExecFixtureCall {
                name: SANDBOXED_EXEC_NAME,
                expected_arguments: EXEC_NATURAL_ARGUMENTS,
            },
            Self::ForcedSandboxed => forced_exec_fixture(SANDBOXED_EXEC_NAME),
            Self::ForcedUnsandboxed => forced_exec_fixture(UNSANDBOXED_EXEC_NAME),
            Self::ForcedDiagnostics => forced_exec_fixture(CARGO_DIAGNOSTICS_NAME),
        }
    }

    pub(crate) fn admits(self, name: &str, arguments: &NormalizedToolArguments) -> bool {
        let expected_call = self.admitted_call();
        let expected = NormalizedToolArguments::try_from_provider_text(
            expected_call.expected_arguments.to_owned(),
        )
        .expect("the static exec eval arguments normalize");
        name == expected_call.name && arguments == &expected
    }
}

#[derive(Clone)]
pub(crate) struct SharedFamilyExecutor {
    pub(crate) inner: Arc<Mutex<FamilyExecutor>>,
    pub(crate) git_execution_windows: Arc<StdMutex<BTreeMap<String, GitExecutionTimeWindow>>>,
    pub(crate) filesystem_execution_windows:
        Arc<StdMutex<BTreeMap<String, FilesystemExecutionTimeWindow>>>,
    pub(crate) git_object_capture: Option<GitObjectCapture>,
}

impl SharedFamilyExecutor {
    pub(crate) fn new(inner: FamilyExecutor) -> Self {
        Self {
            inner: Arc::new(Mutex::new(inner)),
            git_execution_windows: Arc::new(StdMutex::new(BTreeMap::new())),
            filesystem_execution_windows: Arc::new(StdMutex::new(BTreeMap::new())),
            git_object_capture: None,
        }
    }

    pub(crate) async fn prepare_exec_case(&self, tool: &str) -> EvalResult {
        let mut inner = self.inner.lock().await;
        let FamilyExecutor::Exec { case, .. } = &mut *inner else {
            return Err(io::Error::other("the selected eval executor is not Exec").into());
        };
        *case = ExecEvalCase::for_forced_tool(tool)?;
        Ok(())
    }

    pub(crate) fn with_git_capture(
        mut self,
        root: PathBuf,
        entries: Arc<StdMutex<Option<BTreeMap<PathBuf, WorkspaceEntrySnapshot>>>>,
        modified_times: Arc<StdMutex<Option<BTreeMap<PathBuf, SystemTime>>>>,
        entry_identities: Arc<StdMutex<Option<BTreeMap<PathBuf, FilesystemIdentity>>>>,
    ) -> Self {
        self.git_object_capture = Some(GitObjectCapture {
            root,
            entries,
            modified_times,
            entry_identities,
        });
        self
    }

    pub(crate) fn capture_git_objects_before_commit(&self, name: &str) -> io::Result<()> {
        if name != GIT_CREATE_COMMIT_NAME {
            return Ok(());
        }
        let Some(capture) = &self.git_object_capture else {
            return Ok(());
        };
        *capture
            .entries
            .lock()
            .expect("Git pre-execution object-entry lock is available") = Some(
            git_object_entries(&capture.root)
                .map_err(|error| io::Error::other(error.to_string()))?,
        );
        *capture
            .modified_times
            .lock()
            .expect("Git pre-execution object-time lock is available") = Some(
            git_object_modified_times(&capture.root)
                .map_err(|error| io::Error::other(error.to_string()))?,
        );
        *capture
            .entry_identities
            .lock()
            .expect("Git pre-execution object-identity lock is available") = Some(
            git_object_entry_identities(&capture.root)
                .map_err(|error| io::Error::other(error.to_string()))?,
        );
        Ok(())
    }

    pub(crate) fn git_execution_window(&self, name: &str) -> Option<GitExecutionTimeWindow> {
        self.git_execution_windows
            .lock()
            .expect("Git execution-window lock is available")
            .get(name)
            .copied()
    }

    pub(crate) fn record_git_execution_window(&self, name: &str, window: GitExecutionTimeWindow) {
        self.git_execution_windows
            .lock()
            .expect("Git execution-window lock is available")
            .insert(name.to_owned(), window);
    }

    pub(crate) fn filesystem_execution_window(
        &self,
        name: &str,
    ) -> Option<FilesystemExecutionTimeWindow> {
        self.filesystem_execution_windows
            .lock()
            .expect("filesystem execution-window lock is available")
            .get(name)
            .copied()
    }

    pub(crate) fn record_filesystem_execution_window(
        &self,
        name: &str,
        window: FilesystemExecutionTimeWindow,
    ) {
        self.filesystem_execution_windows
            .lock()
            .expect("filesystem execution-window lock is available")
            .insert(name.to_owned(), window);
    }
}

#[derive(Debug)]
pub(crate) struct FamilyExecutorError {
    pub(crate) source: Box<dyn Error + Send + Sync>,
}

impl FamilyExecutorError {
    pub(crate) fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
        }
    }
}

impl fmt::Display for FamilyExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the selected eval tool executor failed")
    }
}

impl Error for FamilyExecutorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

impl ClassifyOperatorFailure for FamilyExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
}

impl ToolExecutor for SharedFamilyExecutor {
    type Error = FamilyExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let name = invocation.request().name().as_str().to_owned();
        self.capture_git_objects_before_commit(&name)
            .map_err(FamilyExecutorError::new)?;
        let git_execution_started = matches!(
            name.as_str(),
            GIT_BRANCH_SWITCH_NAME | GIT_CREATE_COMMIT_NAME
        )
        .then(current_git_recorded_time)
        .transpose()
        .map_err(FamilyExecutorError::new)?;
        let filesystem_execution_started = matches!(
            name.as_str(),
            GIT_BRANCH_CREATE_NAME
                | GIT_BRANCH_SWITCH_NAME
                | GIT_CREATE_COMMIT_NAME
                | GIT_STAGE_NAME
                | APPLY_PATCH_NAME
                | CARGO_DIAGNOSTICS_NAME
                | EDIT_FILE_NAME
                | SANDBOXED_EXEC_NAME
                | WRITE_FILE_NAME
        )
        .then(current_filesystem_recorded_time)
        .transpose()
        .map_err(FamilyExecutorError::new)?;
        let receipt_binding = invocation.clone();
        let mut inner = self.inner.lock().await;
        let evidence = match &mut *inner {
            FamilyExecutor::Git(executor) => executor
                .execute(invocation)
                .await
                .map_err(FamilyExecutorError::new),
            FamilyExecutor::Workspace { read, .. }
                if matches!(
                    name.as_str(),
                    READ_FILE_NAME | LIST_DIRECTORY_NAME | GLOB_FILES_NAME | SEARCH_FILES_NAME
                ) =>
            {
                read.execute(invocation)
                    .await
                    .map_err(FamilyExecutorError::new)
            }
            FamilyExecutor::Workspace { mutation, .. } => mutation
                .execute(invocation)
                .await
                .map_err(FamilyExecutorError::new),
            FamilyExecutor::Web { fetch, .. } if name == WEB_FETCH_NAME => fetch
                .execute(invocation)
                .await
                .map_err(FamilyExecutorError::new),
            FamilyExecutor::Web { search, .. } => search
                .execute(invocation)
                .await
                .map_err(FamilyExecutorError::new),
            FamilyExecutor::Exec {
                sandboxed,
                unsandboxed,
                diagnostics,
                case,
            } => {
                if !case.admits(name.as_str(), invocation.request().arguments()) {
                    return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed { detail: None }));
                }
                match name.as_str() {
                    SANDBOXED_EXEC_NAME => sandboxed
                        .execute(invocation)
                        .await
                        .map_err(FamilyExecutorError::new),
                    UNSANDBOXED_EXEC_NAME => unsandboxed
                        .execute(invocation)
                        .await
                        .map_err(FamilyExecutorError::new),
                    _ => diagnostics
                        .execute(invocation)
                        .await
                        .map_err(FamilyExecutorError::new),
                }
            }
        }?;
        if let Some(started) = git_execution_started {
            let finished = current_git_recorded_time().map_err(FamilyExecutorError::new)?;
            self.git_execution_windows
                .lock()
                .expect("Git execution-window lock is available")
                .insert(name.clone(), GitExecutionTimeWindow { started, finished });
        }
        if let Some(started) = filesystem_execution_started {
            self.record_filesystem_execution_window(
                &name,
                FilesystemExecutionTimeWindow {
                    started,
                    finished: current_filesystem_recorded_time()
                        .map_err(FamilyExecutorError::new)?,
                },
            );
        }
        if evidence.correlation() != receipt_binding.correlation() {
            return Err(FamilyExecutorError::new(io::Error::other(
                "the eval executor returned mismatched correlation",
            )));
        }
        let receipt = Uuid::now_v7().to_string();
        let evidence = add_eval_receipt(evidence.evidence().clone(), &receipt)
            .map_err(FamilyExecutorError::new)?;
        Ok(receipt_binding.bind(evidence))
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ExecApprovalMode {
    DenyAll,
    ApproveOneExactForced,
}

#[derive(Clone, Copy)]
pub(crate) enum ExecApprovalCap {
    NotReached,
    Reached,
}

pub(crate) struct ExecApprovalState {
    pub(crate) mode: ExecApprovalMode,
    pub(crate) exact_forced_approved: bool,
}

impl ExecApprovalState {
    pub(crate) const fn new(mode: ExecApprovalMode) -> Self {
        Self {
            mode,
            exact_forced_approved: false,
        }
    }

    pub(crate) fn decision(
        &mut self,
        name: &str,
        arguments: &NormalizedToolArguments,
    ) -> ToolApprovalDecision {
        if matches!(self.mode, ExecApprovalMode::ApproveOneExactForced)
            && !self.exact_forced_approved
            && ExecEvalCase::ForcedUnsandboxed.admits(name, arguments)
        {
            self.exact_forced_approved = true;
            ToolApprovalDecision::Approve
        } else {
            ToolApprovalDecision::Deny { reason: None }
        }
    }
}

/// Whether a serialized direct-command or Cargo result reports a runner failure
/// before or around execution, rather than evidence about the requested task.
pub(crate) fn exec_result_is_infrastructure(result: &TrackedToolResult) -> bool {
    exec_result_infrastructure_label(result).is_some()
}

pub(crate) fn exec_result_infrastructure_label(result: &TrackedToolResult) -> Option<&'static str> {
    if result.is_error {
        return None;
    }
    let Ok(result) = serde_json::from_str::<serde_json::Value>(&result.content) else {
        return None;
    };
    let execution = result.get("execution").unwrap_or(&result);
    if execution
        .get("cargo_failure")
        .is_some_and(|failure| !failure.is_null())
    {
        return Some("Cargo failure");
    }
    match execution["confinement"]["kind"].as_str() {
        Some("sandbox_refused") => return Some("sandbox refused"),
        Some("sandbox_setup_failed") => return Some("sandbox setup failed"),
        _ => {}
    }
    match execution["outcome"]["kind"].as_str() {
        Some("spawn_failed") => Some("spawn failed"),
        Some("supervision_failed") => Some("supervision failed"),
        Some("timed_out") => Some("timed out"),
        Some("exited")
            if execution["outcome"]["code"]
                .as_i64()
                .is_some_and(|code| code != 0) =>
        {
            Some("nonzero exit")
        }
        _ => None,
    }
}

/// The closed direct-command result envelope accepted as successful eval
/// evidence. The receipt is injected by this harness after execution.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectExecEvalResult {
    pub(crate) confinement: DirectExecEvalConfinement,
    pub(crate) outcome: DirectExecEvalOutcome,
    pub(crate) stdout: DirectExecEvalStream,
    pub(crate) stderr: DirectExecEvalStream,
    pub(crate) eval_receipt: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectExecEvalConfinement {
    pub(crate) kind: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectExecEvalOutcome {
    pub(crate) kind: String,
    pub(crate) code: Option<i64>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectExecEvalStream {
    pub(crate) text: String,
    pub(crate) completeness: String,
    pub(crate) encoding: String,
}

pub(crate) struct DirectExecExpectation<'a> {
    pub(crate) confinement: &'a str,
    pub(crate) stdout: &'a str,
}

pub(crate) fn direct_exec_result_passed(
    result: &serde_json::Value,
    expectation: DirectExecExpectation<'_>,
) -> bool {
    let Ok(result) = serde_json::from_value::<DirectExecEvalResult>(result.clone()) else {
        return false;
    };
    result.confinement.kind == expectation.confinement
        && result.outcome.kind == "exited"
        && result.outcome.code == Some(0)
        && !result.eval_receipt.is_empty()
        && direct_exec_stream_is(&result.stdout, expectation.stdout)
        && direct_exec_stream_is(&result.stderr, "")
}

pub(crate) fn direct_exec_stream_is(stream: &DirectExecEvalStream, expected: &str) -> bool {
    stream.text == expected && stream.completeness == "complete" && stream.encoding == "utf8"
}

pub(crate) fn denied_unsandboxed_request(request_id: Uuid) -> RequestSnapshot {
    RequestSnapshot {
        request_id,
        producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
        name: String::from(UNSANDBOXED_EXEC_NAME),
        arguments_text: String::from("{}"),
        entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
        completed_result_entry_index: None,
        attempt_succeeded: false,
        attempt_denied: true,
    }
}

/// One forced Exec outcome whose sole result carries the supplied execution.
pub(crate) fn forced_exec_outcome(
    target: &'static str,
    execution: serde_json::Value,
) -> CaseOutcome {
    let fixture = forced_exec_fixture(target);
    CaseOutcome {
        target: Some(String::from(fixture.name)),
        expected_arguments: Some(String::from(fixture.expected_arguments)),
        execution_completed: true,
        forced_verification_failed: false,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: execution.to_string(),
            is_error: false,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(fixture.name),
                arguments_text: String::from(fixture.expected_arguments),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    }
}

pub(crate) struct ZeroExitEvidence<'a> {
    pub(crate) confinement: ExecutionConfinement,
    pub(crate) stdout: &'a str,
}

/// One serialized execution with the selected confinement and zero exit.
pub(crate) fn zero_exit_with_confinement(evidence: ZeroExitEvidence<'_>) -> serde_json::Value {
    direct_exec_result(DirectExecEvidence::successful_with_confinement(
        evidence.confinement,
        evidence.stdout,
    ))
}

pub(crate) struct DirectExecEvidence<'a> {
    pub(crate) confinement: ExecutionConfinement,
    pub(crate) outcome: ProcessOutcome,
    pub(crate) stdout: &'a str,
    pub(crate) completeness: CaptureCompleteness,
}

impl<'a> DirectExecEvidence<'a> {
    pub(crate) fn confined_success(stdout: &'a str) -> Self {
        Self::successful_with_confinement(ExecutionConfinement::FilesystemConfined, stdout)
    }

    pub(crate) fn successful_with_confinement(
        confinement: ExecutionConfinement,
        stdout: &'a str,
    ) -> Self {
        Self {
            confinement,
            outcome: ProcessOutcome::Exited { code: Some(0) },
            stdout,
            completeness: CaptureCompleteness::Complete,
        }
    }

    pub(crate) fn unsandboxed_truncated(stdout: &'a str) -> Self {
        Self {
            confinement: ExecutionConfinement::Unsandboxed,
            outcome: ProcessOutcome::Exited { code: Some(0) },
            stdout,
            completeness: CaptureCompleteness::Truncated,
        }
    }

    pub(crate) fn timed_out() -> Self {
        Self {
            outcome: ProcessOutcome::TimedOut,
            ..Self::confined_success("")
        }
    }

    pub(crate) fn nonzero_exit() -> Self {
        Self {
            outcome: ProcessOutcome::Exited { code: Some(1) },
            ..Self::confined_success("")
        }
    }

    pub(crate) fn supervision_failure() -> Self {
        Self {
            outcome: ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Wait,
            },
            ..Self::confined_success("")
        }
    }

    pub(crate) fn sandbox_refusal() -> Self {
        Self {
            confinement: ExecutionConfinement::SandboxRefused {
                availability: BwrapAvailability::Unusable,
            },
            outcome: ProcessOutcome::SpawnFailed {
                reason: ProcessSpawnFailure::SandboxUnavailable,
            },
            stdout: "",
            completeness: CaptureCompleteness::Complete,
        }
    }

    pub(crate) fn sandbox_setup_failure() -> Self {
        Self {
            confinement: ExecutionConfinement::SandboxSetupFailed,
            outcome: ProcessOutcome::SpawnFailed {
                reason: ProcessSpawnFailure::SandboxSetup,
            },
            stdout: "",
            completeness: CaptureCompleteness::Complete,
        }
    }
}

pub(crate) fn direct_exec_result(evidence: DirectExecEvidence<'_>) -> serde_json::Value {
    let mut result = serde_json::to_value(ExecResult {
        confinement: evidence.confinement,
        outcome: evidence.outcome,
        stdout: OutputCapture {
            text: evidence.stdout.to_owned(),
            completeness: evidence.completeness,
            encoding: OutputEncoding::Utf8,
        },
        stderr: OutputCapture {
            text: String::new(),
            completeness: CaptureCompleteness::Complete,
            encoding: OutputEncoding::Utf8,
        },
    })
    .expect("producer direct-exec result serializes");
    result[EVAL_RECEIPT_FIELD] = serde_json::json!(SYNTHETIC_EVAL_RECEIPT);
    result
}

/// One unforced Exec outcome whose sole request carries the supplied execution.
pub(crate) fn natural_exec_outcome(execution: serde_json::Value) -> CaseOutcome {
    CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        forced_verification_failed: false,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: execution.to_string(),
            is_error: false,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(SANDBOXED_EXEC_NAME),
                arguments_text: String::from(EXEC_NATURAL_ARGUMENTS),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    }
}

#[track_caller]
pub(crate) fn assert_forced_exec_fixture_is_admitted(case: &ForcedCase) -> EvalResult {
    let arguments =
        NormalizedToolArguments::try_from_provider_text(case.expected_arguments.to_owned())
            .map_err(|_| io::Error::other("a forced exec fixture does not normalize"))?;

    assert!(
        ExecEvalCase::for_forced_tool(case.name)?.admits(case.name, &arguments),
        "the dispatch allowlist rejects the reported fixture for {}",
        case.name
    );
    Ok(())
}

pub(crate) fn record_workspace_read_result(
    tracker: &OperationTracker,
    content: &str,
    truncated: bool,
) {
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &serde_json::json!({
            "path": WORKSPACE_SEED_PATH,
            "content": content,
            "offset": 0,
            "bytes_read": content.len(),
            "next_offset": content.len(),
            "total_bytes": WORKSPACE_SEED.len(),
            "truncated": truncated,
            EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
        })
        .to_string(),
    );
}

pub(crate) fn record_workspace_write_result(
    tracker: &OperationTracker,
    path: &str,
    bytes_written: usize,
    created: bool,
) {
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
        &serde_json::json!({
            "path": path,
            "bytes_written": bytes_written,
            "created": created,
            EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
        })
        .to_string(),
    );
}

pub(crate) type PreparedExecNaturalWorkspace = (
    TempDir,
    BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    BTreeMap<PathBuf, SystemTime>,
    BTreeMap<PathBuf, FilesystemIdentity>,
    BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
    BTreeMap<PathBuf, u32>,
    FilesystemExecutionTimeWindow,
);

pub(crate) fn prepared_exec_natural_workspace() -> EvalResult<PreparedExecNaturalWorkspace> {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let seed_inode_flags = workspace_inode_flags(workspace.path())?;
    let result = workspace.path().join(EXEC_RESULT_PATH);
    let started = current_filesystem_recorded_time()?;
    fs::write(&result, EXEC_RESULT)?;
    #[cfg(unix)]
    fs::set_permissions(
        result,
        fs::Permissions::from_mode(WORKSPACE_PRIVATE_CREATION_MODE),
    )?;
    let finished = current_filesystem_recorded_time()?;
    Ok((
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        FilesystemExecutionTimeWindow { started, finished },
    ))
}

pub(crate) fn successful_web_natural_snapshot() -> EvalResult<CaseSnapshot> {
    Ok(CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_SEARCH_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"query": WEB_QUERY}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_FETCH_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"url": WEB_URL}).to_string(),
                )?,
                entry_index: ARBITRARY_LATE_RESULT_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_LATE_RESULT_ENTRY_INDEX + 1),
                attempt_succeeded: true,
                attempt_denied: false,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    })
}

pub(crate) fn record_web_natural_results(
    tracker: &OperationTracker,
    search_result: serde_json::Value,
    fetch_result: serde_json::Value,
) {
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &search_result.to_string(),
    );
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
        &fetch_result.to_string(),
    );
}

pub(crate) fn exact_web_search_result() -> serde_json::Value {
    serde_json::json!({
        "results": [{
            "title": WEB_SEARCH_TITLE,
            "url": WEB_URL,
            "snippet": WEB_SEARCH_SNIPPET,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
}

pub(crate) fn exact_web_fetch_result() -> serde_json::Value {
    serde_json::json!({
        "url": WEB_URL,
        "status": 200,
        "content_type": "text/plain",
        "body": WEB_FETCH_BODY,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
}
