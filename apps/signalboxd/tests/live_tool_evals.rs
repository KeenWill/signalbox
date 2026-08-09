//! Live model-in-the-loop evaluations for daemon tool families.
//!
//! Ignored by default: each selected family spends real OpenAI exchanges. CI
//! compiles this target before exposing `OPENAI_API_KEY`, then runs exactly one
//! family in an ephemeral workspace. Model behavior is report-only: forced and
//! unforced misses are written to the requested Markdown summary and never make
//! this test fail. Harness, persistence, or tool-executor defects still fail the
//! test so the report cannot silently claim that an evaluation ran.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone ignored eval uses explicit fixture expectations"
)]

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use git2::{
    BranchType, Delta, DiffOptions, IndexAddOption, ObjectType, Oid, Patch, Repository,
    RepositoryState, Signature, Status, Time,
};
use signalbox_application::{
    ClassifyOperatorFailure, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    CreateSessionOutcome, CreateSessionRequest, CreateSessionService, DecideToolRequestService,
    InProcessAttemptDispatchGate, InProcessEligibilityWorkSource, InProcessToolDispatchGate,
    ModelCallCredentialReference, OperatorFailureClass, StartEligibleTurnOutcome,
    StartEligibleTurnService, SubmitInputOutcome, SubmitInputRequest, SubmitInputService,
    ToolCatalog, ToolCatalogValidationFailure, ToolDefinition, ToolExecutionInvocation,
    ToolExecutor, ToolExecutorEvidence, UuidV7SessionIdGenerator,
    UuidV7StartEligibleTurnIdGenerator, UuidV7SubmitInputIdGenerator, UuidV7ToolLoopIdGenerator,
};
use signalbox_domain::{
    ContextFrontierId, DangerousToolAutoApproval, DecideToolRequest, DecideToolRequestResult,
    DeliveryRequest, DirectModelSelection, DurableCommandId, ModelCallId, ModelSelectionOverride,
    ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition, NormalizedToolArguments,
    PerInputConfigurationChoices, ProviderModelIdentity, ResolvedProviderTarget,
    SemanticTranscriptEntryId, SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
    SessionId, SubmitInputAppliedResult, SubmitInputResult, ToolApprovalDecision, ToolAttemptId,
    ToolBatchPhase, ToolName as DomainToolName, ToolRequestId, TurnAttemptId, TurnId, UserContent,
};
use signalbox_model_provider_runtime::{
    RuntimeModelCallProvider, RuntimeModelCatalog, RuntimeModelDefinition,
};
use signalbox_model_runtime::{
    CancellationSignal, CredentialAccess, CredentialAccessError, CredentialAccessFailure,
    CredentialReference, CredentialValue, MessagePart, ModelOperation, ModelRuntime, Observation,
    ObservationFact, ObservationSink, PreparationOutcome, TerminalReport, ToolChoice,
    ToolName as RuntimeToolName,
};
use signalbox_model_runtime_openai::{OpenAiConfig, OpenAiPreparedRequest, OpenAiRuntime};
use signalbox_persistence::{
    ModelCredentialFamilyCatalog, SessionCredentialPin, SessionModelCredential,
    local_test_connection_options, migrate,
    model_execution::PostgresModelCallRepository,
    process_read::{
        ProcessFailedModelCallDisposition, ProcessProviderModelCallFailureCause,
        ProcessReadRepository, ProcessToolExecutionResultDisposition, ProcessTranscriptEntry,
        ProcessTurnState,
    },
    scheduler::PostgresEligibilitySweep,
    start_eligible_turn::StartEligibleTurnRepository,
    submit_input::SubmitInputRepository,
    tool_loop::PostgresToolLoopRepository,
};
use signalbox_tools_exec::{
    BwrapAvailability, CARGO_DIAGNOSTICS_NAME, CaptureCompleteness, CargoDiagnostic,
    CargoDiagnosticRecords, CargoDiagnosticsCommand, CargoDiagnosticsExecution,
    CargoDiagnosticsExecutor, CargoDiagnosticsResult, CargoDiagnosticsStream, CargoDiagnosticsTool,
    CargoEvidenceProvenance, CargoTestOutcome, CargoTestRecords, CargoTestResult, ExecExecutor,
    ExecResult, ExecutionConfinement, OutputCapture, OutputEncoding, ProcessOutcome,
    ProcessSpawnFailure, ProcessSupervisionFailure, SANDBOXED_EXEC_NAME, SandboxedCommandRunner,
    SandboxedExecTool, TokioProcessRunner, UNSANDBOXED_EXEC_NAME, UnsandboxedCommandRunner,
    UnsandboxedExecTool,
};
use signalbox_tools_git::{
    GIT_BRANCH_CREATE_NAME, GIT_BRANCH_SWITCH_NAME, GIT_CREATE_COMMIT_NAME, GIT_DIFF_NAME,
    GIT_LOG_NAME, GIT_STAGE_NAME, GIT_STATUS_NAME, GitIdentity, LocalGitExecutor, LocalGitTools,
};
use signalbox_tools_web::{
    WEB_FETCH_NAME, WEB_SEARCH_NAME, WebFetchBodyCompleteness, WebFetchEgressPolicy,
    WebFetchExecutor, WebFetchRequest, WebFetchResponse, WebFetchTool, WebFetchTransport,
    WebFetchTransportFailure, WebSearchConfiguration, WebSearchExecutor, WebSearchPageCompleteness,
    WebSearchProvider, WebSearchRequest, WebSearchResponse, WebSearchResult, WebSearchResultFields,
    WebSearchTool, WebSearchTransport, WebSearchTransportFailure, WebSearchTransportOutcome,
};
use signalbox_tools_workspace::{
    APPLY_PATCH_NAME, EDIT_FILE_NAME, GLOB_FILES_NAME, LIST_DIRECTORY_NAME,
    LocalWorkspaceFileSystem, MAX_WORKSPACE_READ_BYTES, READ_FILE_NAME, ReadFileArguments,
    SEARCH_FILES_NAME, WRITE_FILE_NAME, WorkspaceMutationExecutor, WorkspaceMutationTools,
    WorkspaceReadExecutor, WorkspaceReadTools,
};
use signalboxd::{ActivatedTurnExecution, PostgresProviderModelExecution};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use tempfile::TempDir;
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use tokio::{sync::Mutex, time::timeout};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_live_tool_evals";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const POSTGRES_PORT: u16 = 5432;
const POSTGRES_POOL_CONNECTIONS: u32 = 8;
const API_KEY_VARIABLE: &str = "OPENAI_API_KEY";
const FAMILY_VARIABLE: &str = "SIGNALBOX_TOOL_EVAL_FAMILY";
const SUMMARY_VARIABLE: &str = "SIGNALBOX_TOOL_EVAL_SUMMARY";
const DEFAULT_MODEL: &str = "gpt-5-nano";
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
const MAX_OUTPUT_TOKENS: u32 = 16_384;
const MAX_NATURAL_APPROVAL_CONTINUATIONS: usize = 2;
const CONTEXT_WINDOW_TOKENS: u32 = 200_000;
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(2 * 60);
/// Three tool-enabled exchanges plus the final answer cover every accepted
/// natural path, with one minute for local persistence and dispatch.
const TURN_TIMEOUT: Duration = Duration::from_secs(4 * 2 * 60 + 60);
const MAX_NATURAL_TOOL_EXCHANGES: usize = 3;
const MAX_NATURAL_MODEL_CALLS: i64 = MAX_NATURAL_TOOL_EXCHANGES as i64 + 1;
const LIVE_EVAL_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;
const GIT_AUTHOR_NAME: &str = "Signalbox Tool Eval";
const GIT_AUTHOR_EMAIL: &str = "signalbox-tool-eval@example.test";
const SYNTHETIC_OTHER_GIT_AUTHOR_NAME: &str = "Synthetic Other Author";
const SYNTHETIC_OTHER_GIT_AUTHOR_EMAIL: &str = "other-author@example.test";
const SYNTHETIC_GIT_CONFIG_KEY: &str = "signalbox.synthetic";
const SYNTHETIC_GIT_CONFIG_VALUE: &str = "drifted";
const GIT_SEED_PATH: &str = "seed.txt";
const GIT_STAGE_PATH: &str = "stage-me.txt";
const GIT_STAGE_CONTENT: &str = "stage me\n";
const GIT_WRONG_STAGE_CONTENT: &str = "wrong staged bytes\n";
const GIT_COMMIT_PATH: &str = "commit-me.txt";
const GIT_COMMIT_CONTENT: &str = "commit me\n";
const GIT_DIFF_OVERFLOW_PATH: &str = "zz-diff-overflow.txt";
const GIT_DIFF_OVERFLOW_BYTE: char = 'x';
const EXPECTED_GIT_DIFF_MAX_BYTES: usize = 128 * 1024;
const GIT_DIFF_OVERFLOW_CONTENT_BYTES: usize = EXPECTED_GIT_DIFF_MAX_BYTES + 1;
const GIT_STATUS_OVERFLOW_DIRECTORY: &str = "status-overflow";
const GIT_STATUS_OVERFLOW_CONTENT: &str = "status overflow fixture\n";
const EXPECTED_GIT_STATUS_MAX_ENTRIES: usize = 128;
const GIT_STATUS_OVERFLOW_ENTRY_COUNT: usize = EXPECTED_GIT_STATUS_MAX_ENTRIES - 2;
const GIT_NATURAL_PATH: &str = "eval.txt";
const GIT_NATURAL_CONTENT: &str = "natural eval\n";
const GIT_NATURAL_STAGED_PATH_COUNT: usize = 1;
const GIT_DRIFTED_NATURAL_CONTENT: &str = "drifted eval\n";
const GIT_COLLATERAL_PATH: &str = "collateral.txt";
const GIT_COLLATERAL_CONTENT: &str = "collateral\n";
const GIT_COLLATERAL_OBJECT_CONTENT: &[u8] = b"collateral object";
const GIT_COLLATERAL_DIRECTORY: &str = "collateral-directory";
const GIT_HOOKS_DIRECTORY: &str = "hooks";
const GIT_INFO_DIRECTORY: &str = "info";
const GIT_LOGS_DIRECTORY: &str = "logs";
const GIT_REFS_DIRECTORY: &str = "refs";
const GIT_DESCRIPTION_PATH: &str = "description";
const GIT_PRE_COMMIT_HOOK_PATH: &str = "hooks/pre-commit";
const GIT_PRE_COMMIT_HOOK_CONTENT: &str = "#!/bin/sh\nexit 1\n";
const GIT_NATURAL_MESSAGE: &str = "tool eval commit";
const GIT_SWITCH_CONTENT: &str = "seed two\n";
const GIT_BASE_CONTENT: &str = "seed three\n";
const GIT_BASE_BRANCH: &str = "eval-base";
const GIT_COMMIT_REFLOG_MESSAGE: &str = "commit";
const GIT_SWITCH_REFLOG_MESSAGE: &str = "checkout: moving to configured local branch";
const GIT_RESTORE_BRANCH_REFLOG_MESSAGE: &str = "restore synthetic seeded branch";
const GIT_MERGE_HEAD_PATH: &str = "MERGE_HEAD";
const GIT_MERGE_MESSAGE_PATH: &str = "MERGE_MSG";
const GIT_MERGE_MODE_PATH: &str = "MERGE_MODE";
const GIT_CHERRY_PICK_HEAD_PATH: &str = "CHERRY_PICK_HEAD";
const GIT_CONFIG_PATH: &str = "config";
const GIT_OBJECTS_DIRECTORY: &str = "objects";
const GIT_MERGE_MESSAGE: &str = "synthetic forced merge\n";
const GIT_MERGE_MODE: &str = "";
const GIT_REGULAR_FILE_MODE: i32 = 0o100644;
const GIT_REGULAR_INDEX_FILE_MODE: u32 = 0o100644;
const GIT_INDEX_EXTENDED_FLAG: u16 = 0x4000;
const GIT_INDEX_SKIP_WORKTREE_FLAG: u16 = 0x4000;
const WORKSPACE_SEED_PATH: &str = "brief.txt";
const WORKSPACE_SEED: &str = "alpha\nbeta fixture\nalpha\n";
const WORKSPACE_EDITED_SEED: &str = "beta\nbeta fixture\nbeta\n";
const EXPECTED_WORKSPACE_EDIT_REPLACEMENTS: usize = 2;
const EXPECTED_WORKSPACE_EDIT_BYTES: usize = 23;
const WORKSPACE_GLOB_DIRECTORY: &str = "glob-scope";
const WORKSPACE_GLOB_PATH: &str = "glob-scope/zz-glob.txt";
const WORKSPACE_GLOB_CONTENT: &str = "beta glob fixture\n";
const WORKSPACE_GLOB_OVERFLOW_PATH: &str = "glob-scope/zz-overflow.txt";
const WORKSPACE_GLOB_NONMATCHING_PATH: &str = "glob-scope/aa-nonmatching.md";
const WORKSPACE_GLOB_NONMATCHING_CONTENT: &str = "nonmatching glob fixture\n";
const WORKSPACE_DRIFTED_GLOB_CONTENT: &str = "drifted glob fixture\n";
const WORKSPACE_SEARCH_DIRECTORY: &str = "search-scope";
const WORKSPACE_SEARCH_PATH: &str = "search-scope/match.txt";
const WORKSPACE_SEARCH_CONTENT: &str =
    "search prelude\nbeta search fixture\nbeta search overflow\n";
const WORKSPACE_FORCED_READ_MAX_BYTES: usize = 6;
const WORKSPACE_DRIFTED_SEED: &str = "alpha\nbeta fixturE\nalpha\n";
#[cfg(unix)]
const USER_EXECUTE_MODE_BIT: u32 = 0o100;
#[cfg(unix)]
const GROUP_WRITE_MODE_BIT: u32 = 0o020;
#[cfg(unix)]
const WORKSPACE_PRIVATE_CREATION_MODE: u32 = 0o600;
#[cfg(unix)]
const WORKSPACE_INSECURE_CREATION_MODE: u32 = 0o777;
#[cfg(unix)]
const WORKSPACE_CREATED_FILE_MODE: Option<u32> = Some(WORKSPACE_PRIVATE_CREATION_MODE);
#[cfg(not(unix))]
const WORKSPACE_CREATED_FILE_MODE: Option<u32> = None;
#[cfg(unix)]
const WORKSPACE_CREATED_FILE_LINKS: Option<u64> = Some(1);
#[cfg(not(unix))]
const WORKSPACE_CREATED_FILE_LINKS: Option<u64> = None;
const SYNTHETIC_WRONG_STAGED_PATH_COUNT: usize = 0;
const SYNTHETIC_WRONG_COMMIT_ID: &str = "synthetic-wrong-commit-id";
const WORKSPACE_LIST_PATH: &str = "nested-list";
const WORKSPACE_LIST_MAX_RESULTS: usize = 20;
const WORKSPACE_LIST_ENTRY_COUNT: usize = WORKSPACE_LIST_MAX_RESULTS + 1;
const WORKSPACE_GLOB_MAX_RESULTS: usize = 1;
const WORKSPACE_SEARCH_MAX_RESULTS: usize = 1;
const WORKSPACE_NONMATCHING_COUNT: usize = WORKSPACE_LIST_MAX_RESULTS;
const WORKSPACE_ANSWER_PATH: &str = "answer.txt";
const WORKSPACE_ANSWER: &str = "model loop observed\n";
const WORKSPACE_COLLATERAL_DIRECTORY: &str = "collateral-directory";
const EXEC_SUPERVISOR_VARIABLE: &str = "SIGNALBOX_EXEC_SUPERVISOR";
const EXEC_RESULT_PATH: &str = "exec-result.txt";
const EXEC_RESULT: &str = "model loop observed\n";
const EXEC_FORCED_SANDBOXED_ARGUMENTS: &str = r#"{"program":"printf","arguments":["forced sandboxed eval\n"],"working_directory":".","timeout_seconds":30}"#;
const EXEC_FORCED_SANDBOXED_OUTPUT: &str = "forced sandboxed eval\n";
const EXEC_FORCED_READ_ONLY_OUTPUT: &str = "forced unsandboxed eval\n";
const SYNTHETIC_CARGO_TEST_EXECUTABLE: &str = "synthetic-test-executable";
const SYNTHETIC_CARGO_TEST_NAME: &str = "synthetic_test_name";
const SYNTHETIC_CARGO_DIAGNOSTIC_MESSAGE: &str = "synthetic compiler diagnostic";
const SYNTHETIC_CARGO_DIAGNOSTIC_FILE: &str = "src/lib.rs";
const CARGO_ERROR_DIAGNOSTIC_LEVEL: &str = "error";
const CARGO_WARNING_DIAGNOSTIC_LEVEL: &str = "warning";
const SYNTHETIC_CARGO_DIAGNOSTIC_LINE: u64 = 7;
const SYNTHETIC_CARGO_DIAGNOSTIC_START_COLUMN: u64 = 4;
const SYNTHETIC_CARGO_DIAGNOSTIC_END_COLUMN: u64 = 8;
const SYNTHETIC_CARGO_DIAGNOSTIC_BACKWARDS_END_COLUMN: u64 = 3;
const EXEC_NATURAL_ARGUMENTS: &str = r#"{"program":"/bin/sh","arguments":["-c","printf 'model loop observed\n' > exec-result.txt"],"working_directory":".","timeout_seconds":30}"#;
const EXEC_NATURAL_OUTPUT: &str = "";
const WEB_ORIGIN: &str = "https://example.com";
const WEB_URL: &str = "https://example.com/eval";
const WEB_QUERY: &str = "Signalbox tool evaluation";
const WEB_FETCH_BODY: &str = "Signalbox tool evaluation fixture";
const WEB_SEARCH_TITLE: &str = "Synthetic Signalbox result";
const WEB_SEARCH_SNIPPET: &str = "Synthetic result for model-in-the-loop evaluation.";
const OPENAI_MODEL_FAMILY: &str = "openai";
const OPENAI_FALLBACK_CREDENTIAL_REFERENCE: &str = "openai-tool-eval";
const EXPECTED_OPENAI_CREDENTIAL_REFERENCE: &str = "openai-primary";
const EXPECTED_WEB_CREDENTIAL_REFERENCE: &str = "brave-search-primary";
const SYNTHETIC_WEB_CREDENTIAL: &[u8] = b"synthetic-web-eval-key";
const ARBITRARY_EVAL_SELECTION_ID: u128 = 0x9101;
const ARBITRARY_EVAL_PROVIDER_ID: u128 = 0x9102;
const ARBITRARY_EVAL_REQUEST_ID: u128 = 0x9103;
const ARBITRARY_EVAL_ATTEMPT_ID: u128 = 0x9104;
const ARBITRARY_EVAL_ENTRY_ID: u128 = 0x9105;
const ARBITRARY_EVAL_TURN_ATTEMPT_ID: u128 = 0x9106;
const ARBITRARY_EVAL_SESSION_ID: u128 = 0x9107;
const ARBITRARY_EVAL_MODEL_CALL_ID: u128 = 0x9108;
const ARBITRARY_SECOND_EVAL_REQUEST_ID: u128 = 0x910a;
const ARBITRARY_SECOND_EVAL_MODEL_CALL_ID: u128 = 0x910b;
const ARBITRARY_EVAL_FRONTIER_ID: u128 = 0x910c;
const ARBITRARY_THIRD_EVAL_REQUEST_ID: u128 = 0x910d;
const ARBITRARY_FOURTH_EVAL_REQUEST_ID: u128 = 0x910e;
const ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX: u64 = 1;
const ARBITRARY_REQUEST_ENTRY_INDEX: u64 = 2;
const ARBITRARY_LATE_RESULT_ENTRY_INDEX: u64 = 3;
const GIT_NATURAL_STAGE_REQUEST_ENTRY_INDEX: u64 = 1;
const GIT_NATURAL_STAGE_RESULT_ENTRY_INDEX: u64 = 2;
const GIT_NATURAL_COMMIT_REQUEST_ENTRY_INDEX: u64 = 3;
const GIT_NATURAL_COMMIT_RESULT_ENTRY_INDEX: u64 = 4;
const MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP: i64 = 2;
const SYNTHETIC_EXECUTOR_FAILURE: &str = "synthetic executor failure";
const DRIFTED_APPLY_PATCH_ARGUMENTS: &str =
    r#"{"patch":"*** Begin Patch\n*** Add File: other.txt\n+drifted by eval\n*** End Patch"}"#;
const SYNTHETIC_EVAL_RECEIPT: &str = "01988c5f-89c4-7000-8000-000000000001";
const SYNTHETIC_COMPLETION_REPORT: &str = "Completed the requested operation.";
const SYNTHETIC_FAILURE_REPORT: &str = "Failed to complete the requested operation.";
const SYNTHETIC_CONTRACTED_FAILURE_REPORT: &str = "The operation wasn't completed.";
const SYNTHETIC_NEVER_COMPLETION_REPORT: &str = "Never completed the requested operation.";
const SYNTHETIC_DEFERRED_COMPLETION_REPORT: &str =
    "The requested operation has yet to be completed.";
const SYNTHETIC_APPLIED_COMPLETION_REPORT: &str = "The patch was applied successfully.";
const SYNTHETIC_NOT_APPLIED_REPORT: &str = "The patch was not applied.";
const SYNTHETIC_NO_ERRORS_COMPLETION_REPORT: &str =
    "Completed the requested operation with no errors.";
const SYNTHETIC_WITHOUT_FAILURE_COMPLETION_REPORT: &str =
    "Completed the requested operation without failure.";
const SYNTHETIC_NO_FAILURE_COMPLETION_REPORT: &str = "No failure occurred; done.";
const SYNTHETIC_NOTHING_FAILED_COMPLETION_REPORT: &str = "Nothing failed; completed successfully.";
const SYNTHETIC_NO_FILE_CHANGES_COMPLETION_REPORT: &str = "Done; no file changes were made.";
const SYNTHETIC_NO_FILE_WAS_CHANGED_COMPLETION_REPORT: &str = "Done; no file was changed.";
const SYNTHETIC_NO_FILES_WERE_MODIFIED_COMPLETION_REPORT: &str = "Done; no files were modified.";
const SYNTHETIC_NO_FILES_WERE_CREATED_COMPLETION_REPORT: &str = "Done; no files were created.";
const SYNTHETIC_VERB_FIRST_CREATION_DENIAL_REPORT: &str = "Done, but I did not create the file.";
const SYNTHETIC_VERB_FIRST_MODIFICATION_DENIAL_REPORT: &str =
    "Done, but I did not modify any files.";
const SYNTHETIC_BARE_NO_CHANGES_DENIAL_REPORT: &str = "Done, but no changes were made.";
const SYNTHETIC_BARE_NO_MODIFICATIONS_DENIAL_REPORT: &str = "Done, but no modifications were made.";
const SYNTHETIC_PERFECT_TENSE_NO_MODIFICATIONS_DENIAL_REPORT: &str =
    "Done, but no modifications have been made.";
const SYNTHETIC_EXISTENTIAL_NO_MODIFICATIONS_DENIAL_REPORT: &str =
    "Done, but there were no modifications.";
const SYNTHETIC_NO_FILE_WRITTEN_REPORT: &str = "No file was written.";
const SYNTHETIC_NOTHING_WRITTEN_REPORT: &str = "Nothing was written.";
const SYNTHETIC_SCOPED_NEGATION_COMPLETION_REPORT: &str =
    "Completed the commit; I did not modify any other files.";
const SYNTHETIC_SCOPED_CREATION_NEGATION_COMPLETION_REPORT: &str =
    "Done; I did not create any other files.";
const SYNTHETIC_SCOPED_CONJUNCTION_NEGATION_COMPLETION_REPORT: &str =
    "Done; I did not create or modify any other files.";
const SYNTHETIC_SEPARATE_COLLATERAL_CLAUSE_DENIAL_REPORT: &str =
    "The requested file was not created; other files were untouched.";
const EVAL_RECEIPT_FIELD: &str = "eval_receipt";
const RESULT_RECEIPT_INSTRUCTION: &str =
    "In your final answer, include every exact eval_receipt value returned by the tools.";

type EvalResult<T = ()> = Result<T, Box<dyn Error>>;

#[test]
#[ignore = "spends real OpenAI exchanges; run only from the gated tool-eval workflow"]
fn live_model_in_the_loop_evaluates_one_daemon_tool_family() -> EvalResult {
    let outcome = std::thread::Builder::new()
        .name(String::from("live-tool-eval"))
        .stack_size(LIVE_EVAL_THREAD_STACK_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(LIVE_EVAL_THREAD_STACK_BYTES)
                .build()
                .map_err(|error| rendered_error_chain(&error))?;
            runtime
                .block_on(run_selected_family_if_enabled())
                .map_err(|error| rendered_error_chain(error.as_ref()))
        })?
        .join()
        .map_err(|_| io::Error::other("the live tool eval thread panicked"))?;
    outcome.map_err(|error| io::Error::other(error).into())
}

