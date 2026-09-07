//! Shared evaluation driver, snapshots, and outcomes.

use crate::*;

pub(crate) const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
pub(crate) const DATABASE_NAME: &str = "signalbox_live_tool_evals";
pub(crate) const DATABASE_USER: &str = "signalbox";
pub(crate) const DATABASE_PASSWORD: &str = "signalbox-test-only";
pub(crate) const POSTGRES_PORT: u16 = 5432;
pub(crate) const POSTGRES_POOL_CONNECTIONS: u32 = 8;
pub(crate) const API_KEY_VARIABLE: &str = "OPENAI_API_KEY";
pub(crate) const FAMILY_VARIABLE: &str = "SIGNALBOX_TOOL_EVAL_FAMILY";
pub(crate) const SUMMARY_VARIABLE: &str = "SIGNALBOX_TOOL_EVAL_SUMMARY";
pub(crate) const DEFAULT_MODEL: &str = "gpt-5-nano";
/// Output ceiling for one eval exchange.
///
/// The selected models are reasoning models, and the provider charges reasoning
/// tokens against this same ceiling before the visible tool call is emitted. A
/// ceiling small enough to be reached while reasoning terminates the response
/// with the provider's `length` token, which this adapter deliberately refuses
/// to interpret, so the turn terminalizes as a provider failure and the family
/// reports no capability evidence at all. The ceiling therefore sits far above
/// any plausible reasoning burn for these single-call fixtures; it bounds a
/// runaway response rather than shaping the expected one.
pub(crate) const MAX_OUTPUT_TOKENS: u32 = 16_384;
pub(crate) const MAX_NATURAL_APPROVAL_CONTINUATIONS: usize = 2;
pub(crate) const CONTEXT_WINDOW_TOKENS: u32 = 200_000;
pub(crate) const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(2 * 60);
/// Three tool-enabled exchanges plus the final answer cover every accepted
/// natural path, with one minute for local persistence and dispatch.
pub(crate) const TURN_TIMEOUT: Duration = Duration::from_secs(4 * 2 * 60 + 60);
pub(crate) const MAX_NATURAL_TOOL_EXCHANGES: usize = 3;
pub(crate) const MAX_NATURAL_MODEL_CALLS: i64 = MAX_NATURAL_TOOL_EXCHANGES as i64 + 1;
pub(crate) const LIVE_EVAL_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const EXPECTED_WORKSPACE_EDIT_REPLACEMENTS: usize = 2;
pub(crate) const EXPECTED_WORKSPACE_EDIT_BYTES: usize = 23;
#[cfg(unix)]
pub(crate) const USER_EXECUTE_MODE_BIT: u32 = 0o100;
#[cfg(unix)]
pub(crate) const GROUP_WRITE_MODE_BIT: u32 = 0o020;
#[cfg(target_os = "linux")]
pub(crate) const SYNTHETIC_UNEXPECTED_XATTR_NAME: &str = "user.signalbox_tool_eval";
#[cfg(target_os = "linux")]
pub(crate) const SYNTHETIC_UNEXPECTED_XATTR_VALUE: &[u8] = b"unexpected synthetic metadata";
pub(crate) const OPENAI_MODEL_FAMILY: &str = "openai";
pub(crate) const OPENAI_FALLBACK_CREDENTIAL_REFERENCE: &str = "openai-tool-eval";
pub(crate) const EXPECTED_OPENAI_CREDENTIAL_REFERENCE: &str = "openai-primary";
pub(crate) const ARBITRARY_EVAL_SELECTION_ID: u128 = 0x9101;
pub(crate) const ARBITRARY_EVAL_PROVIDER_ID: u128 = 0x9102;
pub(crate) const ARBITRARY_EVAL_REQUEST_ID: u128 = 0x9103;
pub(crate) const ARBITRARY_EVAL_MODEL_CALL_ID: u128 = 0x9108;
pub(crate) const ARBITRARY_SECOND_EVAL_REQUEST_ID: u128 = 0x910a;
pub(crate) const ARBITRARY_SECOND_EVAL_MODEL_CALL_ID: u128 = 0x910b;
pub(crate) const ARBITRARY_THIRD_EVAL_REQUEST_ID: u128 = 0x910d;
pub(crate) const ARBITRARY_FOURTH_EVAL_REQUEST_ID: u128 = 0x910e;
pub(crate) const ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX: u64 = 1;
pub(crate) const ARBITRARY_REQUEST_ENTRY_INDEX: u64 = 2;
pub(crate) const ARBITRARY_LATE_RESULT_ENTRY_INDEX: u64 = 3;
pub(crate) const MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP: i64 = 2;
pub(crate) const SYNTHETIC_EXECUTOR_FAILURE: &str = "synthetic executor failure";
pub(crate) const SYNTHETIC_EVAL_RECEIPT: &str = "01988c5f-89c4-7000-8000-000000000001";
pub(crate) const EXACT_EXECUTOR_FAILURE: &str =
    "an exact tool request reached its executor and failed";
pub(crate) const SYNTHETIC_NO_FILE_CHANGES_COMPLETION_REPORT: &str =
    "Done; no file changes were made.";
pub(crate) const SYNTHETIC_FILE_NOT_MODIFIED_REPORT: &str = "Done, but the file was not modified.";
pub(crate) const SYNTHETIC_VERB_FIRST_MODIFICATION_DENIAL_REPORT: &str =
    "Done, but I did not modify any files.";
pub(crate) const SYNTHETIC_BARE_NO_CHANGES_DENIAL_REPORT: &str = "Done, but no changes were made.";
pub(crate) const SYNTHETIC_SCOPED_NEGATION_COMPLETION_REPORT: &str =
    "Completed the commit; I did not modify any other files.";
pub(crate) const EVAL_RECEIPT_FIELD: &str = "eval_receipt";
pub(crate) const RESULT_RECEIPT_INSTRUCTION: &str =
    "In your final answer, include every exact eval_receipt value returned by the tools.";

pub(crate) type EvalResult<T = ()> = Result<T, Box<dyn Error>>;

pub(crate) async fn run_selected_family_if_enabled() -> EvalResult {
    let Some(family) = EvalFamily::from_environment()? else {
        return Ok(());
    };
    let database = EvalDatabase::start(family.model()).await?;
    let forced = run_forced_tier(&database, family).await?;
    let natural_suite = family.build_suite()?;
    let natural = run_case(
        &database,
        &natural_suite,
        None,
        natural_suite.natural_prompt(),
    )
    .await?;
    let natural_state = natural_suite.natural_state_passed(&natural.snapshot)?;
    let report = FamilyReport {
        family,
        forced,
        natural,
        natural_state: EvalDisposition::from_passed(natural_state),
    };
    write_report(&report)?;
    reject_credential_rejections(&report)?;
    reject_forced_executor_failures(&report.forced)?;
    reject_natural_executor_failure(&report.natural, family, report.natural_state)?;
    Ok(())
}

pub(crate) async fn run_forced_tier(
    database: &EvalDatabase,
    family: EvalFamily,
) -> EvalResult<Vec<CaseOutcome>> {
    let inventory_suite = family.build_suite()?;
    inventory_suite.validate_forced_inventory()?;
    let cases = inventory_suite.forced_cases();
    drop(inventory_suite);
    let mut outcomes = Vec::new();
    for case in cases {
        let suite = family.build_suite()?;
        suite.prepare_for(case.name).await?;
        outcomes.push(run_case(database, &suite, Some(case), case.prompt).await?);
    }
    Ok(outcomes)
}

pub(crate) async fn run_case(
    database: &EvalDatabase,
    suite: &FamilySuite,
    forced_case: Option<&ForcedCase>,
    prompt: &str,
) -> EvalResult<CaseOutcome> {
    let forced_tool = forced_case.map(|case| case.name);
    let prompt = format!("{prompt} {RESULT_RECEIPT_INSTRUCTION}");
    let (session, turn, activated) = database.start_turn(&prompt).await?;
    let tracker = OperationTracker::default();
    let runtime = EvalOpenAiRuntime::new(forced_tool, tracker.clone())?;
    let provider = RuntimeModelCallProvider::new(runtime, database.runtime_models.clone(), None);
    let execution = PostgresProviderModelExecution::new(
        PostgresModelCallRepository::new(
            database.pool.clone(),
            database.targets.clone(),
            ModelCallCredentialReference::new(OPENAI_FALLBACK_CREDENTIAL_REFERENCE),
        )
        .with_session_credentials(database.credential_families.clone()),
        InProcessAttemptDispatchGate::default(),
        provider,
        None,
    )
    .with_tool_loop(
        InProcessToolDispatchGate::default(),
        suite.catalog.clone(),
        suite.executor.clone(),
    )
    .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
        database.pool.clone(),
        None,
        Vec::new(),
    ));
    timeout(TURN_TIMEOUT, execution.execute(Box::new(activated)))
        .await
        .map_err(|_| io::Error::other("the daemon tool eval turn exceeded its timeout"))??;
    let approval_mode = if forced_tool == Some(UNSANDBOXED_EXEC_NAME) {
        ExecApprovalMode::ApproveOneExactForced
    } else {
        ExecApprovalMode::DenyAll
    };
    let mut approval_state = ExecApprovalState::new(approval_mode);
    let mut approval_continuations = 0;
    let mut approval_cap = ExecApprovalCap::NotReached;
    while database
        .decide_pending_unsandboxed_requests(session, turn, &mut approval_state)
        .await?
    {
        if approval_continuations == MAX_NATURAL_APPROVAL_CONTINUATIONS {
            approval_cap = ExecApprovalCap::Reached;
            break;
        }
        timeout(TURN_TIMEOUT, execution.resume_active(session))
            .await
            .map_err(|_| io::Error::other("the daemon tool eval resume exceeded its timeout"))??;
        approval_continuations += 1;
    }
    let snapshot = CaseSnapshot::read(&database.pool, session, turn, approval_cap).await?;
    let expected_arguments = forced_case
        .map(|case| normalized_arguments_text(case.expected_arguments))
        .transpose()?;
    let mut forced_verification_failed = false;
    let execution_completed = match (
        forced_case,
        snapshot.requests.as_slice(),
        expected_arguments.as_deref(),
    ) {
        (None, _, _) => suite.natural_execution_completed(&snapshot, &tracker)?,
        (Some(case), [request], Some(expected_arguments)) => {
            match tracker.result_content(request.request_id) {
                Some(content) => {
                    let execution_verified = forced_execution_completed(
                        suite,
                        case,
                        ForcedExecutionEvidence {
                            persisted_arguments: &request.arguments_text,
                            expected_arguments,
                            result_content: &content,
                        },
                    )?;
                    forced_verification_failed = !execution_verified;
                    forced_case_completion_reported(case.name, execution_verified, &tracker)
                }
                None => false,
            }
        }
        (Some(_), _, _) => false,
    };
    Ok(CaseOutcome {
        target: forced_tool.map(str::to_owned),
        expected_arguments,
        execution_completed,
        forced_verification_failed,
        tool_results: tracker.tool_results(),
        snapshot,
    })
}

pub(crate) fn forced_case_completion_reported(
    case_name: &str,
    execution_completed: bool,
    tracker: &OperationTracker,
) -> bool {
    let required_effect = match case_name {
        EDIT_FILE_NAME => RequiredFileEffect::Mutate,
        APPLY_PATCH_NAME | WRITE_FILE_NAME => RequiredFileEffect::Create,
        _ => RequiredFileEffect::None,
    };
    let output_required = matches!(case_name, SANDBOXED_EXEC_NAME | UNSANDBOXED_EXEC_NAME);
    execution_completed
        && tracker.final_response_reports_completion_with_required_file_effect(required_effect)
        && tracker.final_response_reports_case_outcome(case_name)
        && (!output_required || !tracker.final_response_denies_exec_output())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EvalFamily {
    Git,
    Workspace,
    Web,
    Exec,
}

impl EvalFamily {
    pub(crate) fn from_environment() -> EvalResult<Option<Self>> {
        match std::env::var(FAMILY_VARIABLE).as_deref() {
            Ok("git") => Ok(Some(Self::Git)),
            Ok("workspace") => Ok(Some(Self::Workspace)),
            Ok("web") => Ok(Some(Self::Web)),
            Ok("exec") => Ok(Some(Self::Exec)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            _ => Err(io::Error::other("the configured tool-eval family is unsupported").into()),
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Workspace => "workspace",
            Self::Web => "web",
            Self::Exec => "exec",
        }
    }

    pub(crate) const fn model(self) -> &'static str {
        match self {
            Self::Git | Self::Workspace | Self::Web | Self::Exec => DEFAULT_MODEL,
        }
    }

    pub(crate) fn build_suite(self) -> EvalResult<FamilySuite> {
        match self {
            Self::Git => FamilySuite::git(),
            Self::Workspace => FamilySuite::workspace(),
            Self::Web => FamilySuite::web(),
            Self::Exec => FamilySuite::exec(),
        }
    }
}

pub(crate) struct ForcedCase {
    pub(crate) name: &'static str,
    pub(crate) expected_arguments: &'static str,
    pub(crate) prompt: &'static str,
}

pub(crate) struct FamilySuite {
    pub(crate) family: EvalFamily,
    pub(crate) workspace: TempDir,
    pub(crate) git_seed: Option<Oid>,
    pub(crate) git_seed_refs: GitReferenceInventory,
    pub(crate) git_seed_fixture: GitFixtureSnapshot,
    pub(crate) catalog: MergedCatalog,
    pub(crate) executor: SharedFamilyExecutor,
    pub(crate) workspace_seed_entries: BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    pub(crate) workspace_seed_modified_times: BTreeMap<PathBuf, SystemTime>,
    pub(crate) workspace_seed_entry_identities: BTreeMap<PathBuf, FilesystemIdentity>,
    pub(crate) workspace_seed_extended_attributes: BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
    pub(crate) workspace_seed_inode_flags: BTreeMap<PathBuf, u32>,
    pub(crate) git_pre_execution_worktree_entries:
        StdMutex<Option<BTreeMap<PathBuf, WorkspaceEntrySnapshot>>>,
    pub(crate) git_pre_execution_worktree_modified_times:
        StdMutex<Option<BTreeMap<PathBuf, SystemTime>>>,
    pub(crate) git_pre_execution_worktree_entry_identities:
        StdMutex<Option<BTreeMap<PathBuf, FilesystemIdentity>>>,
    pub(crate) git_pre_execution_worktree_extended_attributes:
        StdMutex<Option<BTreeMap<PathBuf, ExtendedAttributeSnapshot>>>,
    pub(crate) git_pre_execution_metadata_extended_attributes:
        StdMutex<Option<BTreeMap<PathBuf, ExtendedAttributeSnapshot>>>,
    pub(crate) git_pre_execution_index_entries:
        StdMutex<Option<Vec<GitIndexCompleteEntrySnapshot>>>,
    pub(crate) git_pre_execution_metadata_root_modified_time: StdMutex<Option<SystemTime>>,
    pub(crate) git_pre_execution_metadata_root_identity: StdMutex<Option<FilesystemIdentity>>,
    pub(crate) git_pre_execution_metadata_top_level:
        StdMutex<Option<BTreeMap<PathBuf, GitMetadataEntrySnapshot>>>,
    pub(crate) git_pre_execution_objects: StdMutex<Option<GitObjectInventory>>,
    pub(crate) git_pre_execution_object_entries:
        Arc<StdMutex<Option<BTreeMap<PathBuf, WorkspaceEntrySnapshot>>>>,
    pub(crate) git_pre_execution_object_modified_times:
        Arc<StdMutex<Option<BTreeMap<PathBuf, SystemTime>>>>,
    pub(crate) git_pre_execution_object_entry_identities:
        Arc<StdMutex<Option<BTreeMap<PathBuf, FilesystemIdentity>>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FilesystemIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) user_id: u32,
    pub(crate) group_id: u32,
    pub(crate) change_time_seconds: i64,
    pub(crate) change_time_nanoseconds: i64,
}

pub(crate) type ExtendedAttributeSnapshot = BTreeMap<Vec<u8>, Vec<u8>>;

#[cfg(unix)]
pub(crate) fn filesystem_identity(metadata: &fs::Metadata) -> Option<FilesystemIdentity> {
    Some(FilesystemIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        user_id: metadata.uid(),
        group_id: metadata.gid(),
        change_time_seconds: metadata.ctime(),
        change_time_nanoseconds: metadata.ctime_nsec(),
    })
}