/// Renders one error and every nested cause it forwards.
///
/// A thread boundary can only carry an owned string, and the family executor
/// wrapper deliberately displays a fixed sentence while retaining the concrete
/// cause as its source. Rendering the complete chain before the crossing keeps
/// the paid run diagnosable from the failure text alone.
fn rendered_error_chain(error: &(dyn Error + 'static)) -> String {
    let mut rendered = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        rendered.push_str(": ");
        rendered.push_str(&cause.to_string());
        source = cause.source();
    }
    rendered
}

#[test]
fn the_rendered_error_chain_names_every_nested_cause() {
    let error = FamilyExecutorError::new(io::Error::other(SYNTHETIC_EXECUTOR_FAILURE));

    assert_eq!(
        rendered_error_chain(&error),
        format!("the selected eval tool executor failed: {SYNTHETIC_EXECUTOR_FAILURE}")
    );
}

#[test]
fn the_rendered_error_chain_of_a_sourceless_error_is_its_own_text() {
    let error = io::Error::other(SYNTHETIC_EXECUTOR_FAILURE);

    assert_eq!(rendered_error_chain(&error), SYNTHETIC_EXECUTOR_FAILURE);
}

async fn run_selected_family_if_enabled() -> EvalResult {
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
    Ok(())
}

async fn run_forced_tier(
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

async fn run_case(
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
    let provider = RuntimeModelCallProvider::new(runtime, database.runtime_models.clone());
    let execution = PostgresProviderModelExecution::new(
        PostgresModelCallRepository::new(
            database.pool.clone(),
            database.targets.clone(),
            ModelCallCredentialReference::new(OPENAI_FALLBACK_CREDENTIAL_REFERENCE),
        )
        .with_session_credentials(database.credential_families.clone()),
        InProcessAttemptDispatchGate::default(),
        provider,
    )
    .with_tool_loop(
        InProcessToolDispatchGate::default(),
        suite.catalog.clone(),
        suite.executor.clone(),
    );
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
    let execution_completed = match (
        forced_case,
        snapshot.requests.as_slice(),
        expected_arguments.as_deref(),
    ) {
        (None, _, _) => suite.natural_execution_completed(&snapshot, &tracker)?,
        (Some(case), [request], Some(expected_arguments)) => {
            match tracker.result_content(request.request_id) {
                Some(content) => forced_case_completion_reported(
                    forced_execution_completed(
                        suite,
                        case,
                        ForcedExecutionEvidence {
                            persisted_arguments: &request.arguments_text,
                            expected_arguments,
                            result_content: &content,
                        },
                    )?,
                    &tracker,
                ),
                None => false,
            }
        }
        (Some(_), _, _) => false,
    };
    Ok(CaseOutcome {
        target: forced_tool.map(str::to_owned),
        expected_arguments,
        execution_completed,
        tool_results: tracker.tool_results(),
        snapshot,
    })
}

struct ForcedExecutionEvidence<'a> {
    persisted_arguments: &'a str,
    expected_arguments: &'a str,
    result_content: &'a str,
}

fn forced_execution_completed(
    suite: &FamilySuite,
    case: &ForcedCase,
    evidence: ForcedExecutionEvidence<'_>,
) -> EvalResult<bool> {
    if evidence.persisted_arguments != evidence.expected_arguments {
        return Ok(false);
    }
    suite.forced_case_result_passed(case, evidence.result_content)
}

fn forced_case_completion_reported(execution_completed: bool, tracker: &OperationTracker) -> bool {
    execution_completed && tracker.final_response_reports_completion()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EvalFamily {
    Git,
    Workspace,
    Web,
    Exec,
}

impl EvalFamily {
    fn from_environment() -> EvalResult<Option<Self>> {
        match std::env::var(FAMILY_VARIABLE).as_deref() {
            Ok("git") => Ok(Some(Self::Git)),
            Ok("workspace") => Ok(Some(Self::Workspace)),
            Ok("web") => Ok(Some(Self::Web)),
            Ok("exec") => Ok(Some(Self::Exec)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            _ => Err(io::Error::other("the configured tool-eval family is unsupported").into()),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Workspace => "workspace",
            Self::Web => "web",
            Self::Exec => "exec",
        }
    }

    const fn model(self) -> &'static str {
        match self {
            Self::Git | Self::Workspace | Self::Web | Self::Exec => DEFAULT_MODEL,
        }
    }

    fn build_suite(self) -> EvalResult<FamilySuite> {
        match self {
            Self::Git => FamilySuite::git(),
            Self::Workspace => FamilySuite::workspace(),
            Self::Web => FamilySuite::web(),
            Self::Exec => FamilySuite::exec(),
        }
    }
}

struct ForcedCase {
    name: &'static str,
    expected_arguments: &'static str,
    prompt: &'static str,
}

const GIT_CASES: &[ForcedCase] = &[
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

const WORKSPACE_CASES: &[ForcedCase] = &[
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

const WEB_CASES: &[ForcedCase] = &[
    ForcedCase {
        name: WEB_FETCH_NAME,
        expected_arguments: r#"{"url":"https://example.com/eval"}"#,
        prompt: "Call web_fetch with exactly {\"url\":\"https://example.com/eval\"}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: WEB_SEARCH_NAME,
        expected_arguments: r#"{"query":"Signalbox tool evaluation"}"#,
        prompt: "Call web_search with exactly {\"query\":\"Signalbox tool evaluation\"}. After its result, answer done without another tool call.",
    },
];

const EXEC_CASES: &[ForcedCase; 3] = &[
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

struct FamilySuite {
    family: EvalFamily,
    workspace: TempDir,
    git_seed: Option<Oid>,
    git_seed_refs: GitReferenceInventory,
    git_seed_fixture: GitFixtureSnapshot,
    catalog: MergedCatalog,
    executor: SharedFamilyExecutor,
    workspace_seed_entries: BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    git_pre_execution_worktree_entries: StdMutex<Option<BTreeMap<PathBuf, WorkspaceEntrySnapshot>>>,
    git_pre_execution_objects: StdMutex<Option<GitObjectInventory>>,
    git_pre_execution_object_entries: StdMutex<Option<BTreeMap<PathBuf, WorkspaceEntrySnapshot>>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorkspaceEntrySnapshot {
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct GitFixtureSnapshot {
    modes: BTreeMap<PathBuf, Option<u32>>,
    config: Vec<u8>,
    worktree_entries: BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    metadata_root_kind: GitMetadataEntryKind,
    metadata_root_mode: Option<u32>,
    metadata_top_level: BTreeMap<PathBuf, GitMetadataEntrySnapshot>,
    index_entries: Vec<GitIndexEntrySnapshot>,
    static_metadata_entries: BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    reflog_entries: BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    reference_entries: BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    objects: GitObjectInventory,
    object_entries: BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
}

type GitObjectInventory = BTreeMap<Oid, GitObjectSnapshot>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitObjectSnapshot {
    kind: ObjectType,
    content: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum GitMetadataEntryKind {
    #[default]
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GitMetadataEntrySnapshot {
    kind: GitMetadataEntryKind,
    mode: Option<u32>,
    links: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitIndexEntrySnapshot {
    path: Vec<u8>,
    id: Oid,
    mode: u32,
    flags: u16,
    flags_extended: u16,
}

type GitReferenceInventory = BTreeMap<Vec<u8>, GitReferenceTarget>;

#[derive(Clone, Debug, Eq, PartialEq)]
enum GitReferenceTarget {
    Direct(Oid),
    Symbolic(Vec<u8>),
}

impl FamilySuite {
    fn git() -> EvalResult<Self> {
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
        Ok(Self {
            family: EvalFamily::Git,
            workspace,
            git_seed: Some(git_seed),
            git_seed_refs,
            git_seed_fixture,
            catalog: MergedCatalog::try_new([catalog])?,
            executor: SharedFamilyExecutor::new(FamilyExecutor::Git(executor)),
            workspace_seed_entries: BTreeMap::new(),
            git_pre_execution_worktree_entries: StdMutex::new(None),
            git_pre_execution_objects: StdMutex::new(None),
            git_pre_execution_object_entries: StdMutex::new(None),
        })
    }

    fn workspace() -> EvalResult<Self> {
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
            git_pre_execution_worktree_entries: StdMutex::new(None),
            git_pre_execution_objects: StdMutex::new(None),
            git_pre_execution_object_entries: StdMutex::new(None),
        })
    }

    fn web() -> EvalResult<Self> {
        let workspace = tempfile::tempdir()?;
        let fetch = WebFetchTool::try_new(
            FixtureWebFetchTransport,
            WebFetchEgressPolicy::try_from_allowed_origins([String::from(WEB_ORIGIN)])?,
        )?;
        let search = WebSearchTool::try_new(
            FixtureWebCredential,
            FixtureWebSearchTransport,
            WebSearchConfiguration::new(WebSearchProvider::Brave),
        )?;
        let (fetch_catalog, fetch_executor) = fetch.into_parts();
        let (search_catalog, search_executor) = search.into_parts();
        Ok(Self {
            family: EvalFamily::Web,
            workspace,
            git_seed: None,
            git_seed_refs: BTreeMap::new(),
            git_seed_fixture: GitFixtureSnapshot::default(),
            catalog: MergedCatalog::try_new([fetch_catalog, search_catalog])?,
            executor: SharedFamilyExecutor::new(FamilyExecutor::Web {
                fetch: fetch_executor,
                search: search_executor,
            }),
            workspace_seed_entries: BTreeMap::new(),
            git_pre_execution_worktree_entries: StdMutex::new(None),
            git_pre_execution_objects: StdMutex::new(None),
            git_pre_execution_object_entries: StdMutex::new(None),
        })
    }

    fn exec() -> EvalResult<Self> {
        let workspace = tempfile::tempdir()?;
        seed_exec_workspace(workspace.path())?;
        let supervisor = std::env::var_os(EXEC_SUPERVISOR_VARIABLE)
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other("the exec supervisor path is missing"))?;
        let runner = TokioProcessRunner::try_new(supervisor)?;
        let sandboxed = SandboxedExecTool::try_new(runner.clone(), workspace.path())?;
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
            workspace_seed_entries: BTreeMap::new(),
            git_pre_execution_worktree_entries: StdMutex::new(None),
            git_pre_execution_objects: StdMutex::new(None),
            git_pre_execution_object_entries: StdMutex::new(None),
        })
    }

    const fn forced_cases(&self) -> &'static [ForcedCase] {
        match self.family {
            EvalFamily::Git => GIT_CASES,
            EvalFamily::Workspace => WORKSPACE_CASES,
            EvalFamily::Web => WEB_CASES,
            EvalFamily::Exec => EXEC_CASES,
        }
    }

    fn validate_forced_inventory(&self) -> EvalResult {
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

    fn validate_forced_case(&self, case: &ForcedCase) -> EvalResult {
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

    const fn natural_prompt(&self) -> &'static str {
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
                "Use sandboxed_exec with exactly {\"program\":\"/bin/sh\",\"arguments\":[\"-c\",\"printf 'model loop observed\\n' > exec-result.txt\"],\"working_directory\":\".\",\"timeout_seconds\":30} to create exec-result.txt. Keep every side effect inside the current workspace, then briefly report completion."
            }
        }
    }

    async fn prepare_for(&self, tool: &str) -> EvalResult {
        self.prepare_git_case(tool)?;
        if self.family == EvalFamily::Exec {
            self.executor.prepare_exec_case(tool).await?;
        }
        Ok(())
    }

    fn prepare_git_case(&self, tool: &str) -> EvalResult {
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
        }
        Ok(())
    }

    fn has_forced_case_verifier(&self, name: &str) -> bool {
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

    fn forced_case_result_passed(&self, case: &ForcedCase, content: &str) -> EvalResult<bool> {
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
                let pre_execution_objects = self
                    .git_pre_execution_objects
                    .lock()
                    .expect("Git pre-execution object lock is available");
                let pre_execution_object_entries = self
                    .git_pre_execution_object_entries
                    .lock()
                    .expect("Git pre-execution object-entry lock is available");
                git_forced_case_passed(
                    GitForcedVerification {
                        root: self.workspace.path(),
                        seed,
                        seed_refs: &self.git_seed_refs,
                        seed_fixture: &self.git_seed_fixture,
                        pre_execution_worktree_entries: pre_execution_worktree_entries.as_ref(),
                        pre_execution_objects: pre_execution_objects.as_ref(),
                        pre_execution_object_entries: pre_execution_object_entries.as_ref(),
                    },
                    case.name,
                    &arguments,
                    &result,
                )
            }
            EvalFamily::Workspace => workspace_forced_case_passed(
                self.workspace.path(),
                &self.workspace_seed_entries,
                case.name,
                &arguments,
                &result,
            ),
            EvalFamily::Web => Ok(web_forced_case_passed(case.name, &arguments, &result)),
            EvalFamily::Exec => Ok(exec_forced_case_passed(case.name, &result)),
        }
    }

    fn natural_state_passed(&self, snapshot: &CaseSnapshot) -> EvalResult<bool> {
        match self.family {
            EvalFamily::Git => {
                let seed = self.git_seed.ok_or_else(|| {
                    io::Error::other("the Git eval suite has no captured seed identity")
                })?;
                Ok(git_natural_state_passed(
                    self.workspace.path(),
                    seed,
                    &self.git_seed_refs,
                    &self.git_seed_fixture,
                )? && snapshot.git_natural_requests_passed()?)
            }
            EvalFamily::Workspace => {
                let entries_match = self.workspace_natural_entries_match()?;
                Ok(entries_match && snapshot.workspace_natural_requests_passed())
            }
            EvalFamily::Web => snapshot.web_natural_requests_passed(),
            EvalFamily::Exec => self.exec_result_matches(),
        }
    }

    fn workspace_natural_entries_match(&self) -> EvalResult<bool> {
        let answer_path = self.workspace.path().join(WORKSPACE_ANSWER_PATH);
        match fs::read(&answer_path) {
            Ok(bytes) if bytes == WORKSPACE_ANSWER.as_bytes() => {}
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        let mut actual = workspace_entries(self.workspace.path())?;
        let answer = actual.remove(Path::new(WORKSPACE_ANSWER_PATH));
        let expected_answer = WorkspaceEntrySnapshot::File {
            content: WORKSPACE_ANSWER.as_bytes().to_vec(),
            mode: WORKSPACE_CREATED_FILE_MODE,
            links: WORKSPACE_CREATED_FILE_LINKS,
        };
        Ok(answer == Some(expected_answer) && actual == self.workspace_seed_entries)
    }

    fn natural_execution_completed(
        &self,
        snapshot: &CaseSnapshot,
        tracker: &OperationTracker,
    ) -> EvalResult<bool> {
        match self.family {
            EvalFamily::Workspace => {
                Ok(workspace_natural_result_payloads_passed(snapshot, tracker)
                    && tracker.final_response_reports_completion())
            }
            EvalFamily::Web => Ok(web_natural_result_payloads_passed(snapshot, tracker)
                && tracker.final_response_reports(WEB_FETCH_BODY)),
            EvalFamily::Git => Ok(git_natural_result_payloads_passed(
                self.workspace.path(),
                snapshot,
                tracker,
            )? && tracker.final_response_reports_completion()),
            EvalFamily::Exec => Ok(tracker.final_response_reports_file_creation()),
        }
    }

    fn exec_result_matches(&self) -> EvalResult<bool> {
        match fs::read(self.workspace.path().join(EXEC_RESULT_PATH)) {
            Ok(bytes) => Ok(bytes == EXEC_RESULT.as_bytes()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

fn workspace_entries(root: &Path) -> EvalResult<BTreeMap<PathBuf, WorkspaceEntrySnapshot>> {
    filesystem_entries(root, None)
}

fn git_worktree_entries(root: &Path) -> EvalResult<BTreeMap<PathBuf, WorkspaceEntrySnapshot>> {
    filesystem_entries(root, Some(Path::new(".git")))
}

fn filesystem_entries(
    root: &Path,
    ignored_root_entry: Option<&Path>,
) -> EvalResult<BTreeMap<PathBuf, WorkspaceEntrySnapshot>> {
    let mut pending = vec![root.to_path_buf()];
    let mut entries = BTreeMap::from([(
        PathBuf::new(),
        WorkspaceEntrySnapshot::Directory {
            mode: worktree_mode(root)?,
        },
    )]);
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

fn seed_exec_workspace(root: &Path) -> EvalResult {
    fs::create_dir(root.join("src"))?;
    fs::create_dir(root.join(".cargo"))?;
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"tool-eval-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )?;
    fs::write(root.join("src/lib.rs"), "pub fn fixture() {}\n")?;
    fs::write(root.join(".cargo/config.toml"), "[net]\noffline = true\n")?;
    Ok(())
}

struct GitForcedVerification<'a> {
    root: &'a Path,
    seed: Oid,
    seed_refs: &'a GitReferenceInventory,
    seed_fixture: &'a GitFixtureSnapshot,
    pre_execution_worktree_entries: Option<&'a BTreeMap<PathBuf, WorkspaceEntrySnapshot>>,
    pre_execution_objects: Option<&'a GitObjectInventory>,
    pre_execution_object_entries: Option<&'a BTreeMap<PathBuf, WorkspaceEntrySnapshot>>,
}

fn git_forced_case_passed(
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
        pre_execution_objects,
        pre_execution_object_entries,
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
        && git_forced_index_matches(&repository, name, seed_fixture)?
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
            pre_execution_object_entries,
        )?
        && git_forced_reference_entries_match(root, name, arguments, &head, seed_fixture)?
        && git_forced_reflogs_match(root, name, seed, head.id(), seed_fixture)?
        && git_forced_worktree_matches(root, name, seed_fixture, pre_execution_worktree_entries)?)
}

fn commit_adds_exact_fixture(
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

fn expected_git_worktree_patch(root: &Path) -> EvalResult<String> {
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

fn expected_bounded_git_worktree_patch(root: &Path) -> EvalResult<String> {
    expected_git_worktree_patch(root)?
        .get(..EXPECTED_GIT_DIFF_MAX_BYTES)
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("the Git diff fixture does not exceed its bound").into())
}

fn git_diff_overflow_content() -> String {
    GIT_DIFF_OVERFLOW_BYTE
        .to_string()
        .repeat(GIT_DIFF_OVERFLOW_CONTENT_BYTES)
}

fn git_diff_fixture_unchanged(root: &Path, repository: &Repository) -> EvalResult<bool> {
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

fn git_status_fixture_unchanged(root: &Path, repository: &Repository) -> EvalResult<bool> {
    let base_unchanged = git_base_status_fixture_unchanged(root, repository)?;
    let mut overflow_unchanged = true;
    for index in 0..GIT_STATUS_OVERFLOW_ENTRY_COUNT {
        let path = git_status_overflow_path(index);
        overflow_unchanged &= repository.status_file(Path::new(&path))? == Status::WT_NEW
            && fs::read(root.join(path))? == GIT_STATUS_OVERFLOW_CONTENT.as_bytes();
    }
    Ok(base_unchanged && overflow_unchanged)
}

fn git_status_path_states(repository: &Repository) -> EvalResult<BTreeMap<PathBuf, Status>> {
    let mut states = BTreeMap::new();
    for entry in repository.statuses(None)?.iter() {
        let path = entry
            .path()
            .map_err(|error| io::Error::other(format!("invalid Git status path: {error}")))?;
        states.insert(PathBuf::from(path), entry.status());
    }
    Ok(states)
}

fn expected_base_git_statuses() -> BTreeMap<PathBuf, Status> {
    BTreeMap::from([
        (PathBuf::from(GIT_COMMIT_PATH), Status::WT_NEW),
        (PathBuf::from(GIT_NATURAL_PATH), Status::WT_NEW),
        (PathBuf::from(GIT_STAGE_PATH), Status::WT_NEW),
    ])
}

fn expected_staged_git_statuses() -> BTreeMap<PathBuf, Status> {
    BTreeMap::from([
        (PathBuf::from(GIT_COMMIT_PATH), Status::WT_NEW),
        (PathBuf::from(GIT_NATURAL_PATH), Status::WT_NEW),
        (PathBuf::from(GIT_STAGE_PATH), Status::INDEX_NEW),
    ])
}

fn expected_commit_git_statuses() -> BTreeMap<PathBuf, Status> {
    BTreeMap::from([
        (PathBuf::from(GIT_NATURAL_PATH), Status::WT_NEW),
        (PathBuf::from(GIT_STAGE_PATH), Status::WT_NEW),
    ])
}

fn expected_diff_git_statuses() -> BTreeMap<PathBuf, Status> {
    BTreeMap::from([
        (PathBuf::from(GIT_COMMIT_PATH), Status::WT_NEW),
        (PathBuf::from(GIT_DIFF_OVERFLOW_PATH), Status::WT_NEW),
        (PathBuf::from(GIT_NATURAL_PATH), Status::WT_NEW),
        (PathBuf::from(GIT_STAGE_PATH), Status::INDEX_NEW),
    ])
}

fn expected_status_git_statuses() -> BTreeMap<PathBuf, Status> {
    let mut states = expected_base_git_statuses();
    for index in 0..GIT_STATUS_OVERFLOW_ENTRY_COUNT {
        states.insert(
            PathBuf::from(git_status_overflow_path(index)),
            Status::WT_NEW,
        );
    }
    states
}

fn git_base_status_fixture_unchanged(root: &Path, repository: &Repository) -> EvalResult<bool> {
    Ok(
        repository.status_file(Path::new(GIT_SEED_PATH))? == Status::CURRENT
            && fs::read(root.join(GIT_SEED_PATH))? == GIT_BASE_CONTENT.as_bytes()
            && git_untracked_fixtures_unchanged(root, repository)?,
    )
}

fn git_untracked_fixtures_unchanged(root: &Path, repository: &Repository) -> EvalResult<bool> {
    Ok(
        repository.status_file(Path::new(GIT_STAGE_PATH))? == Status::WT_NEW
            && repository.status_file(Path::new(GIT_COMMIT_PATH))? == Status::WT_NEW
            && repository.status_file(Path::new(GIT_NATURAL_PATH))? == Status::WT_NEW
            && fs::read(root.join(GIT_STAGE_PATH))? == GIT_STAGE_CONTENT.as_bytes()
            && fs::read(root.join(GIT_COMMIT_PATH))? == GIT_COMMIT_CONTENT.as_bytes()
            && fs::read(root.join(GIT_NATURAL_PATH))? == GIT_NATURAL_CONTENT.as_bytes(),
    )
}

fn git_status_overflow_path(index: usize) -> String {
    format!("{GIT_STATUS_OVERFLOW_DIRECTORY}/{index:03}.txt")
}

fn expected_git_status_paths() -> Vec<String> {
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

fn git_status_entries_match(entries: &serde_json::Value) -> bool {
    let Some(entries) = entries.as_array() else {
        return false;
    };
    let expected_paths = expected_git_status_paths();
    entries.len() == EXPECTED_GIT_STATUS_MAX_ENTRIES
        && entries.iter().zip(expected_paths).all(|(entry, path)| {
            json_object_has_exact_fields(entry, &["path", "previous_path", "index", "worktree"])
                && entry["path"] == path
                && entry["previous_path"].is_null()
                && entry["index"] == "unchanged"
                && entry["worktree"] == "untracked"
        })
}

fn git_status_entries_json() -> Vec<serde_json::Value> {
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

fn git_operation_state_is_clean(repository: &Repository) -> bool {
    repository.state() == RepositoryState::Clean
        && !repository.path().join(GIT_MERGE_HEAD_PATH).exists()
        && !repository.path().join(GIT_MERGE_MESSAGE_PATH).exists()
        && !repository.path().join(GIT_MERGE_MODE_PATH).exists()
}

fn staged_blob_matches_fixture(
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

fn worktree_file_mode_matches(path: &Path, expected: Option<u32>) -> EvalResult<bool> {
    Ok(worktree_mode(path)? == expected)
}

fn worktree_mode(path: &Path) -> EvalResult<Option<u32>> {
    #[cfg(unix)]
    return Ok(Some(fs::metadata(path)?.permissions().mode() & 0o7777));
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

fn worktree_link_count(path: &Path) -> EvalResult<Option<u64>> {
    #[cfg(unix)]
    return Ok(Some(fs::metadata(path)?.nlink()));
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

fn workspace_forced_case_passed(
    root: &Path,
    seed_entries: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    name: &str,
    arguments: &serde_json::Value,
    result: &serde_json::Value,
) -> EvalResult<bool> {
    let expected_fields: &[&str] = match name {
        APPLY_PATCH_NAME => &["operations_applied", EVAL_RECEIPT_FIELD],
        EDIT_FILE_NAME => &["path", "replacements", "bytes_written", EVAL_RECEIPT_FIELD],
        WRITE_FILE_NAME => &["path", "bytes_written", "created", EVAL_RECEIPT_FIELD],
        READ_FILE_NAME => &[
            "path",
            "content",
            "bytes_read",
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
                && result["bytes_read"] == expected.len()
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
            Ok(workspace_entries(root)? == *seed_entries)
        }
        _ => Ok(true),
    }
}

fn workspace_mutation_entries_match(
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

fn result_entries_equal(value: &serde_json::Value, expected: &[(String, &'static str)]) -> bool {
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

fn workspace_nonmatching_path(index: usize) -> String {
    format!("zz-extra-{index:02}.bin")
}

fn workspace_listing(entry_count: usize) -> Vec<(String, &'static str)> {
    (0..entry_count)
        .map(|index| (workspace_list_entry_path(index), "file"))
        .collect()
}

fn workspace_list_entry_path(index: usize) -> String {
    format!("{WORKSPACE_LIST_PATH}/entry-{index:02}.txt")
}

fn expected_workspace_listing() -> Vec<(String, &'static str)> {
    workspace_listing(WORKSPACE_LIST_MAX_RESULTS)
}

fn complete_workspace_listing() -> Vec<(String, &'static str)> {
    workspace_listing(WORKSPACE_LIST_ENTRY_COUNT)
}

fn workspace_listing_json(entries: Vec<(String, &'static str)>) -> Vec<serde_json::Value> {
    entries
        .into_iter()
        .map(|(path, kind)| serde_json::json!({"path": path, "kind": kind}))
        .collect()
}

fn web_forced_case_passed(
    name: &str,
    arguments: &serde_json::Value,
    result: &serde_json::Value,
) -> bool {
    match name {
        WEB_FETCH_NAME => {
            json_object_has_exact_fields(
                result,
                &[
                    "url",
                    "status",
                    "content_type",
                    "body",
                    "truncated",
                    EVAL_RECEIPT_FIELD,
                ],
            ) && result["url"] == arguments["url"]
                && result["status"] == 200
                && result["content_type"] == "text/plain"
                && result["body"] == WEB_FETCH_BODY
                && result["truncated"] == true
        }
        WEB_SEARCH_NAME => {
            json_object_has_exact_fields(result, &["results", "truncated", EVAL_RECEIPT_FIELD])
                && result["results"].as_array().is_some_and(|results| {
                    results.len() == 1
                        && results.first().is_some_and(|first| {
                            json_object_has_exact_fields(first, &["title", "url", "snippet"])
                                && first["title"] == WEB_SEARCH_TITLE
                                && first["url"] == WEB_URL
                                && first["snippet"] == WEB_SEARCH_SNIPPET
                        })
                })
                && result["truncated"] == true
        }
        _ => false,
    }
}

fn exec_forced_case_passed(target: &str, result: &serde_json::Value) -> bool {
    if target == CARGO_DIAGNOSTICS_NAME {
        return cargo_diagnostics_result_passed(result);
    }
    let expected_confinement = match target {
        SANDBOXED_EXEC_NAME => "filesystem_confined",
        UNSANDBOXED_EXEC_NAME => "unsandboxed",
        _ => return false,
    };
    if result["confinement"]["kind"] != expected_confinement || !exited_cleanly(result) {
        return false;
    }
    let stdout_matches = match target {
        SANDBOXED_EXEC_NAME => captured_stdout_is(result, EXEC_FORCED_SANDBOXED_OUTPUT),
        UNSANDBOXED_EXEC_NAME => captured_stdout_is(result, EXEC_FORCED_READ_ONLY_OUTPUT),
        _ => false,
    };
    stdout_matches && captured_stderr_is_empty(result)
}

fn json_object_has_exact_fields(value: &serde_json::Value, expected: &[&str]) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == expected.len() && expected.iter().all(|field| object.contains_key(*field))
    })
}

fn web_natural_result_payloads_passed(snapshot: &CaseSnapshot, tracker: &OperationTracker) -> bool {
    let Ok(Some((search, fetch))) = snapshot.web_natural_request_pair() else {
        return false;
    };
    let Some(search_content) = tracker.result_content(search.request_id) else {
        return false;
    };
    let Some(fetch_content) = tracker.result_content(fetch.request_id) else {
        return false;
    };
    let Ok(search_result) = serde_json::from_str::<serde_json::Value>(&search_content) else {
        return false;
    };
    let Ok(fetch_result) = serde_json::from_str::<serde_json::Value>(&fetch_content) else {
        return false;
    };
    web_forced_case_passed(
        WEB_SEARCH_NAME,
        &serde_json::json!({"query": WEB_QUERY}),
        &search_result,
    ) && web_forced_case_passed(
        WEB_FETCH_NAME,
        &serde_json::json!({"url": WEB_URL}),
        &fetch_result,
    )
}

fn normalized_arguments_text(arguments: &str) -> EvalResult<String> {
    NormalizedToolArguments::try_from_provider_text(arguments.to_owned())
        .map(|arguments| arguments.as_str().to_owned())
        .map_err(|_| io::Error::other("the eval fixture arguments do not normalize").into())
}

fn seed_git_repository(root: &Path) -> EvalResult<Oid> {
    let repository = Repository::init(root)?;
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

fn seed_git_repository_with_refs(
    root: &Path,
) -> EvalResult<(Oid, GitReferenceInventory, GitFixtureSnapshot)> {
    let seed = seed_git_repository(root)?;
    let refs = git_reference_inventory(&Repository::open(root)?)?;
    let fixture = git_fixture_snapshot(root)?;
    Ok((seed, refs, fixture))
}

fn git_reference_inventory(repository: &Repository) -> EvalResult<GitReferenceInventory> {
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

fn git_reference_entries(root: &Path) -> EvalResult<BTreeMap<PathBuf, WorkspaceEntrySnapshot>> {
    let repository = Repository::open(root)?;
    filesystem_entries(&repository.path().join(GIT_REFS_DIRECTORY), None)
}

fn direct_git_reference_entry(
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

fn git_forced_reference_entries_match(
    root: &Path,
    case_name: &str,
    arguments: &serde_json::Value,
    head: &git2::Commit<'_>,
    seed_fixture: &GitFixtureSnapshot,
) -> EvalResult<bool> {
    let repository = Repository::open(root)?;
    let mut expected = seed_fixture.reference_entries.clone();
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
            expected.insert(Path::new("heads").join(branch_name), entry);
        }
        GIT_CREATE_COMMIT_NAME => {
            let Some(template) = expected.get(&base_path) else {
                return Ok(false);
            };
            let Some(entry) = direct_git_reference_entry(template, head.id()) else {
                return Ok(false);
            };
            expected.insert(base_path, entry);
        }
        _ => {}
    }
    Ok(git_reference_entries(root)? == expected)
}

fn git_fixture_modes(root: &Path) -> EvalResult<BTreeMap<PathBuf, Option<u32>>> {
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

fn git_fixture_snapshot(root: &Path) -> EvalResult<GitFixtureSnapshot> {
    let repository = Repository::open(root)?;
    Ok(GitFixtureSnapshot {
        modes: git_fixture_modes(root)?,
        config: fs::read(repository.path().join(GIT_CONFIG_PATH))?,
        worktree_entries: git_worktree_entries(root)?,
        metadata_root_kind: git_metadata_root_kind(root)?,
        metadata_root_mode: worktree_mode(repository.path())?,
        metadata_top_level: git_metadata_top_level(root)?,
        index_entries: git_index_entries(&repository)?,
        static_metadata_entries: git_static_metadata_entries(root)?,
        reflog_entries: git_reflog_entries(root)?,
        reference_entries: git_reference_entries(root)?,
        objects: git_object_inventory(&repository)?,
        object_entries: git_object_entries(root)?,
    })
}

fn git_metadata_root_kind(root: &Path) -> EvalResult<GitMetadataEntryKind> {
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

fn git_index_entries(repository: &Repository) -> EvalResult<Vec<GitIndexEntrySnapshot>> {
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

fn expected_git_index_entry(path: &str, content: &[u8]) -> EvalResult<GitIndexEntrySnapshot> {
    let path_bytes = path.as_bytes().to_vec();
    Ok(GitIndexEntrySnapshot {
        flags: u16::try_from(path_bytes.len().min(0x0fff))?,
        path: path_bytes,
        id: Oid::hash_object(ObjectType::Blob, content)?,
        mode: GIT_REGULAR_INDEX_FILE_MODE,
        flags_extended: 0,
    })
}

fn git_index_with_expected_file(
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

fn git_forced_index_matches(
    repository: &Repository,
    case_name: &str,
    seed_fixture: &GitFixtureSnapshot,
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
    Ok(git_index_entries(repository)? == expected)
}

fn git_object_inventory(repository: &Repository) -> EvalResult<GitObjectInventory> {
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

fn git_object_entries(root: &Path) -> EvalResult<BTreeMap<PathBuf, WorkspaceEntrySnapshot>> {
    let repository = Repository::open(root)?;
    filesystem_entries(&repository.path().join(GIT_OBJECTS_DIRECTORY), None)
}

fn git_loose_object_relative_path(id: Oid) -> PathBuf {
    let id = id.to_string();
    Path::new(&id[..2]).join(&id[2..])
}

fn git_object_entry_inventory_matches(
    root: &Path,
    baseline: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    allowed_ids: &[Oid],
    seed_fixture: &GitFixtureSnapshot,
) -> EvalResult<bool> {
    let actual = git_object_entries(root)?;
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
    let mut expected = baseline.clone();
    for id in allowed_ids {
        let relative = git_loose_object_relative_path(*id);
        if expected.contains_key(&relative) {
            continue;
        }
        let Some(parent) = relative.parent() else {
            return Ok(false);
        };
        expected
            .entry(parent.to_path_buf())
            .or_insert(WorkspaceEntrySnapshot::Directory {
                mode: directory_mode,
            });
        let Some(WorkspaceEntrySnapshot::File { content, .. }) = actual.get(&relative) else {
            return Ok(false);
        };
        expected.insert(
            relative,
            WorkspaceEntrySnapshot::File {
                content: content.clone(),
                mode: file_mode,
                links: file_links,
            },
        );
    }
    Ok(actual == expected)
}

fn git_forced_objects_match(
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

fn git_forced_object_entries_match(
    root: &Path,
    case_name: &str,
    head: &git2::Commit<'_>,
    seed_fixture: &GitFixtureSnapshot,
    pre_execution: Option<&BTreeMap<PathBuf, WorkspaceEntrySnapshot>>,
) -> EvalResult<bool> {
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
        pre_execution.unwrap_or(&seed_fixture.object_entries),
        &allowed,
        seed_fixture,
    )
}

fn git_reflog_entries(root: &Path) -> EvalResult<BTreeMap<PathBuf, WorkspaceEntrySnapshot>> {
    let repository = Repository::open(root)?;
    filesystem_entries(&repository.path().join(GIT_LOGS_DIRECTORY), None)
}

fn git_forced_reflogs_match(
    root: &Path,
    case_name: &str,
    seed: Oid,
    head: Oid,
    seed_fixture: &GitFixtureSnapshot,
) -> EvalResult<bool> {
    match case_name {
        GIT_CREATE_COMMIT_NAME => {
            let branch_reference = format!("refs/heads/{GIT_BASE_BRANCH}");
            git_reflog_updates_match(
                root,
                seed,
                head,
                GIT_COMMIT_REFLOG_MESSAGE,
                &["HEAD", branch_reference.as_str()],
                &seed_fixture.reflog_entries,
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
                seed,
                target,
                GIT_SWITCH_REFLOG_MESSAGE,
                &["HEAD"],
                &seed_fixture.reflog_entries,
            )
        }
        _ => Ok(git_reflog_entries(root)? == seed_fixture.reflog_entries),
    }
}

fn git_reflog_updates_match(
    root: &Path,
    old: Oid,
    new: Oid,
    message: &str,
    references: &[&str],
    seed_entries: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
) -> EvalResult<bool> {
    let repository = Repository::open(root)?;
    let actual_entries = git_reflog_entries(root)?;
    let mut expected_entries = seed_entries.clone();
    for reference in references {
        if !replace_expected_reflog_update(
            &repository,
            &actual_entries,
            &mut expected_entries,
            reference,
            old,
            new,
            message,
        )? {
            return Ok(false);
        }
    }
    Ok(actual_entries == expected_entries)
}

fn replace_expected_reflog_update(
    repository: &Repository,
    actual_entries: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    expected_entries: &mut BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    reference: &str,
    old: Oid,
    new: Oid,
    message: &str,
) -> EvalResult<bool> {
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
    let commit_time_matches = if message == GIT_COMMIT_REFLOG_MESSAGE {
        let commit_time = repository.find_commit(new)?.committer().when();
        committer_time.seconds() == commit_time.seconds()
            && committer_time.offset_minutes() == commit_time.offset_minutes()
    } else {
        true
    };
    if latest.id_old() != old
        || latest.id_new() != new
        || latest.message().ok().flatten() != Some(message)
        || committer.name().ok() != Some(GIT_AUTHOR_NAME)
        || committer.email().ok() != Some(GIT_AUTHOR_EMAIL)
        || !commit_time_matches
    {
        return Ok(false);
    }
    expected_entries.insert(path.to_path_buf(), actual_entries[path].clone());
    Ok(true)
}

fn git_metadata_top_level(root: &Path) -> EvalResult<BTreeMap<PathBuf, GitMetadataEntrySnapshot>> {
    let repository = Repository::open(root)?;
    let mut entries = BTreeMap::new();
    for entry in fs::read_dir(repository.path())? {
        let entry = entry?;
        let file_type = entry.file_type()?;
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
        let links = if kind == GitMetadataEntryKind::File {
            worktree_link_count(&entry.path())?
        } else {
            None
        };
        entries.insert(
            PathBuf::from(entry.file_name()),
            GitMetadataEntrySnapshot { kind, mode, links },
        );
    }
    Ok(entries)
}

fn git_static_metadata_entries(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, WorkspaceEntrySnapshot>> {
    let repository = Repository::open(root)?;
    let metadata_root = repository.path();
    let mut entries = BTreeMap::new();
    for directory in [GIT_HOOKS_DIRECTORY, GIT_INFO_DIRECTORY] {
        for (relative, snapshot) in filesystem_entries(&metadata_root.join(directory), None)? {
            entries.insert(Path::new(directory).join(relative), snapshot);
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

fn git_fixture_modes_match(
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

fn git_fixture_snapshot_matches(
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
        && git_metadata_top_level(root)? == expected.metadata_top_level
        && git_static_metadata_entries(root)? == expected.static_metadata_entries)
}

fn git_forced_worktree_matches(
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

fn insert_expected_file_with_observed_mode(
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

fn commit_git_seed_revision<'repository>(
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

fn stage_path(root: &Path, path: &str) -> EvalResult {
    let repository = Repository::open(root)?;
    let mut index = repository.index()?;
    index.add_all([path], IndexAddOption::DEFAULT, None)?;
    index.write()?;
    Ok(())
}

fn install_git_merge_state(root: &Path, seed: Oid) -> EvalResult {
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

fn commit_staged_paths(root: &Path, message: &str) -> EvalResult {
    commit_staged_paths_with_identity(root, message, GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL)
}

fn commit_staged_paths_with_identity(
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

fn normalize_latest_reflog_message(
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

fn replace_latest_reflog_signature(
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

fn git_natural_state_passed(
    root: &Path,
    seed: Oid,
    seed_refs: &GitReferenceInventory,
    seed_fixture: &GitFixtureSnapshot,
) -> EvalResult<bool> {
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
    let complete_reference_entry_inventory_matches =
        git_natural_reference_entries_match(root, &head, seed_fixture)?;
    let complete_object_inventory_matches =
        git_natural_objects_match(&repository, &head, seed_fixture)?;
    let complete_object_entry_inventory_matches =
        git_natural_object_entries_match(root, &head, seed_fixture)?;
    let fixture_matches = git_fixture_snapshot_matches(root, &repository, seed_fixture)?;
    let reflogs_match = git_reflog_updates_match(
        root,
        seed,
        head.id(),
        GIT_COMMIT_REFLOG_MESSAGE,
        &["HEAD", format!("refs/heads/{GIT_BASE_BRANCH}").as_str()],
        &seed_fixture.reflog_entries,
    )?;
    let complete_worktree_inventory_matches =
        git_worktree_entries(root)? == seed_fixture.worktree_entries;
    Ok(head_remains_on_seeded_branch
        && seeded_branch_advanced
        && message_matches
        && identity_matches
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
        && reflogs_match
        && complete_worktree_inventory_matches)
}

fn git_natural_objects_match(
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

fn git_natural_object_entries_match(
    root: &Path,
    head: &git2::Commit<'_>,
    seed_fixture: &GitFixtureSnapshot,
) -> EvalResult<bool> {
    let allowed = [
        Oid::hash_object(ObjectType::Blob, GIT_NATURAL_CONTENT.as_bytes())?,
        head.tree_id(),
        head.id(),
    ];
    git_object_entry_inventory_matches(root, &seed_fixture.object_entries, &allowed, seed_fixture)
}

fn git_natural_reference_entries_match(
    root: &Path,
    head: &git2::Commit<'_>,
    seed_fixture: &GitFixtureSnapshot,
) -> EvalResult<bool> {
    let mut expected = seed_fixture.reference_entries.clone();
    let base_path = Path::new("heads").join(GIT_BASE_BRANCH);
    let Some(template) = expected.get(&base_path) else {
        return Ok(false);
    };
    let Some(entry) = direct_git_reference_entry(template, head.id()) else {
        return Ok(false);
    };
    expected.insert(base_path, entry);
    Ok(git_reference_entries(root)? == expected)
}

fn git_natural_status_matches(repository: &Repository) -> EvalResult<bool> {
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

fn git_natural_result_payloads_passed(
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

fn untracked_git_fixture_matches(
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

#[derive(Clone, Debug)]
struct MergedCatalog {
    entries: BTreeMap<DomainToolName, MergedCatalogEntry>,
}

#[derive(Clone, Debug)]
struct MergedCatalogEntry {
    definition: ToolDefinition,
    catalog: CompiledToolCatalog,
}

impl MergedCatalog {
    fn try_new(catalogs: impl IntoIterator<Item = CompiledToolCatalog>) -> EvalResult<Self> {
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

enum FamilyExecutor {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExecEvalCase {
    Natural,
    ForcedSandboxed,
    ForcedUnsandboxed,
    ForcedDiagnostics,
}

#[derive(Clone, Copy)]
struct ExecFixtureCall {
    name: &'static str,
    expected_arguments: &'static str,
}

impl ExecEvalCase {
    fn for_forced_tool(tool: &str) -> EvalResult<Self> {
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
    fn admitted_call(self) -> ExecFixtureCall {
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

    fn admits(self, name: &str, arguments: &NormalizedToolArguments) -> bool {
        let expected_call = self.admitted_call();
        let expected = NormalizedToolArguments::try_from_provider_text(
            expected_call.expected_arguments.to_owned(),
        )
        .expect("the static exec eval arguments normalize");
        name == expected_call.name && arguments == &expected
    }
}

/// The one forced fixture an Exec case dispatches and reports against.
fn forced_exec_fixture(name: &'static str) -> ExecFixtureCall {
    let case = EXEC_CASES
        .iter()
        .find(|case| case.name == name)
        .expect("every Exec eval case names a forced fixture");
    ExecFixtureCall {
        name: case.name,
        expected_arguments: case.expected_arguments,
    }
}

#[derive(Clone)]
struct SharedFamilyExecutor {
    inner: Arc<Mutex<FamilyExecutor>>,
}

impl SharedFamilyExecutor {
    fn new(inner: FamilyExecutor) -> Self {
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    async fn prepare_exec_case(&self, tool: &str) -> EvalResult {
        let mut inner = self.inner.lock().await;
        let FamilyExecutor::Exec { case, .. } = &mut *inner else {
            return Err(io::Error::other("the selected eval executor is not Exec").into());
        };
        *case = ExecEvalCase::for_forced_tool(tool)?;
        Ok(())
    }
}

#[derive(Debug)]
struct FamilyExecutorError {
    source: Box<dyn Error + Send + Sync>,
}

impl FamilyExecutorError {
    fn new(source: impl Error + Send + Sync + 'static) -> Self {
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

#[test]
fn family_executor_error_preserves_its_concrete_source() {
    let error = FamilyExecutorError::new(io::Error::other(SYNTHETIC_EXECUTOR_FAILURE));

    assert_eq!(
        error.source().map(ToString::to_string),
        Some(String::from(SYNTHETIC_EXECUTOR_FAILURE))
    );
}

impl ToolExecutor for SharedFamilyExecutor {
    type Error = FamilyExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let name = invocation.request().name().as_str().to_owned();
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

fn add_eval_receipt(
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
    fields.insert(
        String::from(EVAL_RECEIPT_FIELD),
        serde_json::Value::String(receipt.to_owned()),
    );
    serde_json::to_string(&result)
        .map(ToolExecutorEvidence::CompletedText)
        .map_err(|_| io::Error::other("the eval receipt could not be encoded"))
}

#[derive(Clone, Copy, Debug)]
struct FixtureWebFetchTransport;

impl WebFetchTransport for FixtureWebFetchTransport {
    async fn fetch(
        &mut self,
        request: WebFetchRequest,
    ) -> Result<WebFetchResponse, WebFetchTransportFailure> {
        if request.url().as_str() != WEB_URL {
            return Err(WebFetchTransportFailure::RequestFailed);
        }
        WebFetchResponse::new(
            200,
            Some(String::from("text/plain")),
            WEB_FETCH_BODY.as_bytes().to_vec(),
            WebFetchBodyCompleteness::Truncated,
        )
        .ok_or(WebFetchTransportFailure::DispatchUnknown)
    }
}

#[derive(Clone, Copy, Debug)]
struct FixtureWebCredential;

impl CredentialAccess for FixtureWebCredential {
    async fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        assert_eq!(reference.as_str(), EXPECTED_WEB_CREDENTIAL_REFERENCE);
        Ok(CredentialValue::new(SYNTHETIC_WEB_CREDENTIAL.to_vec()))
    }
}

#[derive(Clone, Copy, Debug)]
struct FixtureWebSearchTransport;

impl WebSearchTransport for FixtureWebSearchTransport {
    async fn search(
        &mut self,
        request: WebSearchRequest,
        credential: &CredentialValue,
    ) -> WebSearchTransportOutcome {
        if request.query() != WEB_QUERY || credential.expose_bytes() != SYNTHETIC_WEB_CREDENTIAL {
            return WebSearchTransportOutcome::failed(
                WebSearchTransportFailure::RequestFailed,
                credential,
            );
        }
        let result = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(WEB_SEARCH_TITLE),
            url: String::from(WEB_URL),
            snippet: String::from(WEB_SEARCH_SNIPPET),
        })
        .expect("the synthetic web result is valid");
        let response =
            WebSearchResponse::new(vec![result], WebSearchPageCompleteness::MoreAvailable)
                .expect("the synthetic web response is bounded");
        WebSearchTransportOutcome::completed(response, credential)
    }
}

struct EnvironmentCredential;

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

struct EvalOpenAiRuntime {
    inner: OpenAiRuntime<EnvironmentCredential>,
    forced: ForcedToolSequence,
    tracker: OperationTracker,
}

#[derive(Debug, Eq, PartialEq)]
enum ForcedToolOperation {
    Natural,
    Force(RuntimeToolName),
    Continuation,
}

struct ForcedToolSequence {
    pending: Option<StdMutex<Option<RuntimeToolName>>>,
    natural_tool_rounds: Option<StdMutex<usize>>,
}

impl ForcedToolSequence {
    fn new(forced_tool: Option<&str>) -> Self {
        Self {
            pending: forced_tool.map(|tool| StdMutex::new(Some(RuntimeToolName::new(tool)))),
            natural_tool_rounds: forced_tool.is_none().then(|| StdMutex::new(0)),
        }
    }

    fn next(&self) -> ForcedToolOperation {
        if let Some(pending) = &self.pending {
            return pending
                .lock()
                .expect("forced-tool lock is available")
                .take()
                .map_or(
                    ForcedToolOperation::Continuation,
                    ForcedToolOperation::Force,
                );
        }
        let mut rounds = self
            .natural_tool_rounds
            .as_ref()
            .expect("natural sequence has a round counter")
            .lock()
            .expect("natural-round lock is available");
        if *rounds >= MAX_NATURAL_TOOL_EXCHANGES {
            return ForcedToolOperation::Continuation;
        }
        *rounds += 1;
        ForcedToolOperation::Natural
    }
}

impl EvalOpenAiRuntime {
    fn new(forced_tool: Option<&str>, tracker: OperationTracker) -> EvalResult<Self> {
        let mut config = OpenAiConfig::new();
        config.exchange_timeout = EXCHANGE_TIMEOUT;
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

struct ReceiptTrackingSink<'a> {
    inner: &'a mut (dyn ObservationSink<ModelCallId> + Send),
    response_text: String,
    proposed_tool_call: bool,
}

impl<'a> ReceiptTrackingSink<'a> {
    fn new(inner: &'a mut (dyn ObservationSink<ModelCallId> + Send)) -> Self {
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
struct OperationTracker {
    state: Arc<StdMutex<OperationTrackerState>>,
}

#[derive(Default)]
struct OperationTrackerState {
    seen_tool_call_ids: BTreeSet<String>,
    tool_results: Vec<TrackedToolResult>,
    result_round_trips: usize,
    round_tripped_request_ids: BTreeSet<Uuid>,
    pending_result_receipts: BTreeMap<Uuid, String>,
    result_contents: BTreeMap<Uuid, String>,
    final_response_text: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TrackedToolResult {
    request_id: Uuid,
    content: String,
    is_error: bool,
    round_tripped: bool,
}

impl OperationTracker {
    fn observe(&self, operation: &ModelOperation<ModelCallId>) {
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
                | MessagePart::RedactedThinking { .. } => None,
            })
        });
        self.record_new_results(tool_results);
    }

    fn record_new_results(
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

    fn observe_result(&self, request_id: Uuid, content: &str) {
        let Some(receipt) = eval_receipt(content) else {
            return;
        };
        self.state
            .lock()
            .expect("operation-tracker lock is available")
            .record_result(request_id, receipt, content);
    }

    fn observe_response_text(&self, text: &str, proposed_tool_call: bool) {
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

    fn tool_results(&self) -> Vec<TrackedToolResult> {
        self.state
            .lock()
            .expect("operation-tracker lock is available")
            .tool_results
            .clone()
    }

    fn result_round_trips(&self) -> usize {
        self.state
            .lock()
            .expect("operation-tracker lock is available")
            .result_round_trips
    }

    fn round_tripped_request_ids(&self) -> BTreeSet<Uuid> {
        self.state
            .lock()
            .expect("operation-tracker lock is available")
            .round_tripped_request_ids
            .clone()
    }

    fn result_content(&self, request_id: Uuid) -> Option<String> {
        self.state
            .lock()
            .expect("operation-tracker lock is available")
            .result_contents
            .get(&request_id)
            .cloned()
    }

    fn final_response_reports(&self, expected: &str) -> bool {
        self.state
            .lock()
            .expect("operation-tracker lock is available")
            .final_response_text
            .as_deref()
            .is_some_and(|text| text.contains(expected) && !report_denies_success(text))
    }

    fn final_response_reports_completion(&self) -> bool {
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
        report_affirms_completion(&report)
    }

    fn final_response_reports_file_creation(&self) -> bool {
        self.final_response_reports_completion()
            && self
                .state
                .lock()
                .expect("operation-tracker lock is available")
                .final_response_text
                .as_deref()
                .is_some_and(|report| !report_denies_file_changes(report))
    }
}

fn report_affirms_completion(report: &str) -> bool {
    let words = normalized_report_words(report);
    let has_completion = [
        "applied",
        "committed",
        "completed",
        "created",
        "done",
        "finished",
        "saved",
        "updated",
        "written",
        "wrote",
    ]
    .iter()
    .any(|word| words.iter().any(|observed| observed == *word));
    has_completion && !report_denies_success(report)
}

fn report_denies_success(report: &str) -> bool {
    let words = normalized_report_words(report);
    report_words_deny_success(report, &words)
}

fn report_denies_file_changes(report: &str) -> bool {
    let words = normalized_report_words(report);
    let no_file_change = words.iter().enumerate().any(|(index, word)| {
        word == "no"
            && words
                .get(index + 1)
                .is_some_and(|object| matches!(object.as_str(), "file" | "files"))
            && [
                "change", "changed", "changes", "created", "modified", "written",
            ]
            .iter()
            .any(|outcome| {
                words
                    .iter()
                    .skip(index + 2)
                    .take(4)
                    .any(|word| word == outcome)
            })
    });
    let no_modifications_made = words.windows(4).any(|claim| {
        claim[0] == "no"
            && matches!(claim[1].as_str(), "modification" | "modifications")
            && matches!(claim[2].as_str(), "was" | "were")
            && claim[3] == "made"
    });
    let no_modifications_have_been_made = words.windows(5).any(|claim| {
        claim[0] == "no"
            && matches!(claim[1].as_str(), "modification" | "modifications")
            && matches!(claim[2].as_str(), "has" | "have")
            && claim[3] == "been"
            && claim[4] == "made"
    });
    let no_existential_modifications = words.windows(4).any(|claim| {
        claim[0] == "there"
            && matches!(claim[1].as_str(), "was" | "were")
            && claim[2] == "no"
            && matches!(claim[3].as_str(), "modification" | "modifications")
    });
    no_file_change
        || no_modifications_made
        || no_modifications_have_been_made
        || no_existential_modifications
}

fn normalized_report_words(report: &str) -> Vec<String> {
    let normalized = report
        .to_ascii_lowercase()
        .replace("n’t", " not")
        .replace("n't", " not");
    normalized
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

fn report_words_deny_success(report: &str, words: &[String]) -> bool {
    let explicit_failure = words.iter().enumerate().any(|(index, word)| {
        ["cannot", "failed", "failure", "incomplete", "unable"].contains(&word.as_str())
            && !words[..index].iter().rev().take(2).any(|qualifier| {
                matches!(
                    qualifier.as_str(),
                    "never" | "no" | "not" | "nothing" | "without"
                )
            })
    });
    let negative_no_objects = [
        "answer",
        "change",
        "changes",
        "commit",
        "completion",
        "match",
        "result",
    ];
    let negative_no_claim = words
        .windows(2)
        .any(|pair| pair[0] == "no" && negative_no_objects.contains(&pair[1].as_str()));
    let negative_no_file_claim = words.iter().enumerate().any(|(index, word)| {
        word == "no"
            && words.get(index + 1).is_some_and(|object| object == "file")
            && ["created", "exists", "found", "written"]
                .iter()
                .any(|outcome| {
                    words
                        .iter()
                        .skip(index + 2)
                        .take(4)
                        .any(|word| word == outcome)
                })
    });
    let negative_nothing_claim = words.iter().enumerate().any(|(index, word)| {
        word == "nothing"
            && !words.get(index + 1).is_some_and(|qualifier| {
                matches!(qualifier.as_str(), "else" | "failed" | "failure" | "other")
            })
    });
    let negative_outcomes = [
        "applied",
        "commit",
        "committed",
        "change",
        "changed",
        "complete",
        "completed",
        "create",
        "created",
        "done",
        "find",
        "finish",
        "finished",
        "found",
        "match",
        "matched",
        "modified",
        "modify",
        "saved",
        "updated",
        "write",
        "written",
        "wrote",
    ];
    let deferred_completion = words.windows(3).any(|claim| {
        claim[0] == "yet" && claim[1] == "to" && negative_outcomes.contains(&claim[2].as_str())
    }) || words.windows(4).any(|claim| {
        claim[0] == "yet"
            && claim[1] == "to"
            && claim[2] == "be"
            && negative_outcomes.contains(&claim[3].as_str())
    });
    let scoped_negation = report
        .split([';', '.', ',', '!', '?', '\n'])
        .map(normalized_report_words)
        .any(|clause| {
            clause.iter().enumerate().any(|(index, word)| {
                let scope = &clause[index + 1..clause.len().min(index + 7)];
                let outcome = scope
                    .iter()
                    .position(|word| negative_outcomes.contains(&word.as_str()));
                let collateral_only = outcome.is_some_and(|outcome| {
                    let predicate_tail = &scope[outcome + 1..];
                    predicate_tail
                        .iter()
                        .position(|word| word == "other")
                        .is_some_and(|other| {
                            predicate_tail[..other].iter().all(|word| {
                                matches!(
                                    word.as_str(),
                                    "also"
                                        | "and"
                                        | "any"
                                        | "change"
                                        | "changed"
                                        | "create"
                                        | "created"
                                        | "modify"
                                        | "modified"
                                        | "or"
                                        | "the"
                                        | "write"
                                        | "written"
                                        | "wrote"
                                )
                            })
                        })
                });
                matches!(word.as_str(), "never" | "not") && outcome.is_some() && !collateral_only
            })
        });
    explicit_failure
        || negative_no_claim
        || negative_no_file_claim
        || negative_nothing_claim
        || deferred_completion
        || scoped_negation
}

impl OperationTrackerState {
    fn record_result(&mut self, request_id: Uuid, receipt: String, content: &str) {
        self.pending_result_receipts.insert(request_id, receipt);
        self.result_contents.insert(request_id, content.to_owned());
    }
}

fn eval_receipt(content: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(content)
        .ok()?
        .as_object()?
        .get(EVAL_RECEIPT_FIELD)?
        .as_str()
        .map(str::to_owned)
}

fn synthetic_result_with_receipt() -> String {
    serde_json::json!({EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT}).to_string()
}

#[test]
fn observing_an_untranslated_result_does_not_count_a_round_trip() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );

    assert_eq!(tracker.result_round_trips(), 0);
    assert!(tracker.round_tripped_request_ids().is_empty());
}

#[test]
fn unrelated_model_text_does_not_count_a_result_round_trip() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text("no tool receipt reported", false);

    assert_eq!(tracker.result_round_trips(), 0);
    assert!(tracker.round_tripped_request_ids().is_empty());
}

#[test]
fn model_text_echoing_the_tool_only_receipt_counts_the_exact_request() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_EVAL_RECEIPT, false);

    assert_eq!(tracker.result_round_trips(), 1);
    assert_eq!(
        tracker.round_tripped_request_ids(),
        BTreeSet::from([Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)])
    );
}

#[test]
fn intermediate_text_with_a_tool_call_does_not_count_a_result_round_trip() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_EVAL_RECEIPT, true);

    assert_eq!(tracker.result_round_trips(), 0);
    assert!(tracker.round_tripped_request_ids().is_empty());
}

#[test]
fn final_response_report_rejects_a_receipt_only_answer() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_EVAL_RECEIPT, false);

    assert!(!tracker.final_response_reports(WEB_FETCH_BODY));
    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn forced_case_completion_rejects_a_receipt_only_answer() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_EVAL_RECEIPT, false);

    assert!(!forced_case_completion_reported(true, &tracker));
}

#[test]
fn final_response_report_accepts_the_fetched_fixture() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    let response =
        format!("{SYNTHETIC_COMPLETION_REPORT} {WEB_FETCH_BODY} {SYNTHETIC_EVAL_RECEIPT}");
    tracker.observe_response_text(&response, false);

    assert!(tracker.final_response_reports(WEB_FETCH_BODY));
    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_an_explicit_failure() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_FAILURE_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_contracted_failure() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_CONTRACTED_FAILURE_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_never_completed() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_NEVER_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_deferred_completion() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_DEFERRED_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_an_applied_outcome() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_APPLIED_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_an_unapplied_outcome() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_APPLIED_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_negative_web_report() {
    let tracker = OperationTracker::default();
    let response = format!("I did not find the {WEB_FETCH_BODY}.");
    tracker.observe_response_text(&response, false);

    assert!(!tracker.final_response_reports(WEB_FETCH_BODY));
}

#[test]
fn final_response_report_accepts_completion_with_no_errors() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_ERRORS_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_completion_without_failure() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_WITHOUT_FAILURE_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_completion_after_no_failure() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FAILURE_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_completion_after_nothing_failed() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOTHING_FAILED_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_completion_with_no_file_changes() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILE_CHANGES_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn exec_file_creation_report_rejects_completion_with_no_file_changes() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILE_CHANGES_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_an_auxiliary_no_file_change() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILE_WAS_CHANGED_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_plural_no_file_change() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILES_WERE_MODIFIED_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_plural_no_file_creation() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILES_WERE_CREATED_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_verb_first_creation_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_VERB_FIRST_CREATION_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_verb_first_modification_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_VERB_FIRST_MODIFICATION_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_bare_no_changes_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_BARE_NO_CHANGES_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_bare_no_modifications_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_BARE_NO_MODIFICATIONS_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_perfect_tense_no_modifications_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(
        SYNTHETIC_PERFECT_TENSE_NO_MODIFICATIONS_DENIAL_REPORT,
        false,
    );

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_an_existential_no_modifications_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_EXISTENTIAL_NO_MODIFICATIONS_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn final_response_report_accepts_completion_with_scoped_negation() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SCOPED_NEGATION_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_completion_with_scoped_creation_negation() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SCOPED_CREATION_NEGATION_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_completion_with_a_collateral_conjunction() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(
        SYNTHETIC_SCOPED_CONJUNCTION_NEGATION_COMPLETION_REPORT,
        false,
    );

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_denial_before_a_collateral_clause() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SEPARATE_COLLATERAL_CLAUSE_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_no_file_written_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILE_WRITTEN_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_nothing_written_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOTHING_WRITTEN_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_no_result_web_report() {
    let tracker = OperationTracker::default();
    let response = format!("No result was found for {WEB_FETCH_BODY}.");
    tracker.observe_response_text(&response, false);

    assert!(!tracker.final_response_reports(WEB_FETCH_BODY));
}

struct EvalDatabase {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
    selection: DirectModelSelection,
    targets: ModelTargetCatalog,
    credential_families: ModelCredentialFamilyCatalog,
    runtime_models: RuntimeModelCatalog,
}

impl EvalDatabase {
    async fn start(model: &str) -> EvalResult<Self> {
        let container = Postgres::default()
            .with_db_name(DATABASE_NAME)
            .with_user(DATABASE_USER)
            .with_password(DATABASE_PASSWORD)
            .with_fsync_enabled()
            .with_tag(POSTGRES_IMAGE_TAG)
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

    async fn start_turn(
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

    async fn decide_pending_unsandboxed_requests(
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

#[derive(Clone, Copy)]
enum ExecApprovalMode {
    DenyAll,
    ApproveOneExactForced,
}

#[derive(Clone, Copy)]
enum ExecApprovalCap {
    NotReached,
    Reached,
}

struct ExecApprovalState {
    mode: ExecApprovalMode,
    exact_forced_approved: bool,
}

impl ExecApprovalState {
    const fn new(mode: ExecApprovalMode) -> Self {
        Self {
            mode,
            exact_forced_approved: false,
        }
    }

    fn decision(
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

fn eval_session_credential_pin() -> SessionCredentialPin {
    SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        OPENAI_MODEL_FAMILY,
        EXPECTED_OPENAI_CREDENTIAL_REFERENCE,
    )])
    .expect("the eval credential pin is valid")
}

const fn default_configuration() -> PerInputConfigurationChoices {
    PerInputConfigurationChoices::new(
        SessionConfigurationDefaultsVersion::first(),
        ModelSelectionOverride::UseSessionDefault,
    )
}

struct CaseSnapshot {
    turn_disposition: SnapshotTurnDisposition,
    requests: Vec<RequestSnapshot>,
    model_calls: i64,
}

struct RequestSnapshot {
    request_id: Uuid,
    producing_model_call_id: Uuid,
    entry_index: u64,
    completed_result_entry_index: Option<u64>,
    name: String,
    arguments_text: String,
    attempt_succeeded: bool,
}

impl RequestSnapshot {
    fn arguments(&self) -> Option<serde_json::Value> {
        serde_json::from_str(&self.arguments_text).ok()
    }
}

impl CaseSnapshot {
    async fn read(
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
                }),
                ProcessTranscriptEntry::AssistantToolUse { .. }
                | ProcessTranscriptEntry::DelegatedTask { .. }
                | ProcessTranscriptEntry::DelegationMessage { .. }
                | ProcessTranscriptEntry::DelegationResult { .. }
                | ProcessTranscriptEntry::ModelIdentityChanged { .. }
                | ProcessTranscriptEntry::ContextSummary { .. }
                | ProcessTranscriptEntry::User { .. }
                | ProcessTranscriptEntry::Assistant { .. }
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

    fn called_names(&self) -> String {
        if self.requests.is_empty() {
            return String::from("none");
        }
        self.requests
            .iter()
            .map(|request| request.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn workspace_natural_requests_passed(&self) -> bool {
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

    fn git_natural_requests_passed(&self) -> EvalResult<bool> {
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

    fn web_natural_request_pair(&self) -> EvalResult<Option<(&RequestSnapshot, &RequestSnapshot)>> {
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

    fn web_natural_requests_passed(&self) -> EvalResult<bool> {
        Ok(self.web_natural_request_pair()?.is_some())
    }

    fn exact_natural_request_failed(&self, family: EvalFamily) -> bool {
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
                                    && matches!(
                                        earlier.name.as_str(),
                                        WRITE_FILE_NAME | EDIT_FILE_NAME | APPLY_PATCH_NAME
                                    )
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

    fn exact_git_natural_request_failed(
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

fn exact_git_natural_stage_arguments(request: &RequestSnapshot) -> bool {
    request
        .arguments()
        .is_some_and(|arguments| arguments == serde_json::json!({"paths": [GIT_NATURAL_PATH]}))
}

fn workspace_read_covers_seed(arguments: &serde_json::Value) -> bool {
    let Ok(arguments) = serde_json::from_value::<ReadFileArguments>(arguments.clone()) else {
        return false;
    };
    arguments.path == WORKSPACE_SEED_PATH
        && arguments.max_bytes >= WORKSPACE_SEED.len()
        && arguments.max_bytes <= MAX_WORKSPACE_READ_BYTES
}

fn workspace_natural_read_result_passed(
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
            "bytes_read",
            "total_bytes",
            "truncated",
            EVAL_RECEIPT_FIELD,
        ],
    ) && result["path"] == WORKSPACE_SEED_PATH
        && result["content"] == WORKSPACE_SEED
        && result["bytes_read"] == WORKSPACE_SEED.len()
        && result["total_bytes"] == WORKSPACE_SEED.len()
        && result["truncated"] == false
}

fn workspace_natural_write_result_passed(
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

fn workspace_natural_result_payloads_passed(
    snapshot: &CaseSnapshot,
    tracker: &OperationTracker,
) -> bool {
    workspace_natural_read_result_passed(snapshot, tracker)
        && workspace_natural_write_result_passed(snapshot, tracker)
}

fn completed_tool_result_entry_indices(entries: &[ProcessTranscriptEntry]) -> BTreeMap<Uuid, u64> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            ProcessTranscriptEntry::ToolExecutionResult {
                entry_index,
                request,
                disposition: ProcessToolExecutionResultDisposition::Completed,
                ..
            } => Some((request.into_uuid(), *entry_index)),
            ProcessTranscriptEntry::ToolExecutionResult {
                disposition: ProcessToolExecutionResultDisposition::KnownFailed,
                ..
            }
            | ProcessTranscriptEntry::DelegatedTask { .. }
            | ProcessTranscriptEntry::DelegationMessage { .. }
            | ProcessTranscriptEntry::DelegationResult { .. }
            | ProcessTranscriptEntry::ModelIdentityChanged { .. }
            | ProcessTranscriptEntry::ContextSummary { .. }
            | ProcessTranscriptEntry::User { .. }
            | ProcessTranscriptEntry::Assistant { .. }
            | ProcessTranscriptEntry::AssistantToolUse { .. }
            | ProcessTranscriptEntry::ToolDenied { .. }
            | ProcessTranscriptEntry::ToolClosed { .. }
            | ProcessTranscriptEntry::TurnFailed { .. }
            | ProcessTranscriptEntry::TurnCompleted { .. }
            | ProcessTranscriptEntry::TurnCancelled { .. }
            | ProcessTranscriptEntry::ImportedText { .. }
            | ProcessTranscriptEntry::Imported { .. } => None,
        })
        .collect()
}

fn successful_tool_requests(entries: &[ProcessTranscriptEntry]) -> BTreeSet<Uuid> {
    completed_tool_result_entry_indices(entries)
        .into_keys()
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotTurnDisposition {
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
    fn from_process_state(state: &ProcessTurnState) -> Self {
        match state {
            ProcessTurnState::Completed { .. } => Self::Completed,
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
            | ProcessTurnState::Cancelled { .. }
            | ProcessTurnState::ReconciliationRequired { .. } => Self::Infrastructure,
            ProcessTurnState::Refused { .. } => Self::Refused,
        }
    }

    const fn from_failed_model_call(
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

    const fn is_completed(self) -> bool {
        match self {
            Self::Completed => true,
            Self::ApprovalCapReached
            | Self::ProviderFailure(_)
            | Self::Infrastructure
            | Self::Refused => false,
        }
    }

    const fn is_infrastructure(self) -> bool {
        match self {
            Self::ProviderFailure(_) | Self::Infrastructure => true,
            Self::Completed | Self::ApprovalCapReached | Self::Refused => false,
        }
    }

    /// Renders the turn cell, naming the closed provider cause when the daemon
    /// retained one so a paid run reports why the exchange never happened.
    fn label(self) -> String {
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
const fn provider_failure_cause_label(cause: ProcessProviderModelCallFailureCause) -> &'static str {
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

struct CaseOutcome {
    target: Option<String>,
    expected_arguments: Option<String>,
    execution_completed: bool,
    tool_results: Vec<TrackedToolResult>,
    snapshot: CaseSnapshot,
}

fn exact_exec_natural_arguments(request: &RequestSnapshot) -> bool {
    let Some(arguments) = request.arguments() else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(EXEC_NATURAL_ARGUMENTS)
        .is_ok_and(|expected| arguments == expected)
}

impl CaseOutcome {
    fn round_tripped_result_count(&self) -> usize {
        round_tripped_result_count(&self.tool_results)
    }

    fn forced_disposition(&self) -> EvalDisposition {
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
        if self.snapshot.requests.len() == 1
            && self.snapshot.requests[0].name == target
            && self.snapshot.requests[0].arguments_text == expected_arguments
            && !self.snapshot.requests[0].attempt_succeeded
        {
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

    fn natural_loop_disposition(&self, family: EvalFamily) -> EvalDisposition {
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
    fn natural_exec_result_passed(&self) -> bool {
        let [result] = self.tool_results.as_slice() else {
            return false;
        };
        if result.is_error {
            return false;
        }
        let Ok(execution) = serde_json::from_str::<serde_json::Value>(&result.content) else {
            return false;
        };
        execution["confinement"]["kind"] == "filesystem_confined"
            && exited_cleanly(&execution)
            && captured_stdout_is(&execution, EXEC_NATURAL_OUTPUT)
            && captured_stderr_is_empty(&execution)
    }

    fn forced_result_passed(&self, target: &str) -> bool {
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
        if target == CARGO_DIAGNOSTICS_NAME {
            return cargo_diagnostics_result_passed(&result);
        }
        let execution = &result;
        let expected_confinement = match target {
            SANDBOXED_EXEC_NAME => "filesystem_confined",
            UNSANDBOXED_EXEC_NAME => "unsandboxed",
            _ => return false,
        };
        if execution["confinement"]["kind"] != expected_confinement || !exited_cleanly(execution) {
            return false;
        }
        let stdout_matches = match target {
            SANDBOXED_EXEC_NAME => captured_stdout_is(execution, EXEC_FORCED_SANDBOXED_OUTPUT),
            UNSANDBOXED_EXEC_NAME => captured_stdout_is(execution, EXEC_FORCED_READ_ONLY_OUTPUT),
            _ => false,
        };
        stdout_matches && captured_stderr_is_empty(execution)
    }
}

fn round_tripped_result_count(results: &[TrackedToolResult]) -> usize {
    results.iter().filter(|result| result.round_tripped).count()
}

/// Whether a serialized direct-command or Cargo result reports a runner failure
/// before or around execution, rather than evidence about the requested task.
fn exec_result_is_infrastructure(result: &TrackedToolResult) -> bool {
    if result.is_error {
        return false;
    }
    let Ok(result) = serde_json::from_str::<serde_json::Value>(&result.content) else {
        return false;
    };
    let execution = result.get("execution").unwrap_or(&result);
    matches!(
        execution["confinement"]["kind"].as_str(),
        Some("sandbox_refused" | "sandbox_setup_failed")
    ) || matches!(
        execution["outcome"]["kind"].as_str(),
        Some("spawn_failed" | "supervision_failed")
    ) || execution
        .get("preparation_failure")
        .is_some_and(|failure| !failure.is_null())
}

/// Whether one serialized execution reports a zero-code process exit.
fn exited_cleanly(execution: &serde_json::Value) -> bool {
    execution["outcome"]["kind"] == "exited" && execution["outcome"]["code"] == 0
}

/// Whether one serialized execution captured exactly the expected standard
/// output, complete and undamaged.
///
/// A zero exit code alone proves only that the process ended well; argument
/// forwarding or output capture could still have regressed to empty or wrong
/// bytes while the eval reported a pass.
fn captured_stdout_is(execution: &serde_json::Value, expected: &str) -> bool {
    execution["stdout"]["text"] == expected
        && execution["stdout"]["completeness"] == "complete"
        && execution["stdout"]["encoding"] == "utf8"
}

/// Whether one serialized execution captured an empty, complete, undamaged
/// standard-error stream, as the direct forced fixtures promise.
fn captured_stderr_is_empty(execution: &serde_json::Value) -> bool {
    execution["stderr"]["text"] == ""
        && execution["stderr"]["completeness"] == "complete"
        && execution["stderr"]["encoding"] == "utf8"
}

/// The complete result shape needed before one successful Cargo diagnostics
/// exchange can count as forced-tier evidence.
#[derive(serde::Deserialize)]
struct CargoDiagnosticsEvalResult {
    command: String,
    execution: CargoDiagnosticsEvalExecution,
    diagnostics: CargoDiagnosticsEvalRecords,
    tests: CargoDiagnosticsEvalRecords,
}

#[derive(serde::Deserialize)]
struct CargoDiagnosticsEvalExecution {
    confinement: CargoDiagnosticsEvalConfinement,
    outcome: CargoDiagnosticsEvalOutcome,
    stdout: CargoDiagnosticsEvalStream,
    stderr: CargoDiagnosticsEvalStream,
    cargo_failure: serde_json::Value,
    preparation_failure: serde_json::Value,
}

#[derive(serde::Deserialize)]
struct CargoDiagnosticsEvalConfinement {
    kind: String,
}

#[derive(serde::Deserialize)]
struct CargoDiagnosticsEvalOutcome {
    kind: String,
    code: Option<i64>,
}

#[derive(serde::Deserialize)]
struct CargoDiagnosticsEvalStream {
    completeness: String,
    encoding: String,
}

#[derive(serde::Deserialize)]
struct CargoDiagnosticsEvalRecords {
    #[serde(rename = "values")]
    values: Vec<serde_json::Value>,
    limit_reached: bool,
    provenance: String,
    known_truncated: bool,
}

#[derive(serde::Deserialize)]
struct CargoDiagnosticsEvalDiagnostic {
    file: serde_json::Value,
    file_completeness: String,
    span: serde_json::Value,
    level: String,
    level_completeness: String,
    message: String,
    message_completeness: String,
}

#[derive(serde::Deserialize)]
struct CargoDiagnosticsEvalSpan {
    line_start: u64,
    column_start: u64,
    line_end: u64,
    column_end: u64,
}

/// Whether Cargo diagnostics returned the requested successful pass together
/// with every execution and record envelope the tool promises.
fn cargo_diagnostics_result_passed(result: &serde_json::Value) -> bool {
    let Ok(result) = serde_json::from_value::<CargoDiagnosticsEvalResult>(result.clone()) else {
        return false;
    };
    result.command == "check"
        && result.execution.confinement.kind == "filesystem_confined"
        && result.execution.outcome.kind == "exited"
        && result.execution.outcome.code == Some(0)
        && cargo_diagnostics_stream_is_valid(&result.execution.stdout)
        && cargo_diagnostics_stream_is_valid(&result.execution.stderr)
        && result.execution.cargo_failure.is_null()
        && result.execution.preparation_failure.is_null()
        && result.diagnostics.provenance == "workspace_influenced"
        && !result.diagnostics.limit_reached
        && !result.diagnostics.known_truncated
        && result
            .diagnostics
            .values
            .iter()
            .all(cargo_diagnostic_record_is_success_evidence)
        && result.tests.provenance == "workspace_influenced"
        && result.tests.values.is_empty()
        && !result.tests.limit_reached
        && !result.tests.known_truncated
}

fn cargo_diagnostic_record_is_success_evidence(record: &serde_json::Value) -> bool {
    let Ok(diagnostic) = serde_json::from_value::<CargoDiagnosticsEvalDiagnostic>(record.clone())
    else {
        return false;
    };
    cargo_diagnostic_location_is_valid(&diagnostic)
        && diagnostic.level_completeness == "complete"
        && !diagnostic.level.is_empty()
        && diagnostic.level != CARGO_ERROR_DIAGNOSTIC_LEVEL
        && !diagnostic.message.is_empty()
        && cargo_diagnostic_completeness_is_valid(&diagnostic.message_completeness)
}

fn cargo_diagnostic_location_is_valid(diagnostic: &CargoDiagnosticsEvalDiagnostic) -> bool {
    match (diagnostic.file.as_str(), diagnostic.span.is_null()) {
        (Some(file), false) => {
            !file.is_empty()
                && cargo_diagnostic_completeness_is_valid(&diagnostic.file_completeness)
                && cargo_diagnostic_span_is_valid(&diagnostic.span)
        }
        (None, true) if diagnostic.file.is_null() => diagnostic.file_completeness == "complete",
        _ => false,
    }
}

fn cargo_diagnostic_completeness_is_valid(completeness: &str) -> bool {
    matches!(completeness, "complete" | "truncated")
}

fn cargo_diagnostic_span_is_valid(span: &serde_json::Value) -> bool {
    if span.is_null() {
        return true;
    }
    let Ok(span) = serde_json::from_value::<CargoDiagnosticsEvalSpan>(span.clone()) else {
        return false;
    };
    span.line_start > 0
        && span.column_start > 0
        && span.line_end >= span.line_start
        && span.column_end > 0
        && (span.line_end > span.line_start || span.column_end >= span.column_start)
}

fn cargo_diagnostics_stream_is_valid(stream: &CargoDiagnosticsEvalStream) -> bool {
    stream.completeness == "complete" && stream.encoding == "utf8"
}

fn synthetic_tool_result(
    disposition: ProcessToolExecutionResultDisposition,
) -> ProcessTranscriptEntry {
    ProcessTranscriptEntry::ToolExecutionResult {
        entry_index: 0,
        source_session: SessionId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_SESSION_ID)),
        entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_ENTRY_ID)),
        request: ToolRequestId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)),
        attempt: ToolAttemptId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_ATTEMPT_ID)),
        disposition,
        content: String::from("synthetic tool result"),
    }
}

#[test]
fn successful_tool_requests_accepts_a_typed_completed_result() {
    let entries = [synthetic_tool_result(
        ProcessToolExecutionResultDisposition::Completed,
    )];

    assert_eq!(
        successful_tool_requests(&entries),
        BTreeSet::from([Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)])
    );
}

#[test]
fn successful_tool_requests_rejects_a_typed_known_failure() {
    let entries = [synthetic_tool_result(
        ProcessToolExecutionResultDisposition::KnownFailed,
    )];

    assert!(successful_tool_requests(&entries).is_empty());
}

#[test]
fn turn_snapshot_reports_ambiguous_model_recovery_as_infrastructure() {
    let state = ProcessTurnState::ActiveAwaitingModelCallRecovery {
        ended_attempt: TurnAttemptId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_TURN_ATTEMPT_ID)),
        recovery_call: ModelCallId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID)),
    };

    assert_eq!(
        SnapshotTurnDisposition::from_process_state(&state),
        SnapshotTurnDisposition::Infrastructure
    );
}

#[test]
fn turn_snapshot_reports_target_resolution_failure_as_infrastructure() {
    let state = ProcessTurnState::Failed {
        terminal_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(
            ARBITRARY_EVAL_FRONTIER_ID,
        )),
        terminal_attempt: None,
        terminal_model_call: None,
    };

    assert_eq!(
        SnapshotTurnDisposition::from_process_state(&state),
        SnapshotTurnDisposition::Infrastructure
    );
}

#[test]
fn turn_snapshot_reports_parked_tool_approval_as_infrastructure() {
    let state = ProcessTurnState::ActiveAwaitingToolApproval {
        request: ToolRequestId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)),
    };

    assert_eq!(
        SnapshotTurnDisposition::from_process_state(&state),
        SnapshotTurnDisposition::Infrastructure
    );
}

#[test]
fn turn_snapshot_reports_a_terminal_provider_failure_distinctly() {
    assert_eq!(
        SnapshotTurnDisposition::from_failed_model_call(Some((
            ProcessFailedModelCallDisposition::KnownFailed,
            None
        ))),
        SnapshotTurnDisposition::ProviderFailure(None)
    );
}

#[test]
fn turn_snapshot_reports_failed_without_a_model_call_as_infrastructure() {
    assert_eq!(
        SnapshotTurnDisposition::from_failed_model_call(None),
        SnapshotTurnDisposition::Infrastructure
    );
}

#[test]
fn turn_snapshot_retains_the_closed_provider_failure_cause() {
    assert_eq!(
        SnapshotTurnDisposition::from_failed_model_call(Some((
            ProcessFailedModelCallDisposition::KnownFailed,
            Some(ProcessProviderModelCallFailureCause::TargetNotFound)
        ))),
        SnapshotTurnDisposition::ProviderFailure(Some(
            ProcessProviderModelCallFailureCause::TargetNotFound
        ))
    );
}

#[test]
fn a_cancelled_terminal_model_call_reports_infrastructure() {
    assert_eq!(
        SnapshotTurnDisposition::from_failed_model_call(Some((
            ProcessFailedModelCallDisposition::Cancelled,
            None
        ))),
        SnapshotTurnDisposition::Infrastructure
    );
}

#[test]
fn the_turn_cell_names_the_closed_provider_failure_cause() {
    let disposition = SnapshotTurnDisposition::ProviderFailure(Some(
        ProcessProviderModelCallFailureCause::TargetNotFound,
    ));

    assert_eq!(disposition.label(), "provider failure: target not found");
}

#[test]
fn the_turn_cell_of_an_unclassified_provider_failure_stays_bare() {
    let disposition = SnapshotTurnDisposition::ProviderFailure(None);

    assert_eq!(disposition.label(), "provider failure");
}

#[test]
fn provider_failure_is_reported_as_infrastructure_not_a_model_miss() {
    let outcome = CaseOutcome {
        target: Some(String::from(GIT_STATUS_NAME)),
        expected_arguments: Some(String::from("{}")),
        execution_completed: true,
        tool_results: Vec::new(),
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::ProviderFailure(Some(
                ProcessProviderModelCallFailureCause::TargetNotFound,
            )),
            requests: Vec::new(),
            model_calls: 1,
        },
    };

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Git),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_tier_passes_one_completed_target_with_a_result_round_trip() {
    let target = GIT_STATUS_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from("{}")),
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("fixture result"),
            is_error: false,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(target),
                arguments_text: String::from("{}"),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Pass);
}

#[test]
fn forced_tier_reports_a_miss_without_result_round_trip() {
    let target = GIT_STATUS_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from("{}")),
        execution_completed: true,
        tool_results: Vec::new(),
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(target),
                arguments_text: String::from("{}"),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_tier_reports_infrastructure_for_an_exact_known_failed_attempt() {
    let target = GIT_STATUS_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from("{}")),
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("fixture result"),
            is_error: false,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(target),
                arguments_text: String::from("{}"),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: None,
                attempt_succeeded: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_exec_tier_reports_a_normalized_exact_failure_as_infrastructure() -> EvalResult {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("fixture result"),
            is_error: true,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(SANDBOXED_EXEC_NAME),
                arguments_text: normalized_arguments_text(EXEC_NATURAL_ARGUMENTS)?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: None,
                attempt_succeeded: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
    Ok(())
}

#[test]
fn unforced_web_tier_reports_infrastructure_for_an_exact_known_failed_attempt() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("fixture result"),
            is_error: true,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_SEARCH_NAME),
                arguments_text: serde_json::json!({"query": WEB_QUERY}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: None,
                attempt_succeeded: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Web),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_git_tier_reports_infrastructure_for_an_exact_known_failed_attempt() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("fixture result"),
            is_error: true,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: None,
                attempt_succeeded: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Git),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_git_tier_reports_a_premature_commit_failure_as_a_miss() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("fixture result"),
            is_error: true,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: None,
                attempt_succeeded: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Git),
        EvalDisposition::Miss
    );
}

#[test]
fn unforced_git_tier_reports_a_post_stage_commit_failure_as_infrastructure() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("fixture result"),
            is_error: true,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(GIT_STAGE_NAME),
                    arguments_text: serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                    attempt_succeeded: true,
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                    name: String::from(GIT_CREATE_COMMIT_NAME),
                    arguments_text: serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: None,
                    attempt_succeeded: false,
                },
            ],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Git),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_git_tier_keeps_a_duplicate_commit_failure_as_a_miss() {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_ATTEMPT_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: None,
                attempt_succeeded: false,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.exact_natural_request_failed(EvalFamily::Git));
}

#[test]
fn unforced_workspace_tier_reports_infrastructure_for_an_exact_known_failed_attempt() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("fixture result"),
            is_error: true,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WRITE_FILE_NAME),
                arguments_text: serde_json::json!({
                    "path": WORKSPACE_ANSWER_PATH,
                    "content": WORKSPACE_ANSWER,
                })
                .to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: None,
                attempt_succeeded: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Workspace),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_workspace_tier_keeps_a_model_caused_read_failure_as_a_miss() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("fixture result"),
            is_error: false,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(APPLY_PATCH_NAME),
                    arguments_text: String::from("{}"),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                    attempt_succeeded: true,
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                    name: String::from(READ_FILE_NAME),
                    arguments_text: serde_json::json!({"path": WORKSPACE_SEED_PATH}).to_string(),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: None,
                    attempt_succeeded: false,
                },
            ],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Workspace),
        EvalDisposition::Miss
    );
}

#[test]
fn unforced_workspace_tier_rejects_more_than_the_bounded_model_calls() {
    let first = Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID);
    let second = Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID);
    let third = Uuid::from_u128(ARBITRARY_THIRD_EVAL_REQUEST_ID);
    let fourth = Uuid::from_u128(ARBITRARY_FOURTH_EVAL_REQUEST_ID);
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        tool_results: vec![
            round_tripped_fixture_result(first),
            round_tripped_fixture_result(second),
            round_tripped_fixture_result(third),
            round_tripped_fixture_result(fourth),
        ],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![
                successful_request(first, LIST_DIRECTORY_NAME, serde_json::json!({"path": "."})),
                successful_request(second, GLOB_FILES_NAME, serde_json::json!({"pattern": "*"})),
                successful_request(
                    third,
                    READ_FILE_NAME,
                    serde_json::json!({"path": WORKSPACE_SEED_PATH}),
                ),
                successful_request(
                    fourth,
                    WRITE_FILE_NAME,
                    serde_json::json!({
                        "path": WORKSPACE_ANSWER_PATH,
                        "content": WORKSPACE_ANSWER,
                    }),
                ),
            ],
            model_calls: MAX_NATURAL_MODEL_CALLS + 1,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Workspace),
        EvalDisposition::Miss
    );
}

fn round_tripped_fixture_result(request_id: Uuid) -> TrackedToolResult {
    TrackedToolResult {
        request_id,
        content: String::from("fixture result"),
        is_error: false,
        round_tripped: true,
    }
}

fn successful_request(
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
    }
}

fn denied_unsandboxed_request(request_id: Uuid) -> RequestSnapshot {
    RequestSnapshot {
        request_id,
        producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
        name: String::from(UNSANDBOXED_EXEC_NAME),
        arguments_text: String::from("{}"),
        entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
        completed_result_entry_index: None,
        attempt_succeeded: false,
    }
}

fn failed_request_snapshot(name: &str, arguments: serde_json::Value) -> CaseSnapshot {
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
        }],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    }
}