#[cfg(not(unix))]
pub(crate) const fn filesystem_identity(_metadata: &fs::Metadata) -> Option<FilesystemIdentity> {
    None
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FilesystemExecutionTimeWindow {
    pub(crate) started: SystemTime,
    pub(crate) finished: SystemTime,
}

impl FilesystemExecutionTimeWindow {
    pub(crate) fn contains_modified(self, modified: SystemTime) -> bool {
        (self.started..=self.finished).contains(&modified)
    }

    pub(crate) fn contains_git_modified(
        self,
        modified: SystemTime,
        identity: FilesystemIdentity,
    ) -> bool {
        if self.contains_modified(modified) {
            return true;
        }
        let Ok(modified) = modified.duration_since(UNIX_EPOCH) else {
            return false;
        };
        modified.subsec_nanos() == 0
            && u64::try_from(identity.change_time_seconds).ok() == Some(modified.as_secs())
            && self.contains_change_time(identity)
    }

    pub(crate) fn contains_change_time(self, identity: FilesystemIdentity) -> bool {
        let Ok(started) = self.started.duration_since(UNIX_EPOCH) else {
            return false;
        };
        let Ok(finished) = self.finished.duration_since(UNIX_EPOCH) else {
            return false;
        };
        let Ok(seconds) = u64::try_from(identity.change_time_seconds) else {
            return false;
        };
        let Ok(nanoseconds) = u32::try_from(identity.change_time_nanoseconds) else {
            return false;
        };
        ((started.as_secs(), started.subsec_nanos())
            ..=(finished.as_secs(), finished.subsec_nanos()))
            .contains(&(seconds, nanoseconds))
    }
}

pub(crate) fn current_filesystem_recorded_time() -> io::Result<SystemTime> {
    let marker = tempfile::tempfile()?;
    marker.metadata()?.modified()
}

pub(crate) fn current_git_recorded_time() -> Result<GitRecordedTime, git2::Error> {
    Signature::now(GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL).map(|signature| signature.when().into())
}

impl FamilySuite {
    pub(crate) const fn forced_cases(&self) -> &'static [ForcedCase] {
        match self.family {
            EvalFamily::Git => GIT_CASES,
            EvalFamily::Workspace => WORKSPACE_CASES,
            EvalFamily::Web => WEB_CASES,
            EvalFamily::Exec => EXEC_CASES,
        }
    }

    pub(crate) fn validate_forced_inventory(&self) -> EvalResult {
        let catalog_names = self
            .catalog
            .definitions()
            .into_iter()
            .map(|definition| definition.name().as_str().to_owned())
            .collect::<BTreeSet<_>>();
        let cases = self.forced_cases();
        let case_names = cases
            .iter()
            .map(|case| case.name.to_owned())
            .collect::<BTreeSet<_>>();
        if catalog_names != case_names
            || case_names.len() != cases.len()
            || cases
                .iter()
                .any(|case| !self.has_forced_case_verifier(case.name))
        {
            return Err(
                io::Error::other("the forced eval inventory differs from its catalog").into(),
            );
        }
        for case in cases {
            self.validate_forced_case(case)?;
        }
        Ok(())
    }

    pub(crate) fn validate_forced_case(&self, case: &ForcedCase) -> EvalResult {
        let name = DomainToolName::try_new(case.name.to_owned())
            .map_err(|_| io::Error::other("the forced eval tool name is invalid"))?;
        let arguments =
            NormalizedToolArguments::try_from_provider_text(case.expected_arguments.to_owned())
                .map_err(|_| io::Error::other("the forced eval arguments do not normalize"))?;
        self.catalog
            .validate_arguments(&name, &arguments)
            .map_err(|_| io::Error::other("the forced eval arguments violate the tool contract"))?;
        Ok(())
    }

    pub(crate) const fn natural_prompt(&self) -> &'static str {
        match self.family {
            EvalFamily::Git => {
                "Stage eval.txt and commit it with the exact message 'tool eval commit'. Use the available tools, then briefly report completion."
            }
            EvalFamily::Workspace => {
                "Read brief.txt, then write answer.txt containing exactly 'model loop observed' followed by a newline. Use the available tools, then briefly report completion."
            }
            EvalFamily::Web => {
                "Search the web for 'Signalbox tool evaluation', then fetch the result at https://example.com/eval. Use the available tools, then briefly report what you found."
            }
            EvalFamily::Exec => {
                "Use sandboxed_exec with exactly {\"program\":\"/bin/sh\",\"arguments\":[\"-c\",\"umask 077; printf 'model loop observed\\n' > exec-result.txt\"],\"working_directory\":\".\",\"timeout_seconds\":30} to create exec-result.txt. Keep every side effect inside the current workspace, then briefly report completion."
            }
        }
    }

    pub(crate) async fn prepare_for(&self, tool: &str) -> EvalResult {
        self.prepare_git_case(tool)?;
        if self.family == EvalFamily::Exec {
            self.executor.prepare_exec_case(tool).await?;
        }
        Ok(())
    }

    pub(crate) fn has_forced_case_verifier(&self, name: &str) -> bool {
        match self.family {
            EvalFamily::Git => matches!(
                name,
                GIT_BRANCH_CREATE_NAME
                    | GIT_BRANCH_SWITCH_NAME
                    | GIT_CREATE_COMMIT_NAME
                    | GIT_DIFF_NAME
                    | GIT_LOG_NAME
                    | GIT_STAGE_NAME
                    | GIT_STATUS_NAME
            ),
            EvalFamily::Workspace => matches!(
                name,
                APPLY_PATCH_NAME
                    | EDIT_FILE_NAME
                    | WRITE_FILE_NAME
                    | READ_FILE_NAME
                    | LIST_DIRECTORY_NAME
                    | GLOB_FILES_NAME
                    | SEARCH_FILES_NAME
            ),
            EvalFamily::Web => matches!(name, WEB_FETCH_NAME | WEB_SEARCH_NAME),
            EvalFamily::Exec => matches!(
                name,
                SANDBOXED_EXEC_NAME | UNSANDBOXED_EXEC_NAME | CARGO_DIAGNOSTICS_NAME
            ),
        }
    }

    pub(crate) fn forced_case_result_passed(
        &self,
        case: &ForcedCase,
        content: &str,
    ) -> EvalResult<bool> {
        let Ok(arguments) = serde_json::from_str::<serde_json::Value>(case.expected_arguments)
        else {
            return Ok(false);
        };
        let Ok(result) = serde_json::from_str::<serde_json::Value>(content) else {
            return Ok(false);
        };
        match self.family {
            EvalFamily::Git => {
                let seed = self.git_seed.ok_or_else(|| {
                    io::Error::other("the Git eval suite has no captured seed identity")
                })?;
                let pre_execution_worktree_entries = self
                    .git_pre_execution_worktree_entries
                    .lock()
                    .expect("Git pre-execution inventory lock is available");
                let pre_execution_worktree_modified_times = self
                    .git_pre_execution_worktree_modified_times
                    .lock()
                    .expect("Git pre-execution worktree-time lock is available");
                let pre_execution_worktree_entry_identities = self
                    .git_pre_execution_worktree_entry_identities
                    .lock()
                    .expect("Git pre-execution worktree-identity lock is available");
                let pre_execution_worktree_extended_attributes = self
                    .git_pre_execution_worktree_extended_attributes
                    .lock()
                    .expect("Git pre-execution worktree-attribute lock is available");
                let pre_execution_metadata_extended_attributes = self
                    .git_pre_execution_metadata_extended_attributes
                    .lock()
                    .expect("Git pre-execution metadata-attribute lock is available");
                let pre_execution_index_entries = self
                    .git_pre_execution_index_entries
                    .lock()
                    .expect("Git pre-execution index lock is available");
                let pre_execution_metadata_root_modified_time = self
                    .git_pre_execution_metadata_root_modified_time
                    .lock()
                    .expect("Git pre-execution metadata-root-time lock is available");
                let pre_execution_metadata_root_identity = self
                    .git_pre_execution_metadata_root_identity
                    .lock()
                    .expect("Git pre-execution metadata-root-identity lock is available");
                let pre_execution_metadata_top_level = self
                    .git_pre_execution_metadata_top_level
                    .lock()
                    .expect("Git pre-execution metadata lock is available");
                let pre_execution_objects = self
                    .git_pre_execution_objects
                    .lock()
                    .expect("Git pre-execution object lock is available");
                let pre_execution_object_entries = self
                    .git_pre_execution_object_entries
                    .lock()
                    .expect("Git pre-execution object-entry lock is available");
                let pre_execution_object_modified_times = self
                    .git_pre_execution_object_modified_times
                    .lock()
                    .expect("Git pre-execution object-time lock is available");
                let pre_execution_object_entry_identities = self
                    .git_pre_execution_object_entry_identities
                    .lock()
                    .expect("Git pre-execution object-identity lock is available");
                git_forced_case_passed(
                    GitForcedVerification {
                        root: self.workspace.path(),
                        seed,
                        seed_refs: &self.git_seed_refs,
                        seed_fixture: &self.git_seed_fixture,
                        pre_execution_worktree_entries: pre_execution_worktree_entries.as_ref(),
                        pre_execution_worktree_modified_times:
                            pre_execution_worktree_modified_times.as_ref(),
                        pre_execution_worktree_entry_identities:
                            pre_execution_worktree_entry_identities.as_ref(),
                        pre_execution_worktree_extended_attributes:
                            pre_execution_worktree_extended_attributes.as_ref(),
                        pre_execution_metadata_extended_attributes:
                            pre_execution_metadata_extended_attributes.as_ref(),
                        pre_execution_index_entries: pre_execution_index_entries.as_deref(),
                        pre_execution_metadata_root_modified_time:
                            *pre_execution_metadata_root_modified_time,
                        pre_execution_metadata_root_identity: *pre_execution_metadata_root_identity,
                        pre_execution_metadata_top_level: pre_execution_metadata_top_level.as_ref(),
                        pre_execution_objects: pre_execution_objects.as_ref(),
                        pre_execution_object_entries: pre_execution_object_entries.as_ref(),
                        pre_execution_object_modified_times: pre_execution_object_modified_times
                            .as_ref(),
                        pre_execution_object_entry_identities:
                            pre_execution_object_entry_identities.as_ref(),
                        execution_window: self.executor.git_execution_window(case.name),
                        filesystem_execution_window: self
                            .executor
                            .filesystem_execution_window(case.name),
                    },
                    case.name,
                    &arguments,
                    &result,
                )
            }
            EvalFamily::Workspace => workspace_forced_case_passed(
                WorkspaceForcedVerification {
                    root: self.workspace.path(),
                    seed_entries: &self.workspace_seed_entries,
                    seed_modified_times: &self.workspace_seed_modified_times,
                    seed_entry_identities: &self.workspace_seed_entry_identities,
                    seed_extended_attributes: &self.workspace_seed_extended_attributes,
                    seed_inode_flags: &self.workspace_seed_inode_flags,
                    execution_window: self.executor.filesystem_execution_window(case.name),
                },
                case.name,
                &arguments,
                &result,
            ),
            EvalFamily::Web => Ok(web_forced_case_passed(case.name, &arguments, &result)),
            EvalFamily::Exec => {
                let result_matches = exec_forced_case_passed(case.name, &result);
                if !result_matches {
                    Ok(result_matches)
                } else if case.name == CARGO_DIAGNOSTICS_NAME {
                    cargo_diagnostics_workspace_matches_seed(
                        self.workspace.path(),
                        &self.workspace_seed_entries,
                        &self.workspace_seed_modified_times,
                        &self.workspace_seed_entry_identities,
                        &self.workspace_seed_extended_attributes,
                        &self.workspace_seed_inode_flags,
                        self.executor.filesystem_execution_window(case.name),
                    )
                } else {
                    exec_workspace_matches_seed(
                        self.workspace.path(),
                        &self.workspace_seed_entries,
                        &self.workspace_seed_modified_times,
                        &self.workspace_seed_entry_identities,
                        &self.workspace_seed_extended_attributes,
                        &self.workspace_seed_inode_flags,
                    )
                }
            }
        }
    }

    pub(crate) fn natural_state_passed(&self, snapshot: &CaseSnapshot) -> EvalResult<bool> {
        match self.family {
            EvalFamily::Git => {
                let seed = self.git_seed.ok_or_else(|| {
                    io::Error::other("the Git eval suite has no captured seed identity")
                })?;
                let pre_commit_object_entries = self
                    .git_pre_execution_object_entries
                    .lock()
                    .expect("Git pre-execution object-entry lock is available");
                let pre_commit_object_modified_times = self
                    .git_pre_execution_object_modified_times
                    .lock()
                    .expect("Git pre-execution object-time lock is available");
                let pre_commit_object_entry_identities = self
                    .git_pre_execution_object_entry_identities
                    .lock()
                    .expect("Git pre-execution object-identity lock is available");
                Ok(git_natural_state_passed_in_window(
                    self.workspace.path(),
                    seed,
                    &self.git_seed_refs,
                    &self.git_seed_fixture,
                    GitNaturalExecutionVerification {
                        execution_window: self
                            .executor
                            .git_execution_window(GIT_CREATE_COMMIT_NAME),
                        stage_filesystem_execution_window: self
                            .executor
                            .filesystem_execution_window(GIT_STAGE_NAME),
                        commit_filesystem_execution_window: self
                            .executor
                            .filesystem_execution_window(GIT_CREATE_COMMIT_NAME),
                        pre_commit_object_entries: pre_commit_object_entries.as_ref(),
                        pre_commit_object_modified_times: pre_commit_object_modified_times.as_ref(),
                        pre_commit_object_entry_identities: pre_commit_object_entry_identities
                            .as_ref(),
                    },
                )? && snapshot.git_natural_requests_passed()?)
            }
            EvalFamily::Workspace => {
                let entries_match = self.workspace_natural_entries_match()?;
                Ok(entries_match && snapshot.workspace_natural_requests_passed())
            }
            EvalFamily::Web => snapshot.web_natural_requests_passed(),
            EvalFamily::Exec => self.exec_natural_entries_match(),
        }
    }

    pub(crate) fn natural_execution_completed(
        &self,
        snapshot: &CaseSnapshot,
        tracker: &OperationTracker,
    ) -> EvalResult<bool> {
        match self.family {
            EvalFamily::Workspace => {
                Ok(workspace_natural_result_payloads_passed(snapshot, tracker)
                    && tracker.final_response_reports_completion_with_file_creation())
            }
            EvalFamily::Web => Ok(web_natural_result_payloads_passed(snapshot, tracker)
                && tracker.final_response_reports(WEB_FETCH_BODY)),
            EvalFamily::Git => Ok(git_natural_result_payloads_passed(
                self.workspace.path(),
                snapshot,
                tracker,
            )? && tracker.final_response_reports_completion()),
            EvalFamily::Exec => Ok(tracker
                .final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))),
        }
    }
}

pub(crate) fn seed_exec_workspace(root: &Path) -> EvalResult {
    fs::create_dir(root.join("src"))?;
    fs::create_dir(root.join(".cargo"))?;
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"tool-eval-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[lib]\ntest = false\ndoctest = false\n",
    )?;
    fs::write(
        root.join("src/lib.rs"),
        "#[deprecated(note = \"tool eval fixture diagnostic\")]\nfn old_fixture() {}\n\npub fn fixture() { old_fixture(); }\n",
    )?;
    fs::write(root.join(".cargo/config.toml"), "[net]\noffline = true\n")?;
    fs::write(
        root.join("Cargo.lock"),
        "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 3\n\n[[package]]\nname = \"tool-eval-fixture\"\nversion = \"0.0.0\"\n",
    )?;
    Ok(())
}

pub(crate) fn filesystem_extended_attributes(
    root: &Path,
    ignored_root_entry: Option<&Path>,
) -> EvalResult<BTreeMap<PathBuf, ExtendedAttributeSnapshot>> {
    filesystem_entries(root, ignored_root_entry)?
        .into_keys()
        .map(|relative| {
            let attributes = extended_attributes(&root.join(&relative))?;
            Ok((relative, attributes))
        })
        .collect()
}

#[cfg(unix)]
pub(crate) fn extended_attributes(path: &Path) -> EvalResult<ExtendedAttributeSnapshot> {
    let mut names = Vec::new();
    let required = rustix::fs::llistxattr(path, &mut names)?;
    names.resize(required, 0);
    let written = rustix::fs::llistxattr(path, &mut names)?;
    if written != required {
        return Err(io::Error::other("extended-attribute names changed during capture").into());
    }
    names.truncate(written);
    names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| {
            let name = OsStr::from_bytes(name);
            let mut value = Vec::new();
            let required = rustix::fs::lgetxattr(path, name, &mut value)?;
            value.resize(required, 0);
            let written = rustix::fs::lgetxattr(path, name, &mut value)?;
            if written != required {
                return Err(
                    io::Error::other("extended-attribute value changed during capture").into(),
                );
            }
            value.truncate(written);
            Ok((name.as_bytes().to_vec(), value))
        })
        .collect()
}

#[cfg(not(unix))]
pub(crate) fn extended_attributes(_path: &Path) -> EvalResult<ExtendedAttributeSnapshot> {
    Ok(BTreeMap::new())
}