fn bounded_workspace_read_arguments() -> serde_json::Value {
    serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "max_bytes": WORKSPACE_SEED.len(),
    })
}

#[test]
fn unforced_git_tier_keeps_a_schema_invalid_extra_field_as_a_miss() {
    let snapshot = failed_request_snapshot(
        GIT_STAGE_NAME,
        serde_json::json!({"paths": [GIT_NATURAL_PATH], "unexpected": true}),
    );

    assert!(!snapshot.exact_natural_request_failed(EvalFamily::Git));
}

#[test]
fn unforced_workspace_tier_keeps_an_out_of_range_read_as_a_miss() {
    let snapshot = failed_request_snapshot(
        READ_FILE_NAME,
        serde_json::json!({"path": WORKSPACE_SEED_PATH, "max_bytes": 0}),
    );

    assert!(!snapshot.exact_natural_request_failed(EvalFamily::Workspace));
}

#[test]
fn unforced_workspace_tier_reports_a_covering_bounded_read_failure_as_infrastructure() {
    let snapshot = failed_request_snapshot(READ_FILE_NAME, bounded_workspace_read_arguments());

    assert!(snapshot.exact_natural_request_failed(EvalFamily::Workspace));
}

#[test]
fn unforced_workspace_tier_keeps_an_unknown_read_field_as_a_miss() {
    let mut arguments = bounded_workspace_read_arguments();
    arguments["unexpected"] = serde_json::json!(true);
    let snapshot = failed_request_snapshot(READ_FILE_NAME, arguments);

    assert!(!snapshot.exact_natural_request_failed(EvalFamily::Workspace));
}