pub(crate) fn filesystem_entry_identities(
    root: &Path,
    ignored_root_entry: Option<&Path>,
) -> EvalResult<BTreeMap<PathBuf, FilesystemIdentity>> {
    #[cfg(unix)]
    return filesystem_entries(root, ignored_root_entry)?
        .into_iter()
        .filter_map(|(relative, snapshot)| {
            matches!(
                snapshot,
                WorkspaceEntrySnapshot::Directory { .. } | WorkspaceEntrySnapshot::File { .. }
            )
            .then_some(relative)
        })
        .map(|relative| {
            let metadata = fs::metadata(root.join(&relative))?;
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
    #[cfg(not(unix))]
    {
        let _ = (root, ignored_root_entry);
        Ok(BTreeMap::new())
    }
}

pub(crate) fn filesystem_modified_times(
    root: &Path,
    ignored_root_entry: Option<&Path>,
) -> EvalResult<BTreeMap<PathBuf, SystemTime>> {
    filesystem_entries(root, ignored_root_entry)?
        .into_keys()
        .map(|relative| {
            let modified = fs::symlink_metadata(root.join(&relative))?.modified()?;
            Ok((relative, modified))
        })
        .collect()
}

pub(crate) fn worktree_mode(path: &Path) -> EvalResult<Option<u32>> {
    #[cfg(unix)]
    return Ok(Some(fs::metadata(path)?.permissions().mode() & 0o7777));
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

pub(crate) fn worktree_link_count(path: &Path) -> EvalResult<Option<u64>> {
    #[cfg(unix)]
    return Ok(Some(fs::metadata(path)?.nlink()));
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

pub(crate) fn inode_flag_snapshots_match_for_mutation(
    actual: BTreeMap<PathBuf, u32>,
    expected: &BTreeMap<PathBuf, u32>,
    target: &Path,
    creation_reference: &Path,
) -> bool {
    if expected.is_empty() {
        return actual.is_empty();
    }
    let mut expected = expected.clone();
    if !expected.contains_key(target) {
        let Some(default_flags) = expected.get(creation_reference).copied() else {
            return false;
        };
        expected.insert(target.to_path_buf(), default_flags);
    }
    actual == expected
}

pub(crate) fn entry_identities_match_except(
    mut actual: BTreeMap<PathBuf, FilesystemIdentity>,
    expected: &BTreeMap<PathBuf, FilesystemIdentity>,
    allowed_paths: &[&Path],
) -> bool {
    let mut expected = expected.clone();
    for path in allowed_paths {
        let expected_ownership = expected.get(*path).or_else(|| expected.get(Path::new("")));
        if !filesystem_ownership_matches(actual.get(*path), expected_ownership) {
            return false;
        }
        let mut ancestor = path.parent();
        while let Some(candidate) = ancestor {
            let Some(actual_identity) = actual.get(candidate) else {
                return false;
            };
            let Some(expected_identity) = expected.get_mut(candidate) else {
                return false;
            };
            admit_filesystem_change_time(expected_identity, *actual_identity);
            ancestor = candidate.parent();
        }
        actual.remove(*path);
        expected.remove(*path);
    }
    actual == expected
}

pub(crate) fn json_object_has_exact_fields(value: &serde_json::Value, expected: &[&str]) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == expected.len() && expected.iter().all(|field| object.contains_key(*field))
    })
}

pub(crate) fn normalized_arguments_text(arguments: &str) -> EvalResult<String> {
    NormalizedToolArguments::try_from_provider_text(arguments.to_owned())
        .map(|arguments| arguments.as_str().to_owned())
        .map_err(|_| io::Error::other("the eval fixture arguments do not normalize").into())
}

pub(crate) fn filesystem_file_and_directory_modified_times(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, SystemTime>> {
    filesystem_entries(root, None)?
        .into_iter()
        .filter_map(|(relative, snapshot)| {
            matches!(
                snapshot,
                WorkspaceEntrySnapshot::Directory { .. } | WorkspaceEntrySnapshot::File { .. }
            )
            .then_some(relative)
        })
        .map(|relative| {
            let modified = fs::symlink_metadata(root.join(&relative))?.modified()?;
            Ok((relative, modified))
        })
        .collect()
}

pub(crate) fn filesystem_ownership_matches(
    actual: Option<&FilesystemIdentity>,
    expected: Option<&FilesystemIdentity>,
) -> bool {
    match (actual, expected) {
        (Some(actual), Some(expected)) => {
            actual.user_id == expected.user_id && actual.group_id == expected.group_id
        }
        (Some(_), None) | (None, None) => true,
        _ => false,
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MergedCatalog {
    pub(crate) entries: BTreeMap<DomainToolName, MergedCatalogEntry>,
}

#[derive(Clone, Debug)]
pub(crate) struct MergedCatalogEntry {
    pub(crate) definition: ToolDefinition,
    pub(crate) catalog: CompiledToolCatalog,
}

impl MergedCatalog {
    pub(crate) fn try_new(
        catalogs: impl IntoIterator<Item = CompiledToolCatalog>,
    ) -> EvalResult<Self> {
        let mut entries = BTreeMap::new();
        for catalog in catalogs {
            for definition in catalog.definitions() {
                let name = definition.name().clone();
                if entries
                    .insert(
                        name,
                        MergedCatalogEntry {
                            definition,
                            catalog: catalog.clone(),
                        },
                    )
                    .is_some()
                {
                    return Err(io::Error::other("duplicate eval tool declaration").into());
                }
            }
        }
        Ok(Self { entries })
    }
}

impl ToolCatalog for MergedCatalog {
    fn definitions(&self) -> Box<[ToolDefinition]> {
        self.entries
            .values()
            .map(|entry| entry.definition.clone())
            .collect()
    }

    fn definition(&self, name: &DomainToolName) -> Option<ToolDefinition> {
        self.entries.get(name).map(|entry| entry.definition.clone())
    }

    fn validate_arguments(
        &self,
        name: &DomainToolName,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolCatalogValidationFailure> {
        self.entries
            .get(name)
            .ok_or(ToolCatalogValidationFailure::UnknownTool)?
            .catalog
            .validate_arguments(name, arguments)
    }
}

pub(crate) enum FamilyExecutor {
    Git(LocalGitExecutor<LocalWorkspaceFileSystem>),
    Workspace {
        read: WorkspaceReadExecutor<LocalWorkspaceFileSystem>,
        mutation: WorkspaceMutationExecutor<LocalWorkspaceFileSystem>,
    },
    Web {
        fetch: WebFetchExecutor<FixtureWebFetchTransport>,
        search: WebSearchExecutor<FixtureWebCredential, FixtureWebSearchTransport>,
    },
    Exec {
        sandboxed: ExecExecutor<SandboxedCommandRunner<TokioProcessRunner>>,
        unsandboxed: ExecExecutor<UnsandboxedCommandRunner<TokioProcessRunner>>,
        diagnostics: CargoDiagnosticsExecutor<TokioProcessRunner>,
        case: ExecEvalCase,
    },
}

/// The one forced fixture an Exec case dispatches and reports against.
pub(crate) fn forced_exec_fixture(name: &'static str) -> ExecFixtureCall {
    let case = EXEC_CASES
        .iter()
        .find(|case| case.name == name)
        .expect("every Exec eval case names a forced fixture");
    ExecFixtureCall {
        name: case.name,
        expected_arguments: case.expected_arguments,
    }
}

pub(crate) fn add_eval_receipt(
    evidence: ToolExecutorEvidence,
    receipt: &str,
) -> io::Result<ToolExecutorEvidence> {
    let ToolExecutorEvidence::CompletedText(content) = evidence else {
        return Ok(evidence);
    };
    let mut result: serde_json::Value = serde_json::from_str(&content)
        .map_err(|_| io::Error::other("the eval tool returned non-JSON success content"))?;
    let fields = result
        .as_object_mut()
        .ok_or_else(|| io::Error::other("the eval tool returned non-object success content"))?;
    if fields.contains_key(EVAL_RECEIPT_FIELD) {
        return Err(io::Error::other(
            "the eval tool returned the reserved eval receipt field",
        ));
    }
    fields.insert(
        String::from(EVAL_RECEIPT_FIELD),
        serde_json::Value::String(receipt.to_owned()),
    );
    serde_json::to_string(&result)
        .map(ToolExecutorEvidence::CompletedText)
        .map_err(|_| io::Error::other("the eval receipt could not be encoded"))
}

pub(crate) struct EnvironmentCredential;

impl CredentialAccess for EnvironmentCredential {
    async fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        assert_eq!(reference.as_str(), EXPECTED_OPENAI_CREDENTIAL_REFERENCE);
        match std::env::var(API_KEY_VARIABLE) {
            Ok(value) if !value.is_empty() => Ok(CredentialValue::new(value.into_bytes())),
            _ => Err(CredentialAccessError::new(
                reference.clone(),
                CredentialAccessFailure::Unavailable,
            )),
        }
    }
}

pub(crate) struct EvalOpenAiRuntime {
    pub(crate) inner: OpenAiRuntime<EnvironmentCredential>,
    pub(crate) forced: ForcedToolSequence,
    pub(crate) tracker: OperationTracker,
}

impl EvalOpenAiRuntime {
    pub(crate) fn new(forced_tool: Option<&str>, tracker: OperationTracker) -> EvalResult<Self> {
        let mut config = OpenAiConfig::new(None);
        config.exchange_timeout = Some(EXCHANGE_TIMEOUT);
        Ok(Self {
            inner: OpenAiRuntime::new(config, EnvironmentCredential)?,
            forced: ForcedToolSequence::new(forced_tool),
            tracker,
        })
    }
}

impl ModelRuntime<ModelCallId> for EvalOpenAiRuntime {
    type Prepared = OpenAiPreparedRequest<ModelCallId>;

    async fn prepare(
        &self,
        mut operation: ModelOperation<ModelCallId>,
        cancellation: CancellationSignal,
    ) -> PreparationOutcome<ModelCallId, Self::Prepared> {
        self.tracker.observe(&operation);
        match self.forced.next() {
            ForcedToolOperation::Natural => {}
            ForcedToolOperation::Force(name) => operation.tool_choice = ToolChoice::Named(name),
            ForcedToolOperation::Continuation => {
                operation.tools.clear();
                operation.tool_choice = ToolChoice::Automatic;
            }
        }
        self.inner.prepare(operation, cancellation).await
    }

    async fn execute(
        &self,
        prepared: Self::Prepared,
        sink: &mut (dyn ObservationSink<ModelCallId> + Send),
        cancellation: CancellationSignal,
    ) -> TerminalReport<ModelCallId> {
        let mut tracking_sink = ReceiptTrackingSink::new(sink);
        let report = self
            .inner
            .execute(prepared, &mut tracking_sink, cancellation)
            .await;
        self.tracker.observe_response_text(
            &tracking_sink.response_text,
            tracking_sink.proposed_tool_call,
        );
        report
    }
}

pub(crate) struct ReceiptTrackingSink<'a> {
    pub(crate) inner: &'a mut (dyn ObservationSink<ModelCallId> + Send),
    pub(crate) response_text: String,
    pub(crate) proposed_tool_call: bool,
}

impl<'a> ReceiptTrackingSink<'a> {
    pub(crate) fn new(inner: &'a mut (dyn ObservationSink<ModelCallId> + Send)) -> Self {
        Self {
            inner,
            response_text: String::new(),
            proposed_tool_call: false,
        }
    }
}

impl ObservationSink<ModelCallId> for ReceiptTrackingSink<'_> {
    fn observe(&mut self, observation: Observation<ModelCallId>) {
        if let ObservationFact::TextDelta { text, .. } = &observation.fact {
            self.response_text.push_str(text);
        }
        if matches!(&observation.fact, ObservationFact::ToolCallProposed(_)) {
            self.proposed_tool_call = true;
        }
        self.inner.observe(observation);
    }
}

#[derive(Clone, Default)]
pub(crate) struct OperationTracker {
    pub(crate) state: Arc<StdMutex<OperationTrackerState>>,
}

#[derive(Default)]
pub(crate) struct OperationTrackerState {
    pub(crate) seen_tool_call_ids: BTreeSet<String>,
    pub(crate) tool_results: Vec<TrackedToolResult>,
    pub(crate) result_round_trips: usize,
    pub(crate) round_tripped_request_ids: BTreeSet<Uuid>,
    pub(crate) pending_result_receipts: BTreeMap<Uuid, String>,
    pub(crate) result_contents: BTreeMap<Uuid, String>,
    pub(crate) final_response_text: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrackedToolResult {
    pub(crate) request_id: Uuid,
    pub(crate) content: String,
    pub(crate) is_error: bool,
    pub(crate) round_tripped: bool,
}

impl OperationTracker {
    pub(crate) fn observe(&self, operation: &ModelOperation<ModelCallId>) {
        let tool_results = operation.messages.iter().flat_map(|message| {
            message.parts.iter().filter_map(|part| match part {
                MessagePart::ToolResult(result) => Uuid::parse_str(result.tool_call_id.as_str())
                    .ok()
                    .map(|request_id| {
                        (
                            String::from(result.tool_call_id.as_str()),
                            TrackedToolResult {
                                request_id,
                                content: result.content.clone(),
                                is_error: result.is_error,
                                round_tripped: false,
                            },
                        )
                    }),
                MessagePart::Text(_)
                | MessagePart::ToolCall(_)
                | MessagePart::Thinking { .. }
                | MessagePart::RedactedThinking { .. }
                | MessagePart::ProviderCompaction { .. }
                | MessagePart::ProviderReasoning { .. } => None,
            })
        });
        self.record_new_results(tool_results);
    }

    pub(crate) fn record_new_results(
        &self,
        tool_results: impl IntoIterator<Item = (String, TrackedToolResult)>,
    ) {
        let mut state = self
            .state
            .lock()
            .expect("operation-tracker lock is available");
        for (tool_call_id, result) in tool_results {
            if state.seen_tool_call_ids.insert(tool_call_id) {
                if let Some(receipt) = eval_receipt(&result.content) {
                    state.record_result(result.request_id, receipt, &result.content);
                }
                state.tool_results.push(result);
            }
        }
    }

    pub(crate) fn observe_result(&self, request_id: Uuid, content: &str) {
        let Some(receipt) = eval_receipt(content) else {
            return;
        };
        self.state
            .lock()
            .expect("operation-tracker lock is available")
            .record_result(request_id, receipt, content);
    }

    pub(crate) fn observe_response_text(&self, text: &str, proposed_tool_call: bool) {
        if proposed_tool_call {
            return;
        }
        let mut state = self
            .state
            .lock()
            .expect("operation-tracker lock is available");
        state.final_response_text = Some(text.to_owned());
        let reported = state
            .pending_result_receipts
            .iter()
            .filter_map(|(request, receipt)| text.contains(receipt).then_some(*request))
            .collect::<Vec<_>>();
        if reported.is_empty() {
            return;
        }
        state.result_round_trips += 1;
        for request in reported {
            state.pending_result_receipts.remove(&request);
            state.round_tripped_request_ids.insert(request);
            if let Some(result) = state
                .tool_results
                .iter_mut()
                .find(|result| result.request_id == request)
            {
                result.round_tripped = true;
            }
        }
    }

    pub(crate) fn tool_results(&self) -> Vec<TrackedToolResult> {
        self.state
            .lock()
            .expect("operation-tracker lock is available")
            .tool_results
            .clone()
    }

    pub(crate) fn result_round_trips(&self) -> usize {
        self.state
            .lock()
            .expect("operation-tracker lock is available")
            .result_round_trips
    }

    pub(crate) fn round_tripped_request_ids(&self) -> BTreeSet<Uuid> {
        self.state
            .lock()
            .expect("operation-tracker lock is available")
            .round_tripped_request_ids
            .clone()
    }

    pub(crate) fn result_content(&self, request_id: Uuid) -> Option<String> {
        self.state
            .lock()
            .expect("operation-tracker lock is available")
            .result_contents
            .get(&request_id)
            .cloned()
    }

    pub(crate) fn final_response_reports(&self, expected: &str) -> bool {
        self.state
            .lock()
            .expect("operation-tracker lock is available")
            .final_response_text
            .as_deref()
            .is_some_and(|text| text.contains(expected) && !report_denies_success(text, false))
    }

    pub(crate) fn final_response_reports_completion(&self) -> bool {
        self.final_response_reports_completion_with_required_file_effect(RequiredFileEffect::None)
    }

    pub(crate) fn final_response_reports_completion_with_file_creation(&self) -> bool {
        self.final_response_reports_completion_with_required_file_effect(RequiredFileEffect::Create)
    }

    pub(crate) fn final_response_reports_completion_with_file_mutation(&self) -> bool {
        self.final_response_reports_completion_with_required_file_effect(RequiredFileEffect::Mutate)
    }

    pub(crate) fn final_response_reports_completion_with_required_file_effect(
        &self,
        required_effect: RequiredFileEffect,
    ) -> bool {
        let state = self
            .state
            .lock()
            .expect("operation-tracker lock is available");
        let Some(mut report) = state.final_response_text.clone() else {
            return false;
        };
        for content in state.result_contents.values() {
            if let Some(receipt) = eval_receipt(content) {
                report = report.replace(&receipt, "");
            }
        }
        let file_creation_required = required_effect == RequiredFileEffect::Create;
        let file_mutation_required = required_effect != RequiredFileEffect::None;
        report_affirms_completion(&report, file_creation_required)
            && (!file_mutation_required || !report_denies_file_changes(&report))
    }

    pub(crate) fn final_response_reports_file_creation(&self) -> bool {
        self.final_response_reports_completion_with_file_creation()
            && self
                .state
                .lock()
                .expect("operation-tracker lock is available")
                .final_response_text
                .as_deref()
                .is_some_and(|report| !report_denies_file_changes(report))
    }

    pub(crate) fn final_response_reports_file_creation_excepting_path(&self, path: &Path) -> bool {
        let state = self
            .state
            .lock()
            .expect("operation-tracker lock is available");
        let Some(mut report) = state.final_response_text.clone() else {
            return false;
        };
        for content in state.result_contents.values() {
            if let Some(receipt) = eval_receipt(content) {
                report = report.replace(&receipt, "");
            }
        }
        report_affirms_completion_excepting_path(&report, path)
            && !report_denies_file_changes_excepting_path(&report, path)
    }

    pub(crate) fn final_response_reports_case_outcome(&self, case_name: &str) -> bool {
        self.state
            .lock()
            .expect("operation-tracker lock is available")
            .final_response_text
            .as_deref()
            .is_some_and(|report| report_affirms_case_outcome(report, case_name))
    }

    pub(crate) fn final_response_denies_exec_output(&self) -> bool {
        self.state
            .lock()
            .expect("operation-tracker lock is available")
            .final_response_text
            .as_deref()
            .is_some_and(report_denies_exec_output)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequiredFileEffect {
    None,
    Create,
    Mutate,
}

pub(crate) fn report_denies_exec_output(report: &str) -> bool {
    normalized_report_clauses(report).into_iter().any(|clause| {
        let denies_all_output = clause.windows(2).enumerate().any(|(index, claim)| {
            let stream_scope = &clause[index.saturating_sub(4)..index];
            let names_stderr = stream_scope.iter().any(|word| word == "stderr")
                || stream_scope
                    .windows(2)
                    .any(|words| words[0] == "standard" && words[1] == "error");
            claim[0] == "no" && claim[1] == "output" && !names_stderr
        });
        let denies_stdout = clause.iter().enumerate().any(|(index, word)| {
            let stream_scope = &clause[index.saturating_sub(5)..index];
            let names_stdout = stream_scope.iter().any(|word| word == "stdout")
                || stream_scope
                    .windows(2)
                    .any(|words| words[0] == "standard" && words[1] == "output");
            names_stdout
                && matches!(
                    word.as_str(),
                    "empty" | "incorrect" | "mismatch" | "mismatched" | "wrong"
                )
                && !failure_term_is_negated(&clause, index)
        });
        denies_all_output || denies_stdout
    })
}

impl OperationTrackerState {
    pub(crate) fn record_result(&mut self, request_id: Uuid, receipt: String, content: &str) {
        self.pending_result_receipts.insert(request_id, receipt);
        self.result_contents.insert(request_id, content.to_owned());
    }
}

pub(crate) fn eval_receipt(content: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(content)
        .ok()?
        .as_object()?
        .get(EVAL_RECEIPT_FIELD)?
        .as_str()
        .map(str::to_owned)
}

pub(crate) struct EvalDatabase {
    pub(crate) _container: ContainerAsync<Postgres>,
    pub(crate) pool: PgPool,
    pub(crate) selection: DirectModelSelection,
    pub(crate) targets: ModelTargetCatalog,
    pub(crate) credential_families: ModelCredentialFamilyCatalog,
    pub(crate) runtime_models: RuntimeModelCatalog,
}

impl EvalDatabase {
    pub(crate) async fn start(model: &str) -> EvalResult<Self> {
        let container = Postgres::default()
            .with_db_name(DATABASE_NAME)
            .with_user(DATABASE_USER)
            .with_password(DATABASE_PASSWORD)
            .with_cmd(disposable_postgres_server_args())
            .with_mount(disposable_postgres_state_tmpfs_from_example()?)
            .with_tag(POSTGRES_IMAGE_TAG)
            .with_labels(disposable_test_container_labels())
            .start()
            .await?;
        let host = container.get_host().await?;
        let port = container.get_host_port_ipv4(POSTGRES_PORT).await?;
        let database_url =
            format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
        let pool = PgPoolOptions::new()
            .max_connections(POSTGRES_POOL_CONNECTIONS)
            .connect_with(local_test_connection_options(&database_url)?)
            .await?;
        migrate(&pool).await?;
        let selection =
            DirectModelSelection::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_SELECTION_ID));
        let target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
            Uuid::from_u128(ARBITRARY_EVAL_PROVIDER_ID),
        ));
        let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
            selection, target,
        )])
        .map_err(|_| io::Error::other("the eval model target is duplicated"))?;
        let credential_families = ModelCredentialFamilyCatalog::try_new([(
            target,
            Arc::<str>::from(OPENAI_MODEL_FAMILY),
            None,
        )])
        .map_err(|_| io::Error::other("the eval credential family is duplicated"))?;
        let runtime_models =
            RuntimeModelCatalog::try_from_definitions([RuntimeModelDefinition::try_new(
                target,
                String::from(model),
                MAX_OUTPUT_TOKENS,
                CONTEXT_WINDOW_TOKENS,
            )?])?;
        Ok(Self {
            _container: container,
            pool,
            selection,
            targets,
            credential_families,
            runtime_models,
        })
    }

    pub(crate) async fn start_turn(
        &self,
        prompt: &str,
    ) -> EvalResult<(SessionId, TurnId, signalbox_domain::ActivatedTurn)> {
        let defaults = SessionConfigurationDefaults::with_dangerous_tool_auto_approval(
            ModelSelectionRequest::Direct(self.selection),
            DangerousToolAutoApproval::ApproveAll,
        );
        let mut create = CreateSessionService::new(
            UuidV7SessionIdGenerator,
            signalbox_persistence::create_session::CreateSessionRepository::new(
                self.pool.clone(),
                eval_session_credential_pin(),
            ),
        );
        let CreateSessionOutcome::Applied(created) = create
            .execute(CreateSessionRequest::try_new(
                DurableCommandId::from_uuid(Uuid::now_v7()),
                defaults,
            )?)
            .await?
        else {
            return Err(io::Error::other("eval session creation was not applied").into());
        };
        let session = created.session();
        let sweep = PostgresEligibilitySweep::new(self.pool.clone());
        let (nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
        let mut submit = SubmitInputService::new(
            UuidV7SubmitInputIdGenerator,
            SubmitInputRepository::new(self.pool.clone()),
            nudge,
            InProcessToolDispatchGate::default(),
        );
        let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(origin),
        )) = submit
            .execute(SubmitInputRequest::try_new(
                DurableCommandId::from_uuid(Uuid::now_v7()),
                session,
                UserContent::try_text(prompt.to_owned())
                    .map_err(|_| io::Error::other("the eval prompt is invalid"))?,
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: default_configuration(),
                },
            )?)
            .await?
        else {
            return Err(io::Error::other("eval input did not create a turn").into());
        };
        let turn = origin.turn();
        let mut start = StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(self.pool.clone()),
        );
        let StartEligibleTurnOutcome::Activated(activated) = start.execute(session).await? else {
            return Err(io::Error::other("eval turn did not activate").into());
        };
        Ok((session, turn, *activated))
    }

    pub(crate) async fn decide_pending_unsandboxed_requests(
        &self,
        session: SessionId,
        turn: TurnId,
        approval_state: &mut ExecApprovalState,
    ) -> EvalResult<bool> {
        let mut decided_any = false;
        loop {
            let repository = PostgresToolLoopRepository::new(self.pool.clone());
            let Some(batch) = repository.load_active_batch(session, turn).await? else {
                return Ok(decided_any);
            };
            let ToolBatchPhase::AwaitingApproval { request } = batch.phase() else {
                return Ok(decided_any);
            };
            let pending = batch
                .requests()
                .iter()
                .find(|candidate| candidate.id() == request)
                .ok_or_else(|| io::Error::other("the pending approval request is absent"))?;
            let decision = approval_state.decision(pending.name().as_str(), pending.arguments());
            let mut service = DecideToolRequestService::new(UuidV7ToolLoopIdGenerator, repository);
            let prepared = service
                .execute(
                    DecideToolRequest::try_new(
                        DurableCommandId::from_uuid(Uuid::now_v7()),
                        request,
                        decision,
                    )
                    .map_err(|_| io::Error::other("the exec eval approval decision is invalid"))?,
                )
                .await?;
            if !matches!(prepared.result(), DecideToolRequestResult::Applied(_)) {
                return Err(
                    io::Error::other("the exec eval approval decision was rejected").into(),
                );
            }
            decided_any = true;
        }
    }
}

pub(crate) fn eval_session_credential_pin() -> SessionCredentialPin {
    SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        OPENAI_MODEL_FAMILY,
        EXPECTED_OPENAI_CREDENTIAL_REFERENCE,
    )])
    .expect("the eval credential pin is valid")
}

pub(crate) const fn default_configuration() -> PerInputConfigurationChoices {
    PerInputConfigurationChoices::new(
        SessionConfigurationDefaultsVersion::first(),
        ModelSelectionOverride::UseSessionDefault,
    )
}

pub(crate) struct CaseSnapshot {
    pub(crate) turn_disposition: SnapshotTurnDisposition,
    pub(crate) requests: Vec<RequestSnapshot>,
    pub(crate) model_calls: i64,
}

pub(crate) struct RequestSnapshot {
    pub(crate) request_id: Uuid,
    pub(crate) producing_model_call_id: Uuid,
    pub(crate) entry_index: u64,
    pub(crate) completed_result_entry_index: Option<u64>,
    pub(crate) name: String,
    pub(crate) arguments_text: String,
    pub(crate) attempt_succeeded: bool,
    pub(crate) attempt_denied: bool,
}

impl RequestSnapshot {
    pub(crate) fn arguments(&self) -> Option<serde_json::Value> {
        serde_json::from_str(&self.arguments_text).ok()
    }
}