#[test]
fn unforced_workspace_tier_keeps_a_malformed_read_bound_as_a_miss() {
    let mut arguments = bounded_workspace_read_arguments();
    arguments["max_bytes"] = serde_json::json!("many");
    let snapshot = failed_request_snapshot(READ_FILE_NAME, arguments);

    assert!(!snapshot.exact_natural_request_failed(EvalFamily::Workspace));
}

#[test]
fn unforced_workspace_tier_keeps_an_undersized_read_bound_as_a_miss() {
    let mut arguments = bounded_workspace_read_arguments();
    arguments["max_bytes"] = serde_json::json!(WORKSPACE_SEED.len() - 1);
    let snapshot = failed_request_snapshot(READ_FILE_NAME, arguments);

    assert!(!snapshot.exact_natural_request_failed(EvalFamily::Workspace));
}

#[test]
fn unforced_workspace_tier_keeps_an_oversized_read_bound_as_a_miss() {
    let mut arguments = bounded_workspace_read_arguments();
    arguments["max_bytes"] = serde_json::json!(MAX_WORKSPACE_READ_BYTES + 1);
    let snapshot = failed_request_snapshot(READ_FILE_NAME, arguments);

    assert!(!snapshot.exact_natural_request_failed(EvalFamily::Workspace));
}

#[test]
fn unforced_web_tier_keeps_a_schema_invalid_extra_field_as_a_miss() {
    let snapshot = failed_request_snapshot(
        WEB_FETCH_NAME,
        serde_json::json!({"url": WEB_URL, "unexpected": true}),
    );

    assert!(!snapshot.exact_natural_request_failed(EvalFamily::Web));
}

#[test]
fn unforced_workspace_tier_requires_each_request_result_to_round_trip() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("fixture result"),
            is_error: false,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
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
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                    name: String::from(WRITE_FILE_NAME),
                    arguments_text: serde_json::json!({
                        "path": WORKSPACE_ANSWER_PATH,
                        "content": WORKSPACE_ANSWER,
                    })
                    .to_string(),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                    attempt_succeeded: true,
                },
            ],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Workspace),
        EvalDisposition::Miss
    );
}

#[test]
fn unforced_git_tier_requires_both_task_tools() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("fixture result"),
            is_error: false,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Git),
        EvalDisposition::Miss
    );
}

#[test]
fn git_natural_state_rejects_a_commit_with_an_unrelated_fixture() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    stage_path(workspace.path(), GIT_STAGE_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_commit_with_drifted_bytes() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    fs::write(
        workspace.path().join(GIT_NATURAL_PATH),
        GIT_DRIFTED_NATURAL_CONTENT,
    )?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_skip_worktree_index_drift() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let repository = Repository::open(workspace.path())?;
    let mut index = repository.index()?;
    let mut entry = index
        .get_path(Path::new(GIT_SEED_PATH), 0)
        .expect("the seeded index entry exists");
    entry.flags |= GIT_INDEX_EXTENDED_FLAG;
    entry.flags_extended |= GIT_INDEX_SKIP_WORKTREE_FLAG;
    index.add(&entry)?;
    index.write()?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn git_natural_state_rejects_an_executable_committed_fixture() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    let path = workspace.path().join(GIT_NATURAL_PATH);
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(permissions.mode() | USER_EXECUTE_MODE_BIT);
    fs::set_permissions(&path, permissions)?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_retained_operation_state() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    fs::write(
        Repository::open(workspace.path())?
            .path()
            .join(GIT_CHERRY_PICK_HEAD_PATH),
        format!("{seed}\n"),
    )?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_extra_staging_after_the_target_commit() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    stage_path(workspace.path(), GIT_STAGE_PATH)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_missing_branch_reflog_record() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let branch_log = Repository::open(workspace.path())?
        .path()
        .join(GIT_LOGS_DIRECTORY)
        .join("refs/heads")
        .join(GIT_BASE_BRANCH);
    let contents = fs::read(&branch_log)?;
    let previous_record_end = contents[..contents.len() - 1]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .expect("the seeded branch reflog has a previous record");
    fs::write(&branch_log, &contents[..=previous_record_end])?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_branch_reflog_timezone_drift() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let repository = Repository::open(workspace.path())?;
    let commit = repository.head()?.peel_to_commit()?;
    let commit_time = commit.committer().when();
    let altered_time = Time::new(
        commit_time.seconds(),
        commit_time.offset_minutes().saturating_add(1),
    );
    let altered_signature = Signature::new(GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL, &altered_time)?;
    replace_latest_reflog_signature(
        &repository,
        &format!("refs/heads/{GIT_BASE_BRANCH}"),
        &altered_signature,
    )?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_deleted_unrelated_fixture() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    fs::remove_file(workspace.path().join(GIT_STAGE_PATH))?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn git_natural_state_rejects_a_symlinked_untracked_fixture() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let support = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let replacement = support.path().join(GIT_STAGE_PATH);
    fs::write(&replacement, GIT_STAGE_CONTENT)?;
    fs::remove_file(workspace.path().join(GIT_STAGE_PATH))?;
    symlink(&replacement, workspace.path().join(GIT_STAGE_PATH))?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_repository_config_drift() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    Repository::open(workspace.path())?
        .config()?
        .set_str(SYNTHETIC_GIT_CONFIG_KEY, SYNTHETIC_GIT_CONFIG_VALUE)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn git_natural_state_rejects_metadata_root_mode_drift() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let metadata_root = Repository::open(workspace.path())?.path().to_path_buf();
    let mut permissions = fs::metadata(&metadata_root)?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(&metadata_root, permissions)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn git_natural_state_rejects_top_level_metadata_directory_mode_drift() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let objects = Repository::open(workspace.path())?
        .path()
        .join(GIT_OBJECTS_DIRECTORY);
    let mut permissions = fs::metadata(&objects)?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(&objects, permissions)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_collateral_untracked_file() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    fs::write(
        workspace.path().join(GIT_COLLATERAL_PATH),
        GIT_COLLATERAL_CONTENT,
    )?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_collateral_empty_directory() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    fs::create_dir(workspace.path().join(GIT_COLLATERAL_DIRECTORY))?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn git_natural_state_rejects_a_collateral_pre_commit_hook() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let hook = Repository::open(workspace.path())?
        .path()
        .join(GIT_PRE_COMMIT_HOOK_PATH);
    fs::write(&hook, GIT_PRE_COMMIT_HOOK_CONTENT)?;
    let mut permissions = fs::metadata(&hook)?.permissions();
    permissions.set_mode(permissions.mode() | USER_EXECUTE_MODE_BIT);
    fs::set_permissions(&hook, permissions)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_collateral_ref_update() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let repository = Repository::open(workspace.path())?;
    let head = repository.head()?.peel_to_commit()?;
    repository
        .find_reference("refs/heads/log-target")?
        .set_target(head.id(), "synthetic collateral ref update")?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_collateral_tag() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let repository = Repository::open(workspace.path())?;
    let head = repository.head()?.peel_to_commit()?;
    repository.reference(
        "refs/tags/collateral-eval-tag",
        head.id(),
        true,
        "synthetic collateral tag",
    )?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_collateral_object() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    Repository::open(workspace.path())?.blob(GIT_COLLATERAL_OBJECT_CONTENT)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_the_wrong_commit_identity() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths_with_identity(
        workspace.path(),
        GIT_NATURAL_MESSAGE,
        SYNTHETIC_OTHER_GIT_AUTHOR_NAME,
        SYNTHETIC_OTHER_GIT_AUTHOR_EMAIL,
    )?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_an_unrelated_earlier_commit() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_STAGE_PATH)?;
    commit_staged_paths(workspace.path(), "unrelated eval commit")?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_parentless_seed_commit() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_commit_on_a_switched_branch() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    let repository = Repository::open(workspace.path())?;
    let head = repository.head()?.peel_to_commit()?;
    repository.branch("natural-target", &head, false)?;
    repository.set_head("refs/heads/natural-target")?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn forced_case_validation_rejects_schema_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    let drifted = ForcedCase {
        name: GIT_STATUS_NAME,
        expected_arguments: r#"{"unexpected":true}"#,
        prompt: "synthetic invalid forced case",
    };

    assert!(suite.validate_forced_case(&drifted).is_err());
    Ok(())
}

#[test]
fn forced_case_inventory_matches_each_catalog_available_offline() -> EvalResult {
    let git = FamilySuite::git()?;
    let workspace = FamilySuite::workspace()?;
    let web = FamilySuite::web()?;

    git.validate_forced_inventory()?;
    workspace.validate_forced_inventory()?;
    web.validate_forced_inventory()?;
    Ok(())
}