impl CaseSnapshot {
    pub(crate) async fn read(
        pool: &PgPool,
        session: SessionId,
        turn: TurnId,
        approval_cap: ExecApprovalCap,
    ) -> EvalResult<Self> {
        let transcript = ProcessReadRepository::new(pool.clone())
            .read_transcript(session)
            .await?
            .ok_or_else(|| io::Error::other("the eval transcript session is missing"))?;
        let turn_state = transcript
            .turns()
            .iter()
            .find(|candidate| candidate.turn() == turn)
            .ok_or_else(|| io::Error::other("the eval transcript turn is missing"))?
            .state();
        let turn_disposition = match approval_cap {
            ExecApprovalCap::Reached
                if matches!(turn_state, ProcessTurnState::ActiveRunning { .. }) =>
            {
                SnapshotTurnDisposition::ApprovalCapReached
            }
            ExecApprovalCap::NotReached | ExecApprovalCap::Reached => {
                SnapshotTurnDisposition::from_process_state(turn_state)
            }
        };
        let completed_results = completed_tool_result_entry_indices(transcript.entries());
        let successful_requests = completed_results.keys().copied().collect::<BTreeSet<_>>();
        let denied_requests = transcript
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                ProcessTranscriptEntry::ToolDenied { request, .. } => Some(request.into_uuid()),
                ProcessTranscriptEntry::AssistantToolUse { .. }
                | ProcessTranscriptEntry::DelegatedTask { .. }
                | ProcessTranscriptEntry::DelegationMessage { .. }
                | ProcessTranscriptEntry::DelegationResult { .. }
                | ProcessTranscriptEntry::ModelIdentityChanged { .. }
                | ProcessTranscriptEntry::ContextSummary { .. }
                | ProcessTranscriptEntry::User { .. }
                | ProcessTranscriptEntry::Assistant { .. }
                | ProcessTranscriptEntry::ProviderCompaction { .. }
                | ProcessTranscriptEntry::ProviderReasoning { .. }
                | ProcessTranscriptEntry::ToolExecutionResult { .. }
                | ProcessTranscriptEntry::ToolClosed { .. }
                | ProcessTranscriptEntry::TurnFailed { .. }
                | ProcessTranscriptEntry::TurnCompleted { .. }
                | ProcessTranscriptEntry::TurnCancelled { .. }
                | ProcessTranscriptEntry::ImportedText { .. }
                | ProcessTranscriptEntry::Imported { .. } => None,
            })
            .collect::<BTreeSet<_>>();
        let requests = transcript
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                ProcessTranscriptEntry::AssistantToolUse {
                    entry_index,
                    turn: request_turn,
                    model_call,
                    request,
                    name,
                    arguments,
                    ..
                } if *request_turn == turn => Some(RequestSnapshot {
                    request_id: request.into_uuid(),
                    producing_model_call_id: model_call.into_uuid(),
                    entry_index: *entry_index,
                    completed_result_entry_index: completed_results
                        .get(&request.into_uuid())
                        .copied(),
                    name: name.clone(),
                    arguments_text: arguments.clone(),
                    attempt_succeeded: successful_requests.contains(&request.into_uuid()),
                    attempt_denied: denied_requests.contains(&request.into_uuid()),
                }),
                ProcessTranscriptEntry::AssistantToolUse { .. }
                | ProcessTranscriptEntry::DelegatedTask { .. }
                | ProcessTranscriptEntry::DelegationMessage { .. }
                | ProcessTranscriptEntry::DelegationResult { .. }
                | ProcessTranscriptEntry::ModelIdentityChanged { .. }
                | ProcessTranscriptEntry::ContextSummary { .. }
                | ProcessTranscriptEntry::User { .. }
                | ProcessTranscriptEntry::Assistant { .. }
                | ProcessTranscriptEntry::ProviderCompaction { .. }
                | ProcessTranscriptEntry::ProviderReasoning { .. }
                | ProcessTranscriptEntry::ToolExecutionResult { .. }
                | ProcessTranscriptEntry::ToolDenied { .. }
                | ProcessTranscriptEntry::ToolClosed { .. }
                | ProcessTranscriptEntry::TurnFailed { .. }
                | ProcessTranscriptEntry::TurnCompleted { .. }
                | ProcessTranscriptEntry::TurnCancelled { .. }
                | ProcessTranscriptEntry::ImportedText { .. }
                | ProcessTranscriptEntry::Imported { .. } => None,
            })
            .collect();
        let model_calls = i64::try_from(
            transcript
                .model_call_usage()
                .iter()
                .filter(|usage| usage.turn() == turn)
                .count(),
        )
        .map_err(|_| io::Error::other("the eval model-call count fits in i64"))?;
        Ok(Self {
            turn_disposition,
            requests,
            model_calls,
        })
    }

    pub(crate) fn called_names(&self) -> String {
        if self.requests.is_empty() {
            return String::from("none");
        }
        self.requests
            .iter()
            .map(|request| request.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub(crate) fn workspace_natural_requests_passed(&self) -> bool {
        let mutation_requests = self
            .requests
            .iter()
            .filter(|request| {
                matches!(
                    request.name.as_str(),
                    WRITE_FILE_NAME | EDIT_FILE_NAME | APPLY_PATCH_NAME
                )
            })
            .collect::<Vec<_>>();
        let read = self.requests.iter().position(|request| {
            request.name == READ_FILE_NAME
                && request.attempt_succeeded
                && request
                    .arguments()
                    .is_some_and(|arguments| workspace_read_covers_seed(&arguments))
        });
        let write = self.requests.iter().position(|request| {
            request.name == WRITE_FILE_NAME
                && request.arguments().is_some_and(|arguments| {
                    arguments["path"] == WORKSPACE_ANSWER_PATH
                        && arguments["content"] == WORKSPACE_ANSWER
                })
        });
        mutation_requests.len() == 1
            && mutation_requests[0].name == WRITE_FILE_NAME
            && read.zip(write).is_some_and(|(read, write)| {
                read < write
                    && self.requests[read].producing_model_call_id
                        != self.requests[write].producing_model_call_id
                    && self.requests[read]
                        .completed_result_entry_index
                        .is_some_and(|result_entry_index| {
                            result_entry_index < self.requests[write].entry_index
                        })
            })
    }

    pub(crate) fn git_natural_requests_passed(&self) -> EvalResult<bool> {
        let expected_stage = normalized_arguments_text(
            &serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
        )?;
        let expected_commit = normalized_arguments_text(r#"{"message":"tool eval commit"}"#)?;
        let mutation_requests = self
            .requests
            .iter()
            .filter(|request| {
                matches!(
                    request.name.as_str(),
                    GIT_BRANCH_CREATE_NAME
                        | GIT_BRANCH_SWITCH_NAME
                        | GIT_STAGE_NAME
                        | GIT_CREATE_COMMIT_NAME
                )
            })
            .collect::<Vec<_>>();
        let stage = self.requests.iter().position(|request| {
            request.name == GIT_STAGE_NAME && request.arguments_text == expected_stage
        });
        let commit = self.requests.iter().position(|request| {
            request.name == GIT_CREATE_COMMIT_NAME && request.arguments_text == expected_commit
        });
        Ok(mutation_requests.len() == 2
            && mutation_requests[0].name == GIT_STAGE_NAME
            && mutation_requests[1].name == GIT_CREATE_COMMIT_NAME
            && stage.zip(commit).is_some_and(|(stage, commit)| {
                stage < commit
                    && self.requests[stage].producing_model_call_id
                        != self.requests[commit].producing_model_call_id
                    && self.requests[stage]
                        .completed_result_entry_index
                        .is_some_and(|result_entry_index| {
                            result_entry_index < self.requests[commit].entry_index
                        })
            }))
    }

    pub(crate) fn web_natural_request_pair(
        &self,
    ) -> EvalResult<Option<(&RequestSnapshot, &RequestSnapshot)>> {
        let expected_query =
            normalized_arguments_text(&serde_json::json!({"query": WEB_QUERY}).to_string())?;
        let expected_url =
            normalized_arguments_text(&serde_json::json!({"url": WEB_URL}).to_string())?;
        Ok(self
            .requests
            .iter()
            .enumerate()
            .find_map(|(search_index, search)| {
                (search.name == WEB_SEARCH_NAME && search.arguments_text == expected_query)
                    .then(|| {
                        self.requests
                            .iter()
                            .skip(search_index + 1)
                            .find(|fetch| {
                                fetch.name == WEB_FETCH_NAME
                                    && fetch.arguments_text == expected_url
                                    && search.producing_model_call_id
                                        != fetch.producing_model_call_id
                                    && search.completed_result_entry_index.is_some_and(
                                        |result_entry_index| result_entry_index < fetch.entry_index,
                                    )
                            })
                            .map(|fetch| (search, fetch))
                    })
                    .flatten()
            }))
    }

    pub(crate) fn web_natural_requests_passed(&self) -> EvalResult<bool> {
        Ok(self.web_natural_request_pair()?.is_some())
    }

    pub(crate) fn exact_natural_request_failed(&self, family: EvalFamily) -> bool {
        self.requests.iter().enumerate().any(|(index, request)| {
            !request.attempt_succeeded
                && match family {
                    EvalFamily::Git => self.exact_git_natural_request_failed(index, request),
                    EvalFamily::Workspace => {
                        (request.name == READ_FILE_NAME
                            && request
                                .arguments()
                                .is_some_and(|arguments| workspace_read_covers_seed(&arguments))
                            && !self.requests[..index].iter().any(|earlier| {
                                earlier.attempt_succeeded
                                    && workspace_mutation_could_alter_seed(earlier)
                            }))
                            || (request.name == WRITE_FILE_NAME
                                && request.arguments().is_some_and(|arguments| {
                                    arguments
                                        == serde_json::json!({
                                            "path": WORKSPACE_ANSWER_PATH,
                                            "content": WORKSPACE_ANSWER,
                                        })
                                }))
                    }
                    EvalFamily::Web => {
                        (request.name == WEB_SEARCH_NAME
                            && request.arguments().is_some_and(|arguments| {
                                arguments == serde_json::json!({"query": WEB_QUERY})
                            }))
                            || (request.name == WEB_FETCH_NAME
                                && request.arguments().is_some_and(|arguments| {
                                    arguments == serde_json::json!({"url": WEB_URL})
                                }))
                    }
                    EvalFamily::Exec => {
                        request.name == SANDBOXED_EXEC_NAME && exact_exec_natural_arguments(request)
                    }
                }
        })
    }

    pub(crate) fn exact_git_natural_request_failed(
        &self,
        request_index: usize,
        request: &RequestSnapshot,
    ) -> bool {
        if request.name == GIT_STAGE_NAME && exact_git_natural_stage_arguments(request) {
            return true;
        }
        request.name == GIT_CREATE_COMMIT_NAME
            && request.arguments().is_some_and(|arguments| {
                arguments == serde_json::json!({"message": GIT_NATURAL_MESSAGE})
            })
            && self.requests[..request_index].iter().any(|stage| {
                stage.attempt_succeeded
                    && stage.name == GIT_STAGE_NAME
                    && exact_git_natural_stage_arguments(stage)
                    && stage.producing_model_call_id != request.producing_model_call_id
            })
            && !self.requests[..request_index]
                .iter()
                .any(|commit| commit.attempt_succeeded && commit.name == GIT_CREATE_COMMIT_NAME)
    }
}

pub(crate) fn exact_git_natural_stage_arguments(request: &RequestSnapshot) -> bool {
    request
        .arguments()
        .is_some_and(|arguments| arguments == serde_json::json!({"paths": [GIT_NATURAL_PATH]}))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotTurnDisposition {
    Completed,
    /// The eval deliberately stopped after its bounded denied approvals while
    /// the daemon remained in the post-decision active-running state.
    ApprovalCapReached,
    /// The turn terminalized on a definitive provider failure, carrying the
    /// closed cause the daemon retained for it when one was recorded.
    ProviderFailure(Option<ProcessProviderModelCallFailureCause>),
    /// The exchange did not reach a model-behavior outcome and cannot be
    /// scored as a model miss.
    Infrastructure,
    Refused,
}

impl SnapshotTurnDisposition {
    pub(crate) fn from_process_state(state: &ProcessTurnState) -> Self {
        match state {
            ProcessTurnState::Completed { .. } => Self::Completed,
            ProcessTurnState::FailedCredentialPoolExhausted(_) => {
                Self::from_failed_model_call(None)
            }
            ProcessTurnState::Failed {
                terminal_model_call,
                ..
            } => Self::from_failed_model_call(
                terminal_model_call
                    .as_ref()
                    .map(|call| (call.disposition(), call.provider_failure_cause())),
            ),
            ProcessTurnState::Queued { .. }
            | ProcessTurnState::QueuedDelegated { .. }
            | ProcessTurnState::QueuedDelegationWake { .. }
            | ProcessTurnState::DelegationTerminated { .. }
            | ProcessTurnState::ActiveRunning { .. }
            | ProcessTurnState::ActiveAwaitingToolApproval { .. }
            | ProcessTurnState::ActiveAwaitingChild { .. }
            | ProcessTurnState::ActiveAwaitingModelCallRecovery { .. }
            | ProcessTurnState::ActiveAwaitingToolRecovery { .. }
            | ProcessTurnState::ActiveAwaitingRunnerRecovery { .. }
            | ProcessTurnState::Cancelled { .. }
            | ProcessTurnState::ReconciliationRequired { .. } => Self::Infrastructure,
            ProcessTurnState::Refused { .. } => Self::Refused,
        }
    }

    pub(crate) const fn from_failed_model_call(
        terminal_call: Option<(
            ProcessFailedModelCallDisposition,
            Option<ProcessProviderModelCallFailureCause>,
        )>,
    ) -> Self {
        match terminal_call {
            Some((ProcessFailedModelCallDisposition::KnownFailed, cause)) => {
                Self::ProviderFailure(cause)
            }
            Some((ProcessFailedModelCallDisposition::Cancelled, _)) => Self::Infrastructure,
            None => Self::Infrastructure,
        }
    }

    pub(crate) const fn is_completed(self) -> bool {
        match self {
            Self::Completed => true,
            Self::ApprovalCapReached
            | Self::ProviderFailure(_)
            | Self::Infrastructure
            | Self::Refused => false,
        }
    }

    pub(crate) const fn is_infrastructure(self) -> bool {
        match self {
            Self::ProviderFailure(_) | Self::Infrastructure => true,
            Self::Completed | Self::ApprovalCapReached | Self::Refused => false,
        }
    }

    /// Renders the turn cell, naming the closed provider cause when the daemon
    /// retained one so a paid run reports why the exchange never happened.
    pub(crate) fn label(self) -> String {
        match self {
            Self::Completed => String::from("completed"),
            Self::ApprovalCapReached => String::from("approval cap reached"),
            Self::ProviderFailure(None) => String::from("provider failure"),
            Self::ProviderFailure(Some(cause)) => {
                format!("provider failure: {}", provider_failure_cause_label(cause))
            }
            Self::Infrastructure => String::from("infrastructure recovery"),
            Self::Refused => String::from("refused"),
        }
    }
}

/// Names one closed provider-failure classification for the eval report.
pub(crate) const fn provider_failure_cause_label(
    cause: ProcessProviderModelCallFailureCause,
) -> &'static str {
    match cause {
        ProcessProviderModelCallFailureCause::CredentialRejected => "credential rejected",
        ProcessProviderModelCallFailureCause::PermissionDenied => "permission denied",
        ProcessProviderModelCallFailureCause::InvalidRequest => "invalid request",
        ProcessProviderModelCallFailureCause::TargetNotFound => "target not found",
        ProcessProviderModelCallFailureCause::RequestTooLarge => "request too large",
        ProcessProviderModelCallFailureCause::RateLimited => "rate limited",
        ProcessProviderModelCallFailureCause::QuotaExhausted => "quota exhausted",
        ProcessProviderModelCallFailureCause::Overloaded => "overloaded",
        ProcessProviderModelCallFailureCause::ProviderInternal => "provider internal",
        ProcessProviderModelCallFailureCause::Unrecognized => "unrecognized",
    }
}

pub(crate) struct CaseOutcome {
    pub(crate) target: Option<String>,
    pub(crate) expected_arguments: Option<String>,
    pub(crate) execution_completed: bool,
    pub(crate) forced_verification_failed: bool,
    pub(crate) tool_results: Vec<TrackedToolResult>,
    pub(crate) snapshot: CaseSnapshot,
}

pub(crate) fn exact_exec_natural_arguments(request: &RequestSnapshot) -> bool {
    let Some(arguments) = request.arguments() else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(EXEC_NATURAL_ARGUMENTS)
        .is_ok_and(|expected| arguments == expected)
}

impl CaseOutcome {
    pub(crate) fn round_tripped_result_count(&self) -> usize {
        round_tripped_result_count(&self.tool_results)
    }

    pub(crate) fn infrastructure_label(&self) -> &'static str {
        if let Some(label) = self
            .tool_results
            .iter()
            .find_map(exec_result_infrastructure_label)
        {
            return label;
        }
        if self.exact_forced_exec_result_mismatched() || self.exact_natural_exec_result_mismatched()
        {
            return "exact result mismatch";
        }
        if self.exact_forced_verification_failed() {
            return "exact state mismatch";
        }
        "—"
    }

    pub(crate) fn exact_forced_exec_result_mismatched(&self) -> bool {
        let Some(target) = self.target.as_deref() else {
            return false;
        };
        let Some(expected_arguments) = self.expected_arguments.as_deref() else {
            return false;
        };
        if !is_exec_tool(target) {
            return false;
        }
        self.snapshot.requests.iter().any(|request| {
            request.name == target
                && request.arguments_text == expected_arguments
                && request.attempt_succeeded
                && self
                    .tool_results
                    .iter()
                    .find(|result| result.request_id == request.request_id)
                    .is_none_or(|result| !tracked_exec_result_passed(target, result))
        })
    }

    pub(crate) fn exact_natural_exec_result_mismatched(&self) -> bool {
        self.snapshot.requests.iter().any(|request| {
            request.name == SANDBOXED_EXEC_NAME
                && exact_exec_natural_arguments(request)
                && request.attempt_succeeded
                && self
                    .tool_results
                    .iter()
                    .find(|result| result.request_id == request.request_id)
                    .is_none_or(|result| !tracked_natural_exec_result_passed(result))
        })
    }

    pub(crate) fn exact_natural_exec_state_mismatched(
        &self,
        natural_state: EvalDisposition,
    ) -> bool {
        natural_state != EvalDisposition::Pass
            && self.snapshot.requests.iter().any(|request| {
                request.name == SANDBOXED_EXEC_NAME
                    && exact_exec_natural_arguments(request)
                    && request.attempt_succeeded
                    && self
                        .tool_results
                        .iter()
                        .find(|result| result.request_id == request.request_id)
                        .is_some_and(tracked_natural_exec_result_passed)
            })
    }

    pub(crate) fn natural_infrastructure_label(
        &self,
        family: EvalFamily,
        natural_state: EvalDisposition,
    ) -> &'static str {
        if family == EvalFamily::Exec && self.exact_natural_exec_state_mismatched(natural_state) {
            return "exact state mismatch";
        }
        self.infrastructure_label()
    }

    pub(crate) fn exact_forced_executor_failed(&self) -> bool {
        let Some(target) = self.target.as_deref() else {
            return false;
        };
        let Some(expected_arguments) = self.expected_arguments.as_deref() else {
            return false;
        };
        let sole_exact_exec_request_denied = is_exec_tool(target)
            && matches!(
                self.snapshot.requests.as_slice(),
                [request]
                    if request.name == target
                        && request.arguments_text == expected_arguments
                        && request.attempt_denied
            );
        sole_exact_exec_request_denied
            || self.snapshot.requests.iter().any(|request| {
                request.name == target
                    && request.arguments_text == expected_arguments
                    && ((!request.attempt_succeeded && !request.attempt_denied)
                        || (is_exec_tool(target)
                            && self.tool_results.iter().any(|result| {
                                result.request_id == request.request_id
                                    && exec_result_is_infrastructure(result)
                            })))
            })
            || self.exact_forced_exec_result_mismatched()
            || self.exact_forced_verification_failed()
    }

    pub(crate) fn exact_forced_verification_failed(&self) -> bool {
        let Some(target) = self.target.as_deref() else {
            return false;
        };
        let Some(expected_arguments) = self.expected_arguments.as_deref() else {
            return false;
        };
        self.forced_verification_failed
            && is_exec_tool(target)
            && self.snapshot.requests.iter().any(|request| {
                request.name == target
                    && request.arguments_text == expected_arguments
                    && request.attempt_succeeded
                    && self
                        .tool_results
                        .iter()
                        .find(|result| result.request_id == request.request_id)
                        .is_some_and(|result| tracked_exec_result_passed(target, result))
            })
    }

    pub(crate) fn forced_disposition(&self) -> EvalDisposition {
        if self.snapshot.turn_disposition.is_infrastructure() {
            return EvalDisposition::Infrastructure;
        }
        let Some(target) = self.target.as_deref() else {
            return EvalDisposition::Miss;
        };
        let Some(expected_arguments) = self.expected_arguments.as_deref() else {
            return EvalDisposition::Miss;
        };
        if matches!(
            target,
            SANDBOXED_EXEC_NAME | UNSANDBOXED_EXEC_NAME | CARGO_DIAGNOSTICS_NAME
        ) && self.tool_results.iter().any(exec_result_is_infrastructure)
        {
            return EvalDisposition::Infrastructure;
        }
        if self.exact_forced_executor_failed() {
            return EvalDisposition::Infrastructure;
        }
        EvalDisposition::from_passed(
            self.execution_completed
                && self.snapshot.turn_disposition.is_completed()
                && self.snapshot.model_calls >= MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP
                && self.tool_results.iter().any(|result| {
                    result.request_id == self.snapshot.requests[0].request_id
                        && result.round_tripped
                })
                && self.forced_result_passed(target)
                && self.snapshot.requests.len() == 1
                && self.snapshot.requests[0].name == target
                && self.snapshot.requests[0].arguments_text == expected_arguments
                && self.snapshot.requests[0].attempt_succeeded,
        )
    }

    pub(crate) fn natural_loop_disposition(&self, family: EvalFamily) -> EvalDisposition {
        if self.snapshot.turn_disposition.is_infrastructure() {
            return EvalDisposition::Infrastructure;
        }
        if self.snapshot.exact_natural_request_failed(family) {
            return EvalDisposition::Infrastructure;
        }
        if family == EvalFamily::Exec && self.tool_results.iter().any(exec_result_is_infrastructure)
        {
            return EvalDisposition::Infrastructure;
        }
        if family == EvalFamily::Exec && self.exact_natural_exec_result_mismatched() {
            return EvalDisposition::Infrastructure;
        }
        if family == EvalFamily::Exec
            && self
                .snapshot
                .requests
                .iter()
                .filter(|request| request.name == UNSANDBOXED_EXEC_NAME)
                .count()
                > MAX_NATURAL_APPROVAL_CONTINUATIONS
        {
            return EvalDisposition::Miss;
        }
        let required_names: &[&str] = match family {
            EvalFamily::Git => &[GIT_STAGE_NAME, GIT_CREATE_COMMIT_NAME],
            EvalFamily::Workspace => &[READ_FILE_NAME, WRITE_FILE_NAME],
            EvalFamily::Web => &[WEB_SEARCH_NAME, WEB_FETCH_NAME],
            EvalFamily::Exec => &[SANDBOXED_EXEC_NAME],
        };
        EvalDisposition::from_passed(
            self.execution_completed
                && self.snapshot.turn_disposition.is_completed()
                && self.snapshot.model_calls <= MAX_NATURAL_MODEL_CALLS
                && !self.tool_results.is_empty()
                && self.snapshot.requests.iter().all(|request| {
                    self.tool_results.iter().any(|result| {
                        result.request_id == request.request_id && result.round_tripped
                    })
                })
                && required_names.iter().all(|required| {
                    self.snapshot
                        .requests
                        .iter()
                        .any(|request| request.name == *required)
                })
                && self
                    .snapshot
                    .requests
                    .iter()
                    .all(|request| request.attempt_succeeded)
                && (family != EvalFamily::Exec
                    || (self.snapshot.requests.len() == 1
                        && self.snapshot.requests[0].name == SANDBOXED_EXEC_NAME
                        && self.natural_exec_result_passed())),
        )
    }

    /// Whether the unforced Exec tier's sole result proves a confined process
    /// that actually ran to a zero exit.
    ///
    /// The executor returns completed evidence for a timeout, a nonzero exit,
    /// and a supervision failure alike, and the workspace file the task writes
    /// can predate any of them, so requiring only that some result exists would
    /// report a pass for a failed process.
    pub(crate) fn natural_exec_result_passed(&self) -> bool {
        let [result] = self.tool_results.as_slice() else {
            return false;
        };
        tracked_natural_exec_result_passed(result)
    }

    pub(crate) fn forced_result_passed(&self, target: &str) -> bool {
        if !matches!(
            target,
            SANDBOXED_EXEC_NAME | UNSANDBOXED_EXEC_NAME | CARGO_DIAGNOSTICS_NAME
        ) {
            return !self.tool_results.is_empty();
        }
        let [result] = self.tool_results.as_slice() else {
            return false;
        };
        if result.is_error {
            return false;
        }
        let Ok(result) = serde_json::from_str::<serde_json::Value>(&result.content) else {
            return false;
        };
        exec_forced_case_passed(target, &result)
    }
}

pub(crate) fn is_exec_tool(target: &str) -> bool {
    matches!(
        target,
        SANDBOXED_EXEC_NAME | UNSANDBOXED_EXEC_NAME | CARGO_DIAGNOSTICS_NAME
    )
}

pub(crate) fn tracked_exec_result_passed(target: &str, result: &TrackedToolResult) -> bool {
    if result.is_error {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&result.content)
        .is_ok_and(|result| exec_forced_case_passed(target, &result))
}

pub(crate) fn tracked_natural_exec_result_passed(result: &TrackedToolResult) -> bool {
    if result.is_error {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&result.content).is_ok_and(|execution| {
        direct_exec_result_passed(
            &execution,
            DirectExecExpectation {
                confinement: "filesystem_confined",
                stdout: EXEC_NATURAL_OUTPUT,
            },
        )
    })
}

pub(crate) fn reject_forced_executor_failures(outcomes: &[CaseOutcome]) -> EvalResult {
    if outcomes
        .iter()
        .any(CaseOutcome::exact_forced_executor_failed)
    {
        return Err(io::Error::other(EXACT_EXECUTOR_FAILURE).into());
    }
    Ok(())
}

pub(crate) fn reject_natural_executor_failure(
    outcome: &CaseOutcome,
    family: EvalFamily,
    natural_state: EvalDisposition,
) -> EvalResult {
    if outcome.snapshot.exact_natural_request_failed(family)
        || (family == EvalFamily::Exec
            && outcome
                .tool_results
                .iter()
                .any(exec_result_is_infrastructure))
        || (family == EvalFamily::Exec && outcome.exact_natural_exec_result_mismatched())
        || (family == EvalFamily::Exec
            && outcome.exact_natural_exec_state_mismatched(natural_state))
    {
        return Err(io::Error::other(EXACT_EXECUTOR_FAILURE).into());
    }
    Ok(())
}