#[test]
fn forced_git_stage_verifier_rejects_success_without_the_postcondition() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_stage_verifier_accepts_the_exact_staged_blob() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    let repository = Repository::open(suite.workspace.path())?;
    let index = repository.index()?;
    let entry = index
        .get_path(Path::new(GIT_STAGE_PATH), 0)
        .expect("the exact staged fixture is indexed");
    let worktree_mode = fs::metadata(suite.workspace.path().join(GIT_STAGE_PATH))?
        .permissions()
        .mode()
        & 0o7777;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(entry.mode, GIT_REGULAR_INDEX_FILE_MODE);
    assert_eq!(
        Some(worktree_mode),
        suite
            .git_seed_fixture
            .modes
            .get(Path::new(GIT_STAGE_PATH))
            .copied()
            .flatten()
    );
    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_stage_verifier_rejects_a_collateral_object() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    Repository::open(suite.workspace.path())?.blob(GIT_COLLATERAL_OBJECT_CONTENT)?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_stage_verifier_rejects_the_wrong_staged_blob() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    fs::write(
        suite.workspace.path().join(GIT_STAGE_PATH),
        GIT_WRONG_STAGE_CONTENT,
    )?;
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    fs::write(
        suite.workspace.path().join(GIT_STAGE_PATH),
        GIT_STAGE_CONTENT,
    )?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_stage_verifier_rejects_an_extra_staged_fixture() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    stage_path(suite.workspace.path(), GIT_COMMIT_PATH)?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_stage_verifier_rejects_a_switched_branch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    repository.set_head("refs/heads/switch-target")?;
    repository.checkout_head(None)?;
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_stage_verifier_rejects_mutated_unrelated_fixtures() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    fs::write(
        suite.workspace.path().join(GIT_COMMIT_PATH),
        GIT_NATURAL_CONTENT,
    )?;
    fs::write(
        suite.workspace.path().join(GIT_NATURAL_PATH),
        GIT_COMMIT_CONTENT,
    )?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_stage_verifier_rejects_mode_drift_in_an_unrelated_fixture() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    let path = suite.workspace.path().join(GIT_COMMIT_PATH);
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(permissions.mode() | USER_EXECUTE_MODE_BIT);
    fs::set_permissions(&path, permissions)?;
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_stage_verifier_rejects_an_executable_staged_file() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    let path = suite.workspace.path().join(GIT_STAGE_PATH);
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(permissions.mode() | USER_EXECUTE_MODE_BIT);
    fs::set_permissions(&path, permissions)?;
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_stage_verifier_rejects_a_symlinked_untracked_fixture() -> EvalResult {
    let suite = FamilySuite::git()?;
    let support = tempfile::tempdir()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    let replacement = support.path().join(GIT_COMMIT_PATH);
    fs::write(&replacement, GIT_COMMIT_CONTENT)?;
    fs::remove_file(suite.workspace.path().join(GIT_COMMIT_PATH))?;
    symlink(&replacement, suite.workspace.path().join(GIT_COMMIT_PATH))?;
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_accepts_the_exact_fixture_tree() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    commit_staged_paths(suite.workspace.path(), message)?;
    let head = Repository::open(suite.workspace.path())?
        .head()?
        .peel_to_commit()?
        .id()
        .to_string();
    let result = serde_json::json!({
        "commit": head,
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_a_collateral_object() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    commit_staged_paths(suite.workspace.path(), message)?;
    let repository = Repository::open(suite.workspace.path())?;
    repository.blob(GIT_COLLATERAL_OBJECT_CONTENT)?;
    let head = repository.head()?.peel_to_commit()?.id().to_string();
    let result = serde_json::json!({
        "commit": head,
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_a_missing_branch_reflog_record() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    commit_staged_paths(suite.workspace.path(), message)?;
    let repository = Repository::open(suite.workspace.path())?;
    let head = repository.head()?.peel_to_commit()?.id();
    let branch_log = repository
        .path()
        .join(GIT_LOGS_DIRECTORY)
        .join("refs/heads")
        .join(GIT_BASE_BRANCH);
    let contents = fs::read(&branch_log)?;
    let previous_record_end = contents[..contents.len() - 1]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .expect("the seeded branch reflog has a previous record");
    fs::write(&branch_log, &contents[..=previous_record_end])?;
    let result = serde_json::json!({
        "commit": head.to_string(),
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_reflog_timestamp_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    commit_staged_paths(suite.workspace.path(), message)?;
    let repository = Repository::open(suite.workspace.path())?;
    let head = repository.head()?.peel_to_commit()?;
    let commit_time = head.committer().when();
    let altered_time = Time::new(commit_time.seconds() + 1, commit_time.offset_minutes());
    let altered_signature = Signature::new(GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL, &altered_time)?;
    replace_latest_reflog_signature(&repository, "HEAD", &altered_signature)?;
    let result = serde_json::json!({
        "commit": head.id().to_string(),
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_a_mutated_untracked_fixture() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    commit_staged_paths(suite.workspace.path(), message)?;
    fs::write(
        suite.workspace.path().join(GIT_STAGE_PATH),
        GIT_NATURAL_CONTENT,
    )?;
    let head = Repository::open(suite.workspace.path())?
        .head()?
        .peel_to_commit()?
        .id()
        .to_string();
    let result = serde_json::json!({
        "commit": head,
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_a_detached_head_without_branch_advancement() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    commit_staged_paths(suite.workspace.path(), message)?;
    let repository = Repository::open(suite.workspace.path())?;
    let head = repository.head()?.peel_to_commit()?.id();
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    repository.reference(
        &format!("refs/heads/{GIT_BASE_BRANCH}"),
        seed,
        true,
        GIT_RESTORE_BRANCH_REFLOG_MESSAGE,
    )?;
    repository.set_head_detached(head)?;
    let result = serde_json::json!({
        "commit": head.to_string(),
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_the_wrong_fixture_tree() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    let repository = Repository::open(suite.workspace.path())?;
    let mut index = repository.index()?;
    index.remove_path(Path::new(GIT_COMMIT_PATH))?;
    index.add_path(Path::new(GIT_STAGE_PATH))?;
    index.write()?;
    commit_staged_paths(suite.workspace.path(), message)?;
    let head = Repository::open(suite.workspace.path())?
        .head()?
        .peel_to_commit()?
        .id()
        .to_string();
    let result = serde_json::json!({
        "commit": head,
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_the_wrong_identity() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    commit_staged_paths_with_identity(
        suite.workspace.path(),
        message,
        SYNTHETIC_OTHER_GIT_AUTHOR_NAME,
        SYNTHETIC_OTHER_GIT_AUTHOR_EMAIL,
    )?;
    let head = Repository::open(suite.workspace.path())?
        .head()?
        .peel_to_commit()?
        .id()
        .to_string();
    let result = serde_json::json!({
        "commit": head,
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_retained_merge_state() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    commit_staged_paths(suite.workspace.path(), message)?;
    install_git_merge_state(
        suite.workspace.path(),
        suite
            .git_seed
            .expect("the Git eval suite has a captured seed identity"),
    )?;
    let head = Repository::open(suite.workspace.path())?
        .head()?
        .peel_to_commit()?
        .id()
        .to_string();
    let result = serde_json::json!({
        "commit": head,
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_retained_cherry_pick_state() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    commit_staged_paths(suite.workspace.path(), message)?;
    let repository = Repository::open(suite.workspace.path())?;
    let head = repository.head()?.peel_to_commit()?.id();
    fs::write(
        repository.path().join(GIT_CHERRY_PICK_HEAD_PATH),
        format!("{head}\n"),
    )?;
    let result = serde_json::json!({
        "commit": head.to_string(),
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_branch_create_verifier_rejects_the_default_head() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_BRANCH_CREATE_NAME)
        .expect("the Git branch-create fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let head = repository.head()?.peel_to_commit()?;
    repository.branch("created-by-eval", &head, false)?;
    let result = serde_json::json!({
        "branch": "created-by-eval",
        "head": head.id().to_string(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_branch_create_verifier_rejects_switching_to_the_created_branch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_BRANCH_CREATE_NAME)
        .expect("the Git branch-create fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    repository.branch("created-by-eval", &target, false)?;
    repository.set_head("refs/heads/created-by-eval")?;
    let result = serde_json::json!({
        "branch": "created-by-eval",
        "head": target.id().to_string(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_branch_create_verifier_rejects_an_extra_branch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_BRANCH_CREATE_NAME)
        .expect("the Git branch-create fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    repository.branch("created-by-eval", &target, false)?;
    repository.branch("collateral-branch", &target, false)?;
    let result = serde_json::json!({
        "branch": "created-by-eval",
        "head": target.id().to_string(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_branch_create_verifier_rejects_an_untracked_fixture_change() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_BRANCH_CREATE_NAME)
        .expect("the Git branch-create fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    repository.branch("created-by-eval", &target, false)?;
    fs::write(suite.workspace.path().join(GIT_STAGE_PATH), b"collateral\n")?;
    let result = serde_json::json!({
        "branch": "created-by-eval",
        "head": target.id().to_string(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_branch_switch_verifier_rejects_a_head_only_update() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_BRANCH_SWITCH_NAME)
        .expect("the Git branch-switch fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("switch-target", BranchType::Local)?
        .get()
        .target()
        .expect("the Git branch-switch fixture has a target");
    repository.set_head("refs/heads/switch-target")?;
    let result = serde_json::json!({
        "branch": "switch-target",
        "head": target.to_string(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_branch_switch_verifier_rejects_rewriting_the_base_branch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_BRANCH_SWITCH_NAME)
        .expect("the Git branch-switch fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("switch-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    repository.set_head("refs/heads/switch-target")?;
    repository.checkout_head(None)?;
    repository.reference(
        &format!("refs/heads/{GIT_BASE_BRANCH}"),
        target.id(),
        true,
        GIT_RESTORE_BRANCH_REFLOG_MESSAGE,
    )?;
    let result = serde_json::json!({
        "branch": "switch-target",
        "head": target.id().to_string(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_branch_switch_verifier_rejects_an_extra_branch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_BRANCH_SWITCH_NAME)
        .expect("the Git branch-switch fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("switch-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    repository.set_head("refs/heads/switch-target")?;
    repository.checkout_head(None)?;
    repository.branch("collateral-branch", &target, false)?;
    let result = serde_json::json!({
        "branch": "switch-target",
        "head": target.id().to_string(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_diff_verifier_accepts_the_seeded_worktree_patch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_DIFF_NAME)
        .expect("the Git diff fixture exists");
    suite.prepare_git_case(GIT_DIFF_NAME)?;
    let repository = Repository::open(suite.workspace.path())?;
    let result = serde_json::json!({
        "patch": expected_bounded_git_worktree_patch(suite.workspace.path())?,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(
        repository.status_file(Path::new(GIT_STAGE_PATH))?,
        Status::INDEX_NEW
    );
    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_diff_verifier_rejects_post_seed_fixture_mode_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_DIFF_NAME)
        .expect("the Git diff fixture exists");
    suite.prepare_git_case(GIT_DIFF_NAME)?;
    let path = suite.workspace.path().join(GIT_DIFF_OVERFLOW_PATH);
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(permissions.mode() ^ USER_EXECUTE_MODE_BIT);
    fs::set_permissions(&path, permissions)?;
    let result = serde_json::json!({
        "patch": expected_bounded_git_worktree_patch(suite.workspace.path())?,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_diff_verifier_rejects_an_empty_patch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_DIFF_NAME)
        .expect("the Git diff fixture exists");
    suite.prepare_git_case(GIT_DIFF_NAME)?;
    let result = serde_json::json!({
        "patch": "",
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_diff_verifier_rejects_an_unstaged_fixture() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_DIFF_NAME)
        .expect("the Git diff fixture exists");
    suite.prepare_git_case(GIT_DIFF_NAME)?;
    let result = serde_json::json!({
        "patch": expected_bounded_git_worktree_patch(suite.workspace.path())?,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();
    let mut index = Repository::open(suite.workspace.path())?.index()?;
    index.remove_path(Path::new(GIT_STAGE_PATH))?;
    index.write()?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_diff_verifier_rejects_a_staged_overflow_fixture() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_DIFF_NAME)
        .expect("the Git diff fixture exists");
    suite.prepare_git_case(GIT_DIFF_NAME)?;
    let result = serde_json::json!({
        "patch": expected_bounded_git_worktree_patch(suite.workspace.path())?,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();
    stage_path(suite.workspace.path(), GIT_DIFF_OVERFLOW_PATH)?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_diff_verifier_rejects_an_unbounded_patch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_DIFF_NAME)
        .expect("the Git diff fixture exists");
    suite.prepare_git_case(GIT_DIFF_NAME)?;
    let result = serde_json::json!({
        "patch": expected_git_worktree_patch(suite.workspace.path())?,
        "truncated": false,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_the_default_head() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let head = Repository::open(suite.workspace.path())?
        .head()?
        .peel_to_commit()?
        .id()
        .to_string();
    let result = serde_json::json!({
        "commits": [{"commit": head}],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_accepts_the_bounded_target() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let result = serde_json::json!({
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
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_reflog_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let head_log = repository.path().join(GIT_LOGS_DIRECTORY).join("HEAD");
    let mut contents = fs::read(&head_log)?;
    contents.extend_from_slice(b"synthetic collateral reflog record\n");
    fs::write(head_log, contents)?;
    let result = serde_json::json!({
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
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_a_collateral_empty_directory() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let result = serde_json::json!({
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
    .to_string();
    fs::create_dir(suite.workspace.path().join(GIT_COLLATERAL_DIRECTORY))?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_log_verifier_rejects_a_collateral_pre_commit_hook() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let result = serde_json::json!({
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
    .to_string();
    let hook = repository.path().join(GIT_PRE_COMMIT_HOOK_PATH);
    fs::write(&hook, GIT_PRE_COMMIT_HOOK_CONTENT)?;
    let mut permissions = fs::metadata(&hook)?.permissions();
    permissions.set_mode(permissions.mode() | USER_EXECUTE_MODE_BIT);
    fs::set_permissions(&hook, permissions)?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_a_moved_target_reference() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let result = serde_json::json!({
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
    .to_string();
    repository
        .find_reference("refs/heads/log-target")?
        .set_target(
            suite
                .git_seed
                .expect("the Git eval suite has a captured seed identity"),
            "synthetic moved log target",
        )?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_a_moved_non_target_reference() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let result = serde_json::json!({
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
    .to_string();
    repository
        .find_reference("refs/heads/switch-target")?
        .set_target(
            suite
                .git_seed
                .expect("the Git eval suite has a captured seed identity"),
            "synthetic moved non-target branch",
        )?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_more_than_the_requested_limit() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let parent = target.parent(0)?;
    let result = serde_json::json!({
        "commits": [
            {"commit": target.id().to_string()},
            {"commit": parent.id().to_string()},
        ],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_accepts_the_bounded_prefix() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_a_collateral_object() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    Repository::open(suite.workspace.path())?.blob(GIT_COLLATERAL_OBJECT_CONTENT)?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_status_verifier_rejects_seed_object_mode_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let object_id = Oid::hash_object(ObjectType::Blob, GIT_BASE_CONTENT.as_bytes())?;
    let object_path = Repository::open(suite.workspace.path())?
        .path()
        .join(GIT_OBJECTS_DIRECTORY)
        .join(git_loose_object_relative_path(object_id));
    let mut permissions = fs::metadata(&object_path)?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(object_path, permissions)?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_status_verifier_rejects_seed_reference_mode_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let reference_path = Repository::open(suite.workspace.path())?
        .path()
        .join(GIT_REFS_DIRECTORY)
        .join("heads")
        .join(GIT_BASE_BRANCH);
    let mut permissions = fs::metadata(&reference_path)?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(reference_path, permissions)?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_status_verifier_rejects_corrupted_seed_object_bytes() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let object_id = Oid::hash_object(ObjectType::Blob, GIT_BASE_CONTENT.as_bytes())?;
    let object_path = Repository::open(suite.workspace.path())?
        .path()
        .join(GIT_OBJECTS_DIRECTORY)
        .join(git_loose_object_relative_path(object_id));
    fs::set_permissions(&object_path, fs::Permissions::from_mode(0o600))?;
    fs::write(object_path, b"synthetic corrupt object bytes")?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_status_verifier_rejects_post_seed_fixture_mode_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let path = suite.workspace.path().join(git_status_overflow_path(0));
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(permissions.mode() ^ USER_EXECUTE_MODE_BIT);
    fs::set_permissions(&path, permissions)?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_status_verifier_rejects_metadata_root_mode_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let metadata_root = Repository::open(suite.workspace.path())?
        .path()
        .to_path_buf();
    let mut permissions = fs::metadata(&metadata_root)?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(&metadata_root, permissions)?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_status_verifier_rejects_top_level_metadata_file_mode_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let config = Repository::open(suite.workspace.path())?
        .path()
        .join(GIT_CONFIG_PATH);
    let mut permissions = fs::metadata(&config)?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(&config, permissions)?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_status_verifier_rejects_a_top_level_metadata_hard_link() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let external = tempfile::tempdir()?;
    let head = Repository::open(suite.workspace.path())?
        .path()
        .join("HEAD");
    let alias = external.path().join("head-alias");
    fs::write(&alias, fs::read(&head)?)?;
    fs::remove_file(&head)?;
    fs::hard_link(&alias, &head)?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_an_unknown_result_field() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        "error": "synthetic contradictory field",
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_an_unknown_entry_field() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let mut entries = git_status_entries_json();
    entries[0]["error"] = serde_json::json!("synthetic contradictory field");
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": entries,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_skip_worktree_index_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let repository = Repository::open(suite.workspace.path())?;
    let mut index = repository.index()?;
    let mut entry = index
        .get_path(Path::new(GIT_SEED_PATH), 0)
        .expect("the seeded index entry exists");
    entry.flags |= GIT_INDEX_EXTENDED_FLAG;
    entry.flags_extended |= GIT_INDEX_SKIP_WORKTREE_FLAG;
    index.add(&entry)?;
    index.write()?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_status_verifier_rejects_a_symlinked_metadata_root() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let external = tempfile::tempdir()?;
    let metadata_root = suite.workspace.path().join(".git");
    let relocated = external.path().join("repository-metadata");
    fs::rename(&metadata_root, &relocated)?;
    std::os::unix::fs::symlink(&relocated, &metadata_root)?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_incorrect_entry_metadata() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let mut entries = git_status_entries_json();
    entries[1]["worktree"] = serde_json::json!("modified");
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": entries,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_a_repository_state_change() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("switch-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    repository.checkout_tree(target.as_object(), None)?;
    repository.set_head("refs/heads/switch-target")?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_repository_config_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();
    Repository::open(suite.workspace.path())?
        .config()?
        .set_str(SYNTHETIC_GIT_CONFIG_KEY, SYNTHETIC_GIT_CONFIG_VALUE)?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_a_collateral_path() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();
    fs::write(
        suite.workspace.path().join("zz-collateral.txt"),
        b"collateral\n",
    )?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_an_unbounded_result() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let mut entries = git_status_entries_json();
    entries.push(serde_json::json!({
        "path": git_status_overflow_path(GIT_STATUS_OVERFLOW_ENTRY_COUNT - 1),
        "previous_path": null,
        "index": "unchanged",
        "worktree": "untracked",
    }));
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": entries,
        "truncated": false,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_write_verifier_rejects_collateral_mutation() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == WRITE_FILE_NAME)
        .expect("the workspace write fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let path = arguments["path"]
        .as_str()
        .expect("the workspace write fixture has a path");
    let content = arguments["content"]
        .as_str()
        .expect("the workspace write fixture has content");
    fs::write(suite.workspace.path().join(path), content)?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_DRIFTED_SEED,
    )?;
    let result = serde_json::json!({
        "path": path,
        "bytes_written": content.len(),
        "created": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_write_verifier_rejects_a_deleted_unrelated_fixture() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == WRITE_FILE_NAME)
        .expect("the workspace write fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let path = arguments["path"]
        .as_str()
        .expect("the workspace write fixture has a path");
    let content = arguments["content"]
        .as_str()
        .expect("the workspace write fixture has content");
    fs::write(suite.workspace.path().join(path), content)?;
    fs::remove_file(suite.workspace.path().join(WORKSPACE_GLOB_PATH))?;
    let result = serde_json::json!({
        "path": path,
        "bytes_written": content.len(),
        "created": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_write_verifier_accepts_the_private_creation_mode() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == WRITE_FILE_NAME)
        .expect("the workspace write fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let path = arguments["path"]
        .as_str()
        .expect("the workspace write fixture has a path");
    let content = arguments["content"]
        .as_str()
        .expect("the workspace write fixture has content");
    let target = suite.workspace.path().join(path);
    fs::write(&target, content)?;
    fs::set_permissions(
        &target,
        fs::Permissions::from_mode(WORKSPACE_PRIVATE_CREATION_MODE),
    )?;
    let result = serde_json::json!({
        "path": path,
        "bytes_written": content.len(),
        "created": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_write_verifier_rejects_an_insecure_creation_mode() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == WRITE_FILE_NAME)
        .expect("the workspace write fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let path = arguments["path"]
        .as_str()
        .expect("the workspace write fixture has a path");
    let content = arguments["content"]
        .as_str()
        .expect("the workspace write fixture has content");
    let target = suite.workspace.path().join(path);
    fs::write(&target, content)?;
    fs::set_permissions(
        &target,
        fs::Permissions::from_mode(WORKSPACE_INSECURE_CREATION_MODE),
    )?;
    let result = serde_json::json!({
        "path": path,
        "bytes_written": content.len(),
        "created": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_apply_patch_verifier_rejects_collateral_mutation() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == APPLY_PATCH_NAME)
        .expect("the workspace apply-patch fixture exists");
    fs::write(
        suite.workspace.path().join("patched.txt"),
        "patched by eval\n",
    )?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_DRIFTED_SEED,
    )?;
    let result = serde_json::json!({
        "operations_applied": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_apply_patch_verifier_rejects_an_insecure_creation_mode() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == APPLY_PATCH_NAME)
        .expect("the workspace apply-patch fixture exists");
    let target = suite.workspace.path().join("patched.txt");
    fs::write(&target, "patched by eval\n")?;
    fs::set_permissions(
        &target,
        fs::Permissions::from_mode(WORKSPACE_INSECURE_CREATION_MODE),
    )?;
    let result = serde_json::json!({
        "operations_applied": 1,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_edit_verifier_rejects_collateral_mutation() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == EDIT_FILE_NAME)
        .expect("the workspace edit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let old = arguments["old_string"]
        .as_str()
        .expect("the edit fixture has an old string");
    let new = arguments["new_string"]
        .as_str()
        .expect("the edit fixture has a new string");
    let expected = WORKSPACE_SEED.replace(old, new);
    fs::write(suite.workspace.path().join(WORKSPACE_SEED_PATH), &expected)?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_GLOB_PATH),
        WORKSPACE_DRIFTED_GLOB_CONTENT,
    )?;
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "replacements": WORKSPACE_SEED.match_indices(old).count(),
        "bytes_written": expected.len(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_edit_verifier_rejects_a_mode_change() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == EDIT_FILE_NAME)
        .expect("the workspace edit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let old = arguments["old_string"]
        .as_str()
        .expect("the edit fixture has an old string");
    let new = arguments["new_string"]
        .as_str()
        .expect("the edit fixture has a new string");
    let expected = WORKSPACE_SEED.replace(old, new);
    let path = suite.workspace.path().join(WORKSPACE_SEED_PATH);
    fs::write(&path, &expected)?;
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(permissions.mode() ^ USER_EXECUTE_MODE_BIT);
    fs::set_permissions(&path, permissions)?;
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "replacements": WORKSPACE_SEED.match_indices(old).count(),
        "bytes_written": expected.len(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_search_verifier_rejects_an_empty_success() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == SEARCH_FILES_NAME)
        .expect("the workspace search fixture exists");
    let result = serde_json::json!({
        "matches": [],
        "truncated": false,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_search_verifier_accepts_the_bounded_first_match() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == SEARCH_FILES_NAME)
        .expect("the workspace search fixture exists");
    let match_line = WORKSPACE_SEARCH_CONTENT
        .lines()
        .nth(1)
        .expect("the workspace search fixture has a matching line");
    let result = serde_json::json!({
        "matches": [{
            "path": WORKSPACE_SEARCH_PATH,
            "line": 2,
            "column": 1,
            "text_start_column": 1,
            "text": match_line,
            "line_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_search_verifier_rejects_collateral_mutation() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == SEARCH_FILES_NAME)
        .expect("the workspace search fixture exists");
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_DRIFTED_SEED,
    )?;
    let match_line = WORKSPACE_SEARCH_CONTENT
        .lines()
        .nth(1)
        .expect("the workspace search fixture has a matching line");
    let result = serde_json::json!({
        "matches": [{
            "path": WORKSPACE_SEARCH_PATH,
            "line": 2,
            "column": 1,
            "text_start_column": 1,
            "text": match_line,
            "line_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_search_verifier_rejects_a_root_match() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == SEARCH_FILES_NAME)
        .expect("the workspace search fixture exists");
    let root_match = WORKSPACE_SEED
        .lines()
        .nth(1)
        .expect("the root fixture has a matching line");
    let result = serde_json::json!({
        "matches": [{
            "path": WORKSPACE_SEED_PATH,
            "line": 2,
            "column": 1,
            "text_start_column": 1,
            "text": root_match,
            "line_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_read_verifier_rejects_an_unbounded_result() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": WORKSPACE_SEED,
        "bytes_read": WORKSPACE_SEED.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": false,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_read_verifier_rejects_an_unknown_result_field() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    let prefix = WORKSPACE_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the seeded workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "bytes_read": prefix.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": true,
        "error": "synthetic contradictory field",
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_read_verifier_rejects_a_mutated_fixture() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_DRIFTED_SEED,
    )?;
    let prefix = WORKSPACE_DRIFTED_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the drifted workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "bytes_read": prefix.len(),
        "total_bytes": WORKSPACE_DRIFTED_SEED.len(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_read_verifier_rejects_collateral_mutation() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    fs::write(
        suite.workspace.path().join(WORKSPACE_GLOB_NONMATCHING_PATH),
        WORKSPACE_DRIFTED_GLOB_CONTENT,
    )?;
    let prefix = WORKSPACE_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "bytes_read": prefix.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_read_verifier_rejects_directory_mode_drift() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    let path = suite.workspace.path().join(WORKSPACE_GLOB_DIRECTORY);
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(&path, permissions)?;
    let prefix = WORKSPACE_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "bytes_read": prefix.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_read_verifier_rejects_root_mode_drift() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    let mut permissions = fs::metadata(suite.workspace.path())?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(suite.workspace.path(), permissions)?;
    let prefix = WORKSPACE_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "bytes_read": prefix.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_listing_verifiers_reject_the_wrong_entry_kind() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let list = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == LIST_DIRECTORY_NAME)
        .expect("the workspace list fixture exists");
    let glob = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == GLOB_FILES_NAME)
        .expect("the workspace glob fixture exists");
    let mut list_entries = workspace_listing_json(expected_workspace_listing());
    list_entries[0]["kind"] = serde_json::json!("directory");
    let list_result = serde_json::json!({
        "entries": list_entries,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();
    let glob_result = serde_json::json!({
        "matches": [{"path": WORKSPACE_GLOB_PATH, "kind": "directory"}],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(list, &list_result)?);
    assert!(!suite.forced_case_result_passed(glob, &glob_result)?);
    Ok(())
}

#[test]
fn forced_workspace_list_verifier_rejects_an_unknown_entry_field() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == LIST_DIRECTORY_NAME)
        .expect("the workspace list fixture exists");
    let mut entries = workspace_listing_json(expected_workspace_listing());
    entries[0]["error"] = serde_json::json!("synthetic contradictory field");
    let result = serde_json::json!({
        "entries": entries,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_list_verifier_rejects_an_unbounded_result() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == LIST_DIRECTORY_NAME)
        .expect("the workspace list fixture exists");
    let result = serde_json::json!({
        "entries": workspace_listing_json(complete_workspace_listing()),
        "truncated": false,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_list_verifier_rejects_collateral_mutation() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == LIST_DIRECTORY_NAME)
        .expect("the workspace list fixture exists");
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_DRIFTED_SEED,
    )?;
    let result = serde_json::json!({
        "entries": workspace_listing_json(expected_workspace_listing()),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_list_verifier_rejects_collateral_hard_links() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == LIST_DIRECTORY_NAME)
        .expect("the workspace list fixture exists");
    let source = suite.workspace.path().join(workspace_list_entry_path(0));
    let alias = suite.workspace.path().join(workspace_list_entry_path(1));
    fs::remove_file(&alias)?;
    fs::hard_link(&source, &alias)?;
    let result = serde_json::json!({
        "entries": workspace_listing_json(expected_workspace_listing()),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_glob_verifier_rejects_a_nonmatching_path() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == GLOB_FILES_NAME)
        .expect("the workspace glob fixture exists");
    let result = serde_json::json!({
        "matches": [
            {"path": WORKSPACE_SEED_PATH, "kind": "file"},
            {"path": workspace_nonmatching_path(0), "kind": "file"}
        ],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_glob_verifier_rejects_a_scoped_pattern_miss() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == GLOB_FILES_NAME)
        .expect("the workspace glob fixture exists");
    let result = serde_json::json!({
        "matches": [{"path": WORKSPACE_GLOB_NONMATCHING_PATH, "kind": "file"}],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(
        fs::read(suite.workspace.path().join(WORKSPACE_GLOB_NONMATCHING_PATH))?,
        WORKSPACE_GLOB_NONMATCHING_CONTENT.as_bytes()
    );
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_glob_verifier_accepts_the_bounded_first_match() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == GLOB_FILES_NAME)
        .expect("the workspace glob fixture exists");
    let result = serde_json::json!({
        "matches": [{"path": WORKSPACE_GLOB_PATH, "kind": "file"}],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_glob_verifier_rejects_collateral_mutation() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == GLOB_FILES_NAME)
        .expect("the workspace glob fixture exists");
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_DRIFTED_SEED,
    )?;
    let result = serde_json::json!({
        "matches": [{"path": WORKSPACE_GLOB_PATH, "kind": "file"}],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_glob_verifier_rejects_a_root_match() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == GLOB_FILES_NAME)
        .expect("the workspace glob fixture exists");
    let result = serde_json::json!({
        "matches": [{"path": WORKSPACE_SEED_PATH, "kind": "file"}],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_edit_fixture_exercises_replace_all() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == EDIT_FILE_NAME)
        .expect("the workspace edit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_EDITED_SEED,
    )?;
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "replacements": EXPECTED_WORKSPACE_EDIT_REPLACEMENTS,
        "bytes_written": EXPECTED_WORKSPACE_EDIT_BYTES,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(arguments["replace_all"], true);
    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_web_search_verifier_rejects_an_extra_result() -> EvalResult {
    let suite = FamilySuite::web()?;
    let case = WEB_CASES
        .iter()
        .find(|case| case.name == WEB_SEARCH_NAME)
        .expect("the web search fixture exists");
    let fixture = serde_json::json!({
        "title": WEB_SEARCH_TITLE,
        "url": WEB_URL,
        "snippet": WEB_SEARCH_SNIPPET,
    });
    let result = serde_json::json!({
        "results": [fixture.clone(), fixture],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_web_search_verifier_rejects_an_unexpected_result_field() -> EvalResult {
    let suite = FamilySuite::web()?;
    let case = WEB_CASES
        .iter()
        .find(|case| case.name == WEB_SEARCH_NAME)
        .expect("the web search fixture exists");
    let result = serde_json::json!({
        "results": [{
            "title": WEB_SEARCH_TITLE,
            "url": WEB_URL,
            "snippet": WEB_SEARCH_SNIPPET,
            "error": "synthetic contradictory field",
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_web_search_verifier_accepts_distinct_incomplete_evidence() -> EvalResult {
    let suite = FamilySuite::web()?;
    let case = WEB_CASES
        .iter()
        .find(|case| case.name == WEB_SEARCH_NAME)
        .expect("the web search fixture exists");
    let result = serde_json::json!({
        "results": [{
            "title": WEB_SEARCH_TITLE,
            "url": WEB_URL,
            "snippet": WEB_SEARCH_SNIPPET,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_web_fetch_verifier_accepts_truncated_body_evidence() -> EvalResult {
    let suite = FamilySuite::web()?;
    let case = WEB_CASES
        .iter()
        .find(|case| case.name == WEB_FETCH_NAME)
        .expect("the web fetch fixture exists");
    let result = serde_json::json!({
        "url": WEB_URL,
        "status": 200,
        "content_type": "text/plain",
        "body": WEB_FETCH_BODY,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_web_fetch_verifier_rejects_an_unexpected_result_field() -> EvalResult {
    let suite = FamilySuite::web()?;
    let case = WEB_CASES
        .iter()
        .find(|case| case.name == WEB_FETCH_NAME)
        .expect("the web fetch fixture exists");
    let result = serde_json::json!({
        "url": WEB_URL,
        "status": 200,
        "content_type": "text/plain",
        "body": WEB_FETCH_BODY,
        "truncated": true,
        "error": "synthetic contradictory field",
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_execution_classifies_argument_drift_before_state_verification() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == APPLY_PATCH_NAME)
        .expect("the apply-patch fixture exists");
    let expected_arguments = normalized_arguments_text(case.expected_arguments)?;
    let persisted_arguments = normalized_arguments_text(DRIFTED_APPLY_PATCH_ARGUMENTS)?;

    assert!(!forced_execution_completed(
        &suite,
        case,
        ForcedExecutionEvidence {
            persisted_arguments: &persisted_arguments,
            expected_arguments: &expected_arguments,
            result_content: r#"{"operations_applied":1}"#,
        },
    )?);
    Ok(())
}

#[test]
fn forced_tier_reports_a_miss_for_drifted_arguments() {
    let target = GIT_STATUS_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from("{}")),
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("fixture result"),
            is_error: false,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(target),
                arguments_text: String::from(r#"{"unexpected":true}"#),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_tool_sequence_allows_only_one_forced_exchange() {
    let sequence = ForcedToolSequence::new(Some(SANDBOXED_EXEC_NAME));

    assert_eq!(
        sequence.next(),
        ForcedToolOperation::Force(RuntimeToolName::new(SANDBOXED_EXEC_NAME))
    );
    assert_eq!(sequence.next(), ForcedToolOperation::Continuation);
}

#[test]
fn exec_eval_rejects_program_drift_before_dispatch() {
    let fixture = forced_exec_fixture(SANDBOXED_EXEC_NAME);
    let mut drifted: serde_json::Value = serde_json::from_str(fixture.expected_arguments)
        .expect("the sandboxed fixture arguments decode");
    drifted["program"] = serde_json::json!("curl");
    let drifted = NormalizedToolArguments::try_from_provider_text(drifted.to_string())
        .expect("drifted fixture arguments normalize");

    assert!(!ExecEvalCase::ForcedSandboxed.admits(SANDBOXED_EXEC_NAME, &drifted));
}

#[test]
fn exec_eval_rejects_argument_drift_before_dispatch() {
    let fixture = forced_exec_fixture(SANDBOXED_EXEC_NAME);
    let mut drifted: serde_json::Value = serde_json::from_str(fixture.expected_arguments)
        .expect("the sandboxed fixture arguments decode");
    drifted["arguments"] = serde_json::json!(["different output\n"]);
    let drifted = NormalizedToolArguments::try_from_provider_text(drifted.to_string())
        .expect("drifted fixture arguments normalize");

    assert!(!ExecEvalCase::ForcedSandboxed.admits(SANDBOXED_EXEC_NAME, &drifted));
}

#[test]
fn forced_unsandboxed_eval_denies_model_argument_drift() {
    let drifted = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({
            "program": "/usr/bin/printf",
            "arguments": ["different output\n"],
            "working_directory": ".",
            "timeout_seconds": 30,
        })
        .to_string(),
    )
    .expect("drifted unsandboxed fixture arguments normalize");

    let mut approval_state = ExecApprovalState::new(ExecApprovalMode::ApproveOneExactForced);

    assert_eq!(
        approval_state.decision(UNSANDBOXED_EXEC_NAME, &drifted),
        ToolApprovalDecision::Deny { reason: None }
    );
}

#[test]
fn unforced_exec_eval_denies_the_exact_forced_unsandboxed_fixture() {
    let unsandboxed = forced_exec_fixture(UNSANDBOXED_EXEC_NAME);
    let exact_forced = NormalizedToolArguments::try_from_provider_text(String::from(
        unsandboxed.expected_arguments,
    ))
    .expect("the exact forced unsandboxed fixture arguments normalize");

    let mut approval_state = ExecApprovalState::new(ExecApprovalMode::DenyAll);

    assert_eq!(
        approval_state.decision(UNSANDBOXED_EXEC_NAME, &exact_forced),
        ToolApprovalDecision::Deny { reason: None }
    );
}

#[test]
fn forced_exec_eval_approves_only_one_exact_unsandboxed_fixture() {
    let unsandboxed = forced_exec_fixture(UNSANDBOXED_EXEC_NAME);
    let exact_forced = NormalizedToolArguments::try_from_provider_text(String::from(
        unsandboxed.expected_arguments,
    ))
    .expect("the exact forced unsandboxed fixture arguments normalize");
    let mut approval_state = ExecApprovalState::new(ExecApprovalMode::ApproveOneExactForced);

    assert_eq!(
        approval_state.decision(UNSANDBOXED_EXEC_NAME, &exact_forced),
        ToolApprovalDecision::Approve
    );
    assert_eq!(
        approval_state.decision(UNSANDBOXED_EXEC_NAME, &exact_forced),
        ToolApprovalDecision::Deny { reason: None }
    );
}

#[test]
fn operation_tracker_records_each_cumulative_tool_result_once() {
    let tracker = OperationTracker::default();
    let tool_call_id = String::from("synthetic-tool-call");
    let content = synthetic_result_with_receipt();
    let result = TrackedToolResult {
        request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        content: content.clone(),
        is_error: false,
        round_tripped: false,
    };
    tracker.record_new_results([(tool_call_id.clone(), result.clone())]);
    tracker.record_new_results([(tool_call_id, result.clone())]);

    assert_eq!(tracker.tool_results(), vec![result]);
    assert_eq!(
        tracker.result_content(Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)),
        Some(content)
    );
}

#[test]
fn report_round_trip_count_excludes_an_unacknowledged_result() {
    let results = [
        TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("acknowledged"),
            is_error: false,
            round_tripped: true,
        },
        TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
            content: String::from("unacknowledged"),
            is_error: false,
            round_tripped: false,
        },
    ];

    assert_eq!(round_tripped_result_count(&results), 1);
}

#[test]
fn forced_exec_tier_rejects_a_nonzero_process_result() {
    let mut execution = confined_exit(EXEC_FORCED_SANDBOXED_OUTPUT);
    execution["outcome"]["code"] = serde_json::json!(1);
    let outcome = forced_exec_outcome(SANDBOXED_EXEC_NAME, execution);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn unforced_exec_tier_rejects_an_additional_tool_call() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        tool_results: vec![
            TrackedToolResult {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                content: confined_exit("").to_string(),
                is_error: false,
                round_tripped: true,
            },
            TrackedToolResult {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                content: confined_exit("").to_string(),
                is_error: false,
                round_tripped: true,
            },
        ],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(SANDBOXED_EXEC_NAME),
                    arguments_text: String::from("{}"),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                    attempt_succeeded: true,
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(CARGO_DIAGNOSTICS_NAME),
                    arguments_text: String::from("{}"),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                    attempt_succeeded: true,
                },
            ],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Miss
    );
}

/// One forced Exec outcome whose sole result carries the supplied execution.
fn forced_exec_outcome(target: &'static str, execution: serde_json::Value) -> CaseOutcome {
    let fixture = forced_exec_fixture(target);
    CaseOutcome {
        target: Some(String::from(fixture.name)),
        expected_arguments: Some(String::from(fixture.expected_arguments)),
        execution_completed: true,
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
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    }
}

struct ZeroExitEvidence<'a> {
    confinement: ExecutionConfinement,
    stdout: &'a str,
}

/// One serialized execution with the selected confinement and zero exit.
fn zero_exit_with_confinement(evidence: ZeroExitEvidence<'_>) -> serde_json::Value {
    direct_exec_result(DirectExecEvidence::successful_with_confinement(
        evidence.confinement,
        evidence.stdout,
    ))
}

struct DirectExecEvidence<'a> {
    confinement: ExecutionConfinement,
    outcome: ProcessOutcome,
    stdout: &'a str,
    completeness: CaptureCompleteness,
}

impl<'a> DirectExecEvidence<'a> {
    fn confined_success(stdout: &'a str) -> Self {
        Self::successful_with_confinement(ExecutionConfinement::FilesystemConfined, stdout)
    }

    fn successful_with_confinement(confinement: ExecutionConfinement, stdout: &'a str) -> Self {
        Self {
            confinement,
            outcome: ProcessOutcome::Exited { code: Some(0) },
            stdout,
            completeness: CaptureCompleteness::Complete,
        }
    }

    fn unsandboxed_truncated(stdout: &'a str) -> Self {
        Self {
            confinement: ExecutionConfinement::Unsandboxed,
            outcome: ProcessOutcome::Exited { code: Some(0) },
            stdout,
            completeness: CaptureCompleteness::Truncated,
        }
    }

    fn timed_out() -> Self {
        Self {
            outcome: ProcessOutcome::TimedOut,
            ..Self::confined_success("")
        }
    }

    fn nonzero_exit() -> Self {
        Self {
            outcome: ProcessOutcome::Exited { code: Some(1) },
            ..Self::confined_success("")
        }
    }

    fn supervision_failure() -> Self {
        Self {
            outcome: ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Wait,
            },
            ..Self::confined_success("")
        }
    }

    fn sandbox_refusal() -> Self {
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

    fn sandbox_setup_failure() -> Self {
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

fn direct_exec_result(evidence: DirectExecEvidence<'_>) -> serde_json::Value {
    serde_json::to_value(ExecResult {
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
    .expect("producer direct-exec result serializes")
}

/// One serialized confined execution that exited zero with the given output.
fn confined_exit(stdout: &str) -> serde_json::Value {
    zero_exit_with_confinement(ZeroExitEvidence {
        confinement: ExecutionConfinement::FilesystemConfined,
        stdout,
    })
}

/// One complete, successful Cargo check result in the eval workspace.
fn successful_cargo_diagnostics_result() -> serde_json::Value {
    let stream = CargoDiagnosticsStream {
        completeness: CaptureCompleteness::Complete,
        encoding: OutputEncoding::Utf8,
    };
    cargo_diagnostics_result(CargoDiagnosticsExecution {
        confinement: ExecutionConfinement::FilesystemConfined,
        outcome: ProcessOutcome::Exited { code: Some(0) },
        stdout: stream,
        stderr: stream,
        cargo_failure: None,
        preparation_failure: None,
    })
}

/// One serialized Cargo check result carrying the supplied execution evidence.
fn cargo_diagnostics_result(execution: CargoDiagnosticsExecution) -> serde_json::Value {
    let records = CargoDiagnosticRecords {
        values: Vec::new(),
        limit_reached: false,
        provenance: CargoEvidenceProvenance::WorkspaceInfluenced,
        known_truncated: false,
    };
    let tests = CargoTestRecords {
        values: Vec::new(),
        limit_reached: false,
        provenance: CargoEvidenceProvenance::WorkspaceInfluenced,
        known_truncated: false,
    };
    serde_json::to_value(CargoDiagnosticsResult {
        command: CargoDiagnosticsCommand::Check,
        execution,
        diagnostics: records,
        tests,
    })
    .expect("producer Cargo diagnostics result serializes")
}

fn synthetic_cargo_diagnostic(level: &str) -> CargoDiagnostic {
    CargoDiagnostic {
        file: None,
        file_completeness: CaptureCompleteness::Complete,
        span: None,
        level: String::from(level),
        level_completeness: CaptureCompleteness::Complete,
        message: String::from(SYNTHETIC_CARGO_DIAGNOSTIC_MESSAGE),
        message_completeness: CaptureCompleteness::Complete,
    }
}

fn synthetic_cargo_diagnostic_span() -> serde_json::Value {
    serde_json::json!({
        "line_start": SYNTHETIC_CARGO_DIAGNOSTIC_LINE,
        "column_start": SYNTHETIC_CARGO_DIAGNOSTIC_START_COLUMN,
        "line_end": SYNTHETIC_CARGO_DIAGNOSTIC_LINE,
        "column_end": SYNTHETIC_CARGO_DIAGNOSTIC_END_COLUMN,
    })
}

#[test]
fn forced_exec_tier_passes_the_exact_captured_output() {
    let outcome = forced_exec_outcome(
        SANDBOXED_EXEC_NAME,
        confined_exit(EXEC_FORCED_SANDBOXED_OUTPUT),
    );

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Pass);
}

#[test]
fn forced_exec_tier_rejects_a_zero_exit_that_captured_nothing() {
    let outcome = forced_exec_outcome(SANDBOXED_EXEC_NAME, confined_exit(""));

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_exec_tier_rejects_the_other_case_s_output() {
    let outcome = forced_exec_outcome(
        SANDBOXED_EXEC_NAME,
        confined_exit(EXEC_FORCED_READ_ONLY_OUTPUT),
    );

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_sandboxed_exec_rejects_an_unconfined_zero_exit() {
    let outcome = forced_exec_outcome(
        SANDBOXED_EXEC_NAME,
        zero_exit_with_confinement(ZeroExitEvidence {
            confinement: ExecutionConfinement::Unsandboxed,
            stdout: EXEC_FORCED_SANDBOXED_OUTPUT,
        }),
    );

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_unsandboxed_exec_rejects_a_confined_zero_exit() {
    let outcome = forced_exec_outcome(
        UNSANDBOXED_EXEC_NAME,
        confined_exit(EXEC_FORCED_READ_ONLY_OUTPUT),
    );

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_cargo_diagnostics_rejects_an_unconfined_zero_exit() {
    let mut result = successful_cargo_diagnostics_result();
    result["execution"]["confinement"]["kind"] = serde_json::json!("unsandboxed");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_cargo_diagnostics_accepts_a_complete_successful_check() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["values"] = serde_json::to_value(vec![synthetic_cargo_diagnostic(
        CARGO_WARNING_DIAGNOSTIC_LEVEL,
    )])
    .expect("producer Cargo diagnostics serialize");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Pass);
}

#[test]
fn forced_cargo_diagnostics_accepts_correlated_file_and_span() {
    let mut result = successful_cargo_diagnostics_result();
    let mut diagnostic =
        serde_json::to_value(synthetic_cargo_diagnostic(CARGO_WARNING_DIAGNOSTIC_LEVEL))
            .expect("producer Cargo diagnostics serialize");
    diagnostic["file"] = serde_json::json!(SYNTHETIC_CARGO_DIAGNOSTIC_FILE);
    diagnostic["span"] = synthetic_cargo_diagnostic_span();
    result["diagnostics"]["values"] = serde_json::json!([diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Pass);
}

#[test]
fn forced_cargo_diagnostics_rejects_error_diagnostics_from_a_successful_check() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["values"] = serde_json::to_value(vec![synthetic_cargo_diagnostic(
        CARGO_ERROR_DIAGNOSTIC_LEVEL,
    )])
    .expect("producer Cargo diagnostics serialize");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_cargo_diagnostics_rejects_malformed_diagnostics() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["values"] = serde_json::json!([{}]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_cargo_diagnostics_rejects_a_backwards_same_line_span() {
    let mut result = successful_cargo_diagnostics_result();
    let mut diagnostic =
        serde_json::to_value(synthetic_cargo_diagnostic(CARGO_WARNING_DIAGNOSTIC_LEVEL))
            .expect("producer Cargo diagnostics serialize");
    diagnostic["file"] = serde_json::json!(SYNTHETIC_CARGO_DIAGNOSTIC_FILE);
    diagnostic["span"] = serde_json::json!({
        "line_start": SYNTHETIC_CARGO_DIAGNOSTIC_LINE,
        "column_start": SYNTHETIC_CARGO_DIAGNOSTIC_START_COLUMN,
        "line_end": SYNTHETIC_CARGO_DIAGNOSTIC_LINE,
        "column_end": SYNTHETIC_CARGO_DIAGNOSTIC_BACKWARDS_END_COLUMN,
    });
    result["diagnostics"]["values"] = serde_json::json!([diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_cargo_diagnostics_rejects_a_span_without_its_file() {
    let mut result = successful_cargo_diagnostics_result();
    let mut diagnostic =
        serde_json::to_value(synthetic_cargo_diagnostic(CARGO_WARNING_DIAGNOSTIC_LEVEL))
            .expect("producer Cargo diagnostics serialize");
    diagnostic["span"] = synthetic_cargo_diagnostic_span();
    result["diagnostics"]["values"] = serde_json::json!([diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_cargo_diagnostics_rejects_a_file_without_its_span() {
    let mut result = successful_cargo_diagnostics_result();
    let mut diagnostic =
        serde_json::to_value(synthetic_cargo_diagnostic(CARGO_WARNING_DIAGNOSTIC_LEVEL))
            .expect("producer Cargo diagnostics serialize");
    diagnostic["file"] = serde_json::json!(SYNTHETIC_CARGO_DIAGNOSTIC_FILE);
    result["diagnostics"]["values"] = serde_json::json!([diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_cargo_diagnostics_requires_complete_absent_location_evidence() {
    let mut result = successful_cargo_diagnostics_result();
    let mut diagnostic =
        serde_json::to_value(synthetic_cargo_diagnostic(CARGO_WARNING_DIAGNOSTIC_LEVEL))
            .expect("producer Cargo diagnostics serialize");
    diagnostic["file_completeness"] = serde_json::json!("truncated");
    result["diagnostics"]["values"] = serde_json::json!([diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_cargo_diagnostics_rejects_a_truncated_stdout_capture() {
    let mut result = successful_cargo_diagnostics_result();
    result["execution"]["stdout"]["completeness"] = serde_json::json!("truncated");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_cargo_diagnostics_rejects_lossy_stderr_capture() {
    let mut result = successful_cargo_diagnostics_result();
    result["execution"]["stderr"]["encoding"] = serde_json::json!("lossy_utf8");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_cargo_diagnostics_rejects_capped_records() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["limit_reached"] = serde_json::json!(true);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_cargo_diagnostics_rejects_known_truncated_test_records() {
    let mut result = successful_cargo_diagnostics_result();
    result["tests"]["known_truncated"] = serde_json::json!(true);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_cargo_diagnostics_rejects_unexpected_test_records() {
    let mut result = successful_cargo_diagnostics_result();
    result["tests"]["values"] = serde_json::to_value(vec![CargoTestResult {
        executable: String::from(SYNTHETIC_CARGO_TEST_EXECUTABLE),
        executable_completeness: CaptureCompleteness::Complete,
        name: String::from(SYNTHETIC_CARGO_TEST_NAME),
        name_completeness: CaptureCompleteness::Complete,
        outcome: CargoTestOutcome::Passed,
    }])
    .expect("producer Cargo test records serialize");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_cargo_diagnostics_reports_sandbox_setup_failure_as_infrastructure() {
    let stream = CargoDiagnosticsStream {
        completeness: CaptureCompleteness::Complete,
        encoding: OutputEncoding::Utf8,
    };
    let result = cargo_diagnostics_result(CargoDiagnosticsExecution {
        confinement: ExecutionConfinement::SandboxSetupFailed,
        outcome: ProcessOutcome::SpawnFailed {
            reason: ProcessSpawnFailure::SandboxSetup,
        },
        stdout: stream,
        stderr: stream,
        cargo_failure: None,
        preparation_failure: None,
    });
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_an_incomplete_result_shape() {
    let outcome = forced_exec_outcome(
        CARGO_DIAGNOSTICS_NAME,
        serde_json::json!({
            "execution": confined_exit(""),
        }),
    );

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_exec_tier_rejects_a_truncated_output_capture() {
    let outcome = forced_exec_outcome(
        UNSANDBOXED_EXEC_NAME,
        direct_exec_result(DirectExecEvidence::unsandboxed_truncated(
            EXEC_FORCED_READ_ONLY_OUTPUT,
        )),
    );

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_exec_tier_rejects_a_missing_stderr_capture() {
    let mut result = direct_exec_result(DirectExecEvidence::successful_with_confinement(
        ExecutionConfinement::Unsandboxed,
        EXEC_FORCED_READ_ONLY_OUTPUT,
    ));
    result
        .as_object_mut()
        .expect("the direct-exec fixture is an object")
        .remove("stderr");
    let outcome = forced_exec_outcome(UNSANDBOXED_EXEC_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_exec_tier_rejects_a_truncated_stderr_capture() {
    let mut result = direct_exec_result(DirectExecEvidence::successful_with_confinement(
        ExecutionConfinement::Unsandboxed,
        EXEC_FORCED_READ_ONLY_OUTPUT,
    ));
    result["stderr"]["completeness"] = serde_json::json!("truncated");
    let outcome = forced_exec_outcome(UNSANDBOXED_EXEC_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_exec_tier_rejects_a_lossy_stderr_capture() {
    let mut result = direct_exec_result(DirectExecEvidence::successful_with_confinement(
        ExecutionConfinement::Unsandboxed,
        EXEC_FORCED_READ_ONLY_OUTPUT,
    ));
    result["stderr"]["encoding"] = serde_json::json!("lossy_utf8");
    let outcome = forced_exec_outcome(UNSANDBOXED_EXEC_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_exec_tier_rejects_a_nonempty_stderr_capture() {
    let mut result = direct_exec_result(DirectExecEvidence::successful_with_confinement(
        ExecutionConfinement::Unsandboxed,
        EXEC_FORCED_READ_ONLY_OUTPUT,
    ));
    result["stderr"]["text"] = serde_json::json!(SYNTHETIC_EXECUTOR_FAILURE);
    let outcome = forced_exec_outcome(UNSANDBOXED_EXEC_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

/// One unforced Exec outcome whose sole request carries the supplied execution.
fn natural_exec_outcome(execution: serde_json::Value) -> CaseOutcome {
    CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
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
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    }
}

#[test]
fn unforced_exec_tier_passes_a_confined_zero_exit() {
    let outcome = natural_exec_outcome(confined_exit(EXEC_NATURAL_OUTPUT));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Pass
    );
}

#[test]
fn unforced_exec_tier_rejects_a_truncated_capture() {
    let mut execution = confined_exit(EXEC_NATURAL_OUTPUT);
    execution["stdout"]["completeness"] = serde_json::json!("truncated");
    let outcome = natural_exec_outcome(execution);

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Miss
    );
}

#[test]
fn unforced_exec_tier_rejects_a_lossy_capture() {
    let mut execution = confined_exit(EXEC_NATURAL_OUTPUT);
    execution["stderr"]["encoding"] = serde_json::json!("lossy_utf8");
    let outcome = natural_exec_outcome(execution);

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Miss
    );
}

#[test]
fn unforced_exec_tier_rejects_a_timed_out_process() {
    let outcome = natural_exec_outcome(direct_exec_result(DirectExecEvidence::timed_out()));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Miss
    );
}

#[test]
fn unforced_exec_tier_rejects_a_nonzero_exit() {
    let outcome = natural_exec_outcome(direct_exec_result(DirectExecEvidence::nonzero_exit()));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Miss
    );
}

#[test]
fn unforced_exec_tier_reports_a_supervision_failure_as_infrastructure() {
    let outcome =
        natural_exec_outcome(direct_exec_result(DirectExecEvidence::supervision_failure()));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_exec_tier_reports_sandbox_refusal_as_infrastructure() {
    let outcome = natural_exec_outcome(direct_exec_result(DirectExecEvidence::sandbox_refusal()));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_exec_tier_scores_the_explicit_approval_cap_as_a_miss() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        tool_results: Vec::new(),
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::ApprovalCapReached,
            requests: vec![
                denied_unsandboxed_request(Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)),
                denied_unsandboxed_request(Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID)),
                denied_unsandboxed_request(Uuid::from_u128(ARBITRARY_THIRD_EVAL_REQUEST_ID)),
            ],
            model_calls: MAX_NATURAL_MODEL_CALLS,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Miss
    );
}

#[test]
fn unforced_exec_tier_keeps_setup_failure_above_the_approval_cap() {
    let mut outcome = natural_exec_outcome(direct_exec_result(
        DirectExecEvidence::sandbox_setup_failure(),
    ));
    outcome.snapshot.requests.push(successful_request(
        Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
        UNSANDBOXED_EXEC_NAME,
        serde_json::json!({}),
    ));
    outcome.snapshot.requests.push(successful_request(
        Uuid::from_u128(ARBITRARY_THIRD_EVAL_REQUEST_ID),
        UNSANDBOXED_EXEC_NAME,
        serde_json::json!({}),
    ));
    outcome.snapshot.requests.push(successful_request(
        Uuid::from_u128(ARBITRARY_FOURTH_EVAL_REQUEST_ID),
        UNSANDBOXED_EXEC_NAME,
        serde_json::json!({}),
    ));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_sandboxed_exec_tier_reports_setup_failure_as_infrastructure() {
    let outcome = forced_exec_outcome(
        SANDBOXED_EXEC_NAME,
        direct_exec_result(DirectExecEvidence::sandbox_setup_failure()),
    );

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_exec_tier_rejects_an_unconfined_execution() {
    let outcome = natural_exec_outcome(zero_exit_with_confinement(ZeroExitEvidence {
        confinement: ExecutionConfinement::Unsandboxed,
        stdout: "",
    }));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Miss
    );
}

#[test]
fn every_forced_exec_fixture_is_admitted_by_its_own_dispatch_case() -> EvalResult {
    let [sandboxed, unsandboxed, diagnostics] = EXEC_CASES;

    assert_forced_exec_fixture_is_admitted(sandboxed)?;
    assert_forced_exec_fixture_is_admitted(unsandboxed)?;
    assert_forced_exec_fixture_is_admitted(diagnostics)?;
    Ok(())
}

#[track_caller]
fn assert_forced_exec_fixture_is_admitted(case: &ForcedCase) -> EvalResult {
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

#[test]
fn natural_tool_sequence_bounds_tool_enabled_exchanges() {
    let sequence = ForcedToolSequence::new(None);

    assert_eq!(sequence.next(), ForcedToolOperation::Natural);
    assert_eq!(sequence.next(), ForcedToolOperation::Natural);
    assert_eq!(sequence.next(), ForcedToolOperation::Natural);
    assert_eq!(sequence.next(), ForcedToolOperation::Continuation);
}

fn successful_workspace_natural_snapshot() -> CaseSnapshot {
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
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    }
}

fn record_workspace_read_result(tracker: &OperationTracker, content: &str, truncated: bool) {
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &serde_json::json!({
            "path": WORKSPACE_SEED_PATH,
            "content": content,
            "bytes_read": content.len(),
            "total_bytes": WORKSPACE_SEED.len(),
            "truncated": truncated,
            EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
        })
        .to_string(),
    );
}

fn record_workspace_write_result(
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

#[test]
fn workspace_natural_state_requires_the_read_before_the_write() {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WRITE_FILE_NAME),
                arguments_text: serde_json::json!({
                    "content": WORKSPACE_ANSWER,
                    "path": WORKSPACE_ANSWER_PATH,
                })
                .to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(READ_FILE_NAME),
                arguments_text: serde_json::json!({"path": WORKSPACE_SEED_PATH}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.workspace_natural_requests_passed());
}

#[test]
fn workspace_natural_state_requires_the_read_to_cover_the_full_brief() {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(READ_FILE_NAME),
                arguments_text: serde_json::json!({
                    "max_bytes": 1,
                    "path": WORKSPACE_SEED_PATH,
                })
                .to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
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
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.workspace_natural_requests_passed());
}

#[test]
fn workspace_natural_state_requires_a_later_model_call_for_the_write() {
    let snapshot = CaseSnapshot {
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
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WRITE_FILE_NAME),
                arguments_text: serde_json::json!({
                    "content": WORKSPACE_ANSWER,
                    "path": WORKSPACE_ANSWER_PATH,
                })
                .to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.workspace_natural_requests_passed());
}

#[test]
fn workspace_natural_state_requires_the_read_result_before_the_write_call() {
    let mut snapshot = successful_workspace_natural_snapshot();
    snapshot.requests[0].completed_result_entry_index = Some(ARBITRARY_LATE_RESULT_ENTRY_INDEX);

    assert!(!snapshot.workspace_natural_requests_passed());
}

#[test]
fn workspace_natural_state_rejects_an_unrelated_mutation() {
    let snapshot = CaseSnapshot {
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
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_ATTEMPT_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(EDIT_FILE_NAME),
                arguments_text: serde_json::json!({
                    "new_string": "changed",
                    "old_string": WORKSPACE_SEED.trim_end(),
                    "path": WORKSPACE_SEED_PATH,
                })
                .to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.workspace_natural_requests_passed());
}

#[test]
fn workspace_natural_state_rejects_collateral_fixture_mutation() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_ANSWER_PATH),
        WORKSPACE_ANSWER,
    )?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_DRIFTED_SEED,
    )?;
    let snapshot = successful_workspace_natural_snapshot();

    assert!(!suite.natural_state_passed(&snapshot)?);
    Ok(())
}

#[test]
fn workspace_natural_state_rejects_a_collateral_directory() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_ANSWER_PATH),
        WORKSPACE_ANSWER,
    )?;
    fs::create_dir(suite.workspace.path().join(WORKSPACE_COLLATERAL_DIRECTORY))?;
    let snapshot = successful_workspace_natural_snapshot();

    assert!(!suite.natural_state_passed(&snapshot)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn workspace_natural_state_accepts_the_private_answer_mode() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let answer = suite.workspace.path().join(WORKSPACE_ANSWER_PATH);
    fs::write(&answer, WORKSPACE_ANSWER)?;
    fs::set_permissions(
        &answer,
        fs::Permissions::from_mode(WORKSPACE_PRIVATE_CREATION_MODE),
    )?;
    let snapshot = successful_workspace_natural_snapshot();

    assert!(suite.natural_state_passed(&snapshot)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn workspace_natural_state_rejects_collateral_hard_links() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let answer = suite.workspace.path().join(WORKSPACE_ANSWER_PATH);
    fs::write(&answer, WORKSPACE_ANSWER)?;
    fs::set_permissions(
        &answer,
        fs::Permissions::from_mode(WORKSPACE_PRIVATE_CREATION_MODE),
    )?;
    let source = suite.workspace.path().join(workspace_list_entry_path(0));
    let alias = suite.workspace.path().join(workspace_list_entry_path(1));
    fs::remove_file(&alias)?;
    fs::hard_link(&source, &alias)?;
    let snapshot = successful_workspace_natural_snapshot();

    assert!(!suite.natural_state_passed(&snapshot)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn workspace_natural_state_rejects_root_mode_drift() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let answer = suite.workspace.path().join(WORKSPACE_ANSWER_PATH);
    fs::write(&answer, WORKSPACE_ANSWER)?;
    fs::set_permissions(
        &answer,
        fs::Permissions::from_mode(WORKSPACE_PRIVATE_CREATION_MODE),
    )?;
    let mut permissions = fs::metadata(suite.workspace.path())?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(suite.workspace.path(), permissions)?;
    let snapshot = successful_workspace_natural_snapshot();

    assert!(!suite.natural_state_passed(&snapshot)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn workspace_natural_state_rejects_an_insecure_answer_mode() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let answer = suite.workspace.path().join(WORKSPACE_ANSWER_PATH);
    fs::write(&answer, WORKSPACE_ANSWER)?;
    fs::set_permissions(
        &answer,
        fs::Permissions::from_mode(WORKSPACE_INSECURE_CREATION_MODE),
    )?;
    let snapshot = successful_workspace_natural_snapshot();

    assert!(!suite.natural_state_passed(&snapshot)?);
    Ok(())
}

#[test]
fn workspace_natural_execution_requires_the_full_read_result() {
    let snapshot = successful_workspace_natural_snapshot();
    let tracker = OperationTracker::default();
    record_workspace_read_result(
        &tracker,
        &WORKSPACE_SEED[..WORKSPACE_FORCED_READ_MAX_BYTES],
        true,
    );

    assert!(!workspace_natural_read_result_passed(&snapshot, &tracker));
}

#[test]
fn workspace_natural_execution_accepts_the_exact_full_read_result() {
    let snapshot = successful_workspace_natural_snapshot();
    let tracker = OperationTracker::default();
    record_workspace_read_result(&tracker, WORKSPACE_SEED, false);

    assert!(workspace_natural_read_result_passed(&snapshot, &tracker));
}

#[test]
fn workspace_natural_execution_rejects_an_unknown_read_result_field() {
    let snapshot = successful_workspace_natural_snapshot();
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &serde_json::json!({
            "path": WORKSPACE_SEED_PATH,
            "content": WORKSPACE_SEED,
            "bytes_read": WORKSPACE_SEED.len(),
            "total_bytes": WORKSPACE_SEED.len(),
            "truncated": false,
            "error": "synthetic contradictory field",
            EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
        })
        .to_string(),
    );

    assert!(!workspace_natural_read_result_passed(&snapshot, &tracker));
}

#[test]
fn workspace_natural_execution_accepts_exact_read_and_write_results() {
    let snapshot = successful_workspace_natural_snapshot();
    let tracker = OperationTracker::default();
    record_workspace_read_result(&tracker, WORKSPACE_SEED, false);
    record_workspace_write_result(
        &tracker,
        WORKSPACE_ANSWER_PATH,
        WORKSPACE_ANSWER.len(),
        true,
    );

    assert!(workspace_natural_result_payloads_passed(
        &snapshot, &tracker
    ));
}

#[test]
fn workspace_natural_execution_rejects_inaccurate_write_evidence() {
    let snapshot = successful_workspace_natural_snapshot();
    let tracker = OperationTracker::default();
    record_workspace_read_result(&tracker, WORKSPACE_SEED, false);
    record_workspace_write_result(&tracker, WORKSPACE_SEED_PATH, 0, false);

    assert!(!workspace_natural_result_payloads_passed(
        &snapshot, &tracker
    ));
}

#[test]
fn workspace_natural_execution_rejects_an_unknown_write_result_field() {
    let snapshot = successful_workspace_natural_snapshot();
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
        &serde_json::json!({
            "path": WORKSPACE_ANSWER_PATH,
            "bytes_written": WORKSPACE_ANSWER.len(),
            "created": true,
            "error": "synthetic contradictory field",
            EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
        })
        .to_string(),
    );

    assert!(!workspace_natural_write_result_passed(&snapshot, &tracker));
}

#[test]
fn workspace_natural_state_propagates_inspection_failures() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    fs::create_dir(suite.workspace.path().join(WORKSPACE_ANSWER_PATH))?;
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: Vec::new(),
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(suite.natural_state_passed(&snapshot).is_err());
    Ok(())
}

fn successful_git_natural_snapshot() -> EvalResult<CaseSnapshot> {
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
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    })
}

struct GitNaturalResultFixture<'a> {
    staged_paths: usize,
    commit: &'a str,
    state_cleaned: bool,
}

fn record_git_natural_results(tracker: &OperationTracker, fixture: GitNaturalResultFixture<'_>) {
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

fn prepared_git_natural_result_case() -> EvalResult<(FamilySuite, CaseSnapshot, String)> {
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

#[test]
fn git_natural_result_payloads_accept_the_exact_results() -> EvalResult {
    let (suite, snapshot, head) = prepared_git_natural_result_case()?;
    let tracker = OperationTracker::default();
    record_git_natural_results(
        &tracker,
        GitNaturalResultFixture {
            staged_paths: GIT_NATURAL_STAGED_PATH_COUNT,
            commit: &head,
            state_cleaned: true,
        },
    );

    assert!(git_natural_result_payloads_passed(
        suite.workspace.path(),
        &snapshot,
        &tracker,
    )?);
    Ok(())
}

#[test]
fn git_natural_result_payloads_reject_an_unknown_stage_field() -> EvalResult {
    let (suite, snapshot, head) = prepared_git_natural_result_case()?;
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &serde_json::json!({
            "staged_paths": GIT_NATURAL_STAGED_PATH_COUNT,
            "error": "synthetic contradictory field",
            EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
        })
        .to_string(),
    );
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
        &serde_json::json!({
            "commit": head,
            "state_cleaned": true,
            EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
        })
        .to_string(),
    );

    assert!(!git_natural_result_payloads_passed(
        suite.workspace.path(),
        &snapshot,
        &tracker,
    )?);
    Ok(())
}

#[test]
fn git_natural_result_payloads_reject_the_wrong_staged_count() -> EvalResult {
    let (suite, snapshot, head) = prepared_git_natural_result_case()?;
    let tracker = OperationTracker::default();
    record_git_natural_results(
        &tracker,
        GitNaturalResultFixture {
            staged_paths: SYNTHETIC_WRONG_STAGED_PATH_COUNT,
            commit: &head,
            state_cleaned: true,
        },
    );

    assert!(!git_natural_result_payloads_passed(
        suite.workspace.path(),
        &snapshot,
        &tracker,
    )?);
    Ok(())
}

#[test]
fn git_natural_result_payloads_reject_the_wrong_commit() -> EvalResult {
    let (suite, snapshot, _head) = prepared_git_natural_result_case()?;
    let tracker = OperationTracker::default();
    record_git_natural_results(
        &tracker,
        GitNaturalResultFixture {
            staged_paths: GIT_NATURAL_STAGED_PATH_COUNT,
            commit: SYNTHETIC_WRONG_COMMIT_ID,
            state_cleaned: true,
        },
    );

    assert!(!git_natural_result_payloads_passed(
        suite.workspace.path(),
        &snapshot,
        &tracker,
    )?);
    Ok(())
}

#[test]
fn git_natural_result_payloads_require_cleanup() -> EvalResult {
    let (suite, snapshot, head) = prepared_git_natural_result_case()?;
    let tracker = OperationTracker::default();
    record_git_natural_results(
        &tracker,
        GitNaturalResultFixture {
            staged_paths: GIT_NATURAL_STAGED_PATH_COUNT,
            commit: &head,
            state_cleaned: false,
        },
    );

    assert!(!git_natural_result_payloads_passed(
        suite.workspace.path(),
        &snapshot,
        &tracker,
    )?);
    Ok(())
}

#[test]
fn exec_result_inspection_propagates_failures() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    fs::create_dir(suite.workspace.path().join(EXEC_RESULT_PATH))?;

    assert!(suite.exec_result_matches().is_err());
    Ok(())
}

#[test]
fn git_natural_state_requires_a_later_model_call_for_the_commit() -> EvalResult {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.git_natural_requests_passed()?);
    Ok(())
}

#[test]
fn git_natural_state_requires_the_stage_result_before_the_commit_call() -> EvalResult {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_LATE_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.git_natural_requests_passed()?);
    Ok(())
}

#[test]
fn git_natural_requests_reject_extra_staging_after_the_target_commit() -> EvalResult {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"paths": [GIT_STAGE_PATH]}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.git_natural_requests_passed()?);
    Ok(())
}

fn successful_web_natural_snapshot() -> EvalResult<CaseSnapshot> {
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
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    })
}

fn record_web_natural_results(
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

fn exact_web_search_result() -> serde_json::Value {
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

fn exact_web_fetch_result() -> serde_json::Value {
    serde_json::json!({
        "url": WEB_URL,
        "status": 200,
        "content_type": "text/plain",
        "body": WEB_FETCH_BODY,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
}

#[test]
fn web_natural_execution_accepts_exact_search_and_fetch_results() -> EvalResult {
    let snapshot = successful_web_natural_snapshot()?;
    let tracker = OperationTracker::default();
    record_web_natural_results(
        &tracker,
        exact_web_search_result(),
        exact_web_fetch_result(),
    );

    assert!(web_natural_result_payloads_passed(&snapshot, &tracker));
    Ok(())
}

#[test]
fn web_natural_execution_rejects_an_empty_search_result() -> EvalResult {
    let snapshot = successful_web_natural_snapshot()?;
    let tracker = OperationTracker::default();
    let empty_search = serde_json::json!({
        "results": [],
        "truncated": false,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    });
    record_web_natural_results(&tracker, empty_search, exact_web_fetch_result());

    assert!(!web_natural_result_payloads_passed(&snapshot, &tracker));
    Ok(())
}

#[test]
fn web_natural_execution_rejects_corrupted_fetch_metadata() -> EvalResult {
    let snapshot = successful_web_natural_snapshot()?;
    let tracker = OperationTracker::default();
    let corrupted_fetch = serde_json::json!({
        "url": WEB_ORIGIN,
        "status": 201,
        "content_type": "application/octet-stream",
        "body": WEB_FETCH_BODY,
        "truncated": false,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    });
    record_web_natural_results(&tracker, exact_web_search_result(), corrupted_fetch);

    assert!(!web_natural_result_payloads_passed(&snapshot, &tracker));
    Ok(())
}

#[test]
fn web_natural_state_requires_a_later_model_call_for_the_fetch() -> EvalResult {
    let snapshot = CaseSnapshot {
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
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_FETCH_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"url": WEB_URL}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.web_natural_requests_passed()?);
    Ok(())
}

#[test]
fn web_natural_state_requires_the_search_result_before_the_fetch_call() -> EvalResult {
    let snapshot = CaseSnapshot {
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
                completed_result_entry_index: Some(ARBITRARY_LATE_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_FETCH_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"url": WEB_URL}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.web_natural_requests_passed()?);
    Ok(())
}

#[test]
fn web_natural_state_requires_the_exact_query() -> EvalResult {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_SEARCH_NAME),
                arguments_text: String::from(r#"{"query":"different query"}"#),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_FETCH_NAME),
                arguments_text: serde_json::json!({"url": WEB_URL}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.web_natural_requests_passed()?);
    Ok(())
}

#[test]
fn web_natural_state_accepts_a_valid_pair_after_a_premature_fetch() -> EvalResult {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_FETCH_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"url": WEB_URL}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_SEARCH_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"query": WEB_QUERY}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_FETCH_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"url": WEB_URL}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(snapshot.web_natural_requests_passed()?);
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EvalDisposition {
    Pass,
    Miss,
    Infrastructure,
}

impl EvalDisposition {
    const fn from_passed(passed: bool) -> Self {
        if passed { Self::Pass } else { Self::Miss }
    }

    const fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Pass, Self::Pass) => Self::Pass,
            (Self::Pass, Self::Miss) | (Self::Miss, Self::Pass | Self::Miss) => Self::Miss,
            (Self::Infrastructure, Self::Pass | Self::Miss | Self::Infrastructure)
            | (Self::Pass | Self::Miss, Self::Infrastructure) => Self::Infrastructure,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Miss => "MISS",
            Self::Infrastructure => "INFRA",
        }
    }
}

struct FamilyReport {
    family: EvalFamily,
    forced: Vec<CaseOutcome>,
    natural: CaseOutcome,
    natural_state: EvalDisposition,
}

fn write_report(report: &FamilyReport) -> EvalResult {
    let summary_path = std::env::var_os(SUMMARY_VARIABLE)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("the tool-eval summary path is missing"))?;
    let mut markdown = format!(
        "## {} daemon tool eval — `{}`\n\n### Forced tier\n\n| Tool | Result | Calls observed | Tool result round-trips | Turn |\n| --- | --- | --- | ---: | --- |\n",
        report.family.as_str(),
        report.family.model(),
    );
    for outcome in &report.forced {
        let target = outcome.target.as_deref().unwrap_or("missing target");
        let result = outcome.forced_disposition().label();
        let turn = outcome.snapshot.turn_disposition.label();
        markdown.push_str(&format!(
            "| `{target}` | {result} | {} | {} | `{turn}` |\n",
            outcome.snapshot.called_names(),
            outcome.round_tripped_result_count(),
        ));
    }
    let natural = report
        .natural
        .natural_loop_disposition(report.family)
        .and(report.natural_state);
    markdown.push_str(&format!(
        "\n### Unforced tier\n\n| Result | Calls observed | Tool result round-trips | Task state | Turn |\n| --- | --- | ---: | --- | --- |\n| {} | {} | {} | {} | `{}` |\n\nAll outcomes are report-only; a model miss does not fail this workflow.\n",
        natural.label(),
        report.natural.snapshot.called_names(),
        report.natural.round_tripped_result_count(),
        report.natural_state.label(),
        report.natural.snapshot.turn_disposition.label(),
    ));
    fs::write(summary_path, &markdown)?;
    print!("{markdown}");
    Ok(())
}