pub(crate) fn successful_request(
    request_id: Uuid,
    name: &str,
    arguments: serde_json::Value,
) -> RequestSnapshot {
    RequestSnapshot {
        request_id,
        producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
        name: name.to_owned(),
        arguments_text: arguments.to_string(),
        entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
        completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
        attempt_succeeded: true,
        attempt_denied: false,
    }
}

pub(crate) fn failed_request_snapshot(name: &str, arguments: serde_json::Value) -> CaseSnapshot {
    CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![RequestSnapshot {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
            name: name.to_owned(),
            arguments_text: arguments.to_string(),
            entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
            completed_result_entry_index: None,
            attempt_succeeded: false,
            attempt_denied: false,
        }],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    }
}

/// One serialized confined execution that exited zero with the given output.
pub(crate) fn confined_exit(stdout: &str) -> serde_json::Value {
    zero_exit_with_confinement(ZeroExitEvidence {
        confinement: ExecutionConfinement::FilesystemConfined,
        stdout,
    })
}

pub(crate) fn successful_workspace_natural_snapshot() -> CaseSnapshot {
    CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(READ_FILE_NAME),
                arguments_text: serde_json::json!({"path": WORKSPACE_SEED_PATH}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(WRITE_FILE_NAME),
                arguments_text: serde_json::json!({
                    "content": WORKSPACE_ANSWER,
                    "path": WORKSPACE_ANSWER_PATH,
                })
                .to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    }
}

pub(crate) type PreparedExecWorkspace = (
    TempDir,
    BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    BTreeMap<PathBuf, SystemTime>,
    BTreeMap<PathBuf, FilesystemIdentity>,
);

pub(crate) fn prepared_exec_seed_workspace() -> EvalResult<PreparedExecWorkspace> {
    let workspace = tempfile::tempdir()?;
    seed_exec_workspace(workspace.path())?;
    let seed_entries = workspace_entries(workspace.path())?;
    let seed_modified_times = workspace_modified_times(workspace.path())?;
    let seed_entry_identities = workspace_entry_identities(workspace.path())?;
    Ok((
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
    ))
}

#[cfg(unix)]
pub(crate) fn replace_exec_seed_file_byte_identically(
    root: &Path,
    seed_modified_times: &BTreeMap<PathBuf, SystemTime>,
) -> EvalResult {
    let relative = Path::new("Cargo.toml");
    let target = root.join(relative);
    let replacement = root.join("replacement-fixture");
    let content = fs::read(&target)?;
    let permissions = fs::metadata(&target)?.permissions();
    let target_modified = *seed_modified_times
        .get(relative)
        .expect("the Exec fixture has a captured target modified time");
    let root_modified = *seed_modified_times
        .get(Path::new(""))
        .expect("the Exec fixture has a captured root modified time");
    fs::write(&replacement, content)?;
    fs::set_permissions(&replacement, permissions)?;
    fs::rename(&replacement, &target)?;
    fs::File::open(&target)?.set_times(fs::FileTimes::new().set_modified(target_modified))?;
    fs::File::open(root)?.set_times(fs::FileTimes::new().set_modified(root_modified))?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EvalDisposition {
    Pass,
    Miss,
    Infrastructure,
}

impl EvalDisposition {
    pub(crate) const fn from_passed(passed: bool) -> Self {
        if passed { Self::Pass } else { Self::Miss }
    }

    pub(crate) const fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Pass, Self::Pass) => Self::Pass,
            (Self::Pass, Self::Miss) | (Self::Miss, Self::Pass | Self::Miss) => Self::Miss,
            (Self::Infrastructure, Self::Pass | Self::Miss | Self::Infrastructure)
            | (Self::Pass | Self::Miss, Self::Infrastructure) => Self::Infrastructure,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Miss => "MISS",
            Self::Infrastructure => "INFRA",
        }
    }
}

pub(crate) fn write_report(report: &FamilyReport) -> EvalResult {
    let summary_path = std::env::var_os(SUMMARY_VARIABLE)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("the tool-eval summary path is missing"))?;
    let mut markdown = format!(
        "## {} daemon tool eval — `{}`\n\n### Forced tier\n\n| Tool | Result | Infrastructure | Calls observed | Tool result round-trips | Turn |\n| --- | --- | --- | --- | ---: | --- |\n",
        report.family.as_str(),
        report.family.model(),
    );
    for outcome in &report.forced {
        log_exec_infrastructure_evidence(outcome);
        let target = outcome.target.as_deref().unwrap_or("missing target");
        let result = outcome.forced_disposition().label();
        let turn = outcome.snapshot.turn_disposition.label();
        markdown.push_str(&format!(
            "| `{target}` | {result} | {} | {} | {} | `{turn}` |\n",
            outcome.infrastructure_label(),
            outcome.snapshot.called_names(),
            outcome.round_tripped_result_count(),
        ));
    }
    let natural = report
        .natural
        .natural_loop_disposition(report.family)
        .and(report.natural_state);
    log_exec_infrastructure_evidence(&report.natural);
    markdown.push_str(&format!(
        "\n### Unforced tier\n\n| Result | Infrastructure | Calls observed | Tool result round-trips | Task state | Turn |\n| --- | --- | --- | ---: | --- | --- |\n| {} | {} | {} | {} | {} | `{}` |\n\nModel outcomes are report-only; a model miss does not fail this workflow. An exact forced or natural executor failure or rejected model credential fails after this summary is written.\n",
        natural.label(),
        report
            .natural
            .natural_infrastructure_label(report.family, report.natural_state),
        report.natural.snapshot.called_names(),
        report.natural.round_tripped_result_count(),
        report.natural_state.label(),
        report.natural.snapshot.turn_disposition.label(),
    ));
    fs::write(summary_path, &markdown)?;
    print!("{markdown}");
    Ok(())
}

pub(crate) fn log_exec_infrastructure_evidence(outcome: &CaseOutcome) {
    for result in &outcome.tool_results {
        if exec_result_is_infrastructure(result) {
            eprintln!(
                "structured Exec infrastructure evidence: {}",
                result.content
            );
        }
    }
}
