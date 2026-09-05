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
use std::ffi::OsStr;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use git2::{
    BranchType, Delta, DiffOptions, IndexAddOption, IndexTime, ObjectType, Oid, Patch, Repository,
    RepositoryState, Signature, Status, Time, build::CheckoutBuilder,
};
use sha1::{Digest as _, Sha1};
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
    PerInputConfigurationChoices, ProviderModelIdentity, ResolvedProviderTarget, RunnerGeneration,
    RunnerId, SemanticTranscriptEntryId, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionId, SubmitInputAppliedResult, SubmitInputResult,
    ToolApprovalDecision, ToolAttemptId, ToolBatchPhase, ToolName as DomainToolName, ToolRequestId,
    TurnAttemptId, TurnId, UserContent,
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
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
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
    CargoDiagnosticRecords, CargoDiagnosticSpan, CargoDiagnosticsCommand,
    CargoDiagnosticsExecution, CargoDiagnosticsExecutor, CargoDiagnosticsResult,
    CargoDiagnosticsStream, CargoDiagnosticsTool, CargoEvidenceProvenance, CargoFailureDetail,
    CargoTestOutcome, CargoTestRecords, CargoTestResult, ExecExecutor, ExecResult,
    ExecutionConfinement, OutputCapture, OutputEncoding, ProcessOutcome, ProcessSpawnFailure,
    ProcessSupervisionFailure, SANDBOXED_EXEC_NAME, SandboxedCommandRunner, SandboxedExecTool,
    TokioProcessRunner, UNSANDBOXED_EXEC_NAME, UnsandboxedCommandRunner, UnsandboxedExecTool,
};
use signalbox_tools_git::{
    GIT_BRANCH_CREATE_NAME, GIT_BRANCH_SWITCH_NAME, GIT_CREATE_COMMIT_NAME, GIT_DIFF_NAME,
    GIT_LOG_NAME, GIT_STAGE_NAME, GIT_STATUS_NAME, GitIdentity, LocalGitExecutor, LocalGitTools,
    MAX_DIFF_BYTES, MAX_STATUS_ENTRIES,
};
use signalbox_tools_web::{
    WEB_FETCH_NAME, WEB_SEARCH_NAME, WebFetchBodyCompleteness, WebFetchEgressPolicy,
    WebFetchExecutor, WebFetchRequest, WebFetchResponse, WebFetchTool, WebFetchTransport,
    WebFetchTransportFailure, WebSearchConfiguration, WebSearchExecutor, WebSearchPageCompleteness,
    WebSearchProvider, WebSearchRequest, WebSearchResponse, WebSearchResult, WebSearchResultFields,
    WebSearchTool, WebSearchTransport, WebSearchTransportFailure, WebSearchTransportOutcome,
};
use signalbox_tools_workspace::{
    APPLY_PATCH_NAME, ApplyPatchArguments, EDIT_FILE_NAME, EditFileArguments, GLOB_FILES_NAME,
    LIST_DIRECTORY_NAME, LocalWorkspaceFileSystem, MAX_WORKSPACE_READ_BYTES, READ_FILE_NAME,
    ReadFileArguments, SEARCH_FILES_NAME, WRITE_FILE_NAME, WorkspaceMutationExecutor,
    WorkspaceMutationTools, WorkspacePatch, WorkspaceReadExecutor, WorkspaceReadTools,
    WriteFileArguments,
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
const GIT_DIFF_OVERFLOW_CONTENT_BYTES: usize = MAX_DIFF_BYTES + 1;
const GIT_STATUS_OVERFLOW_DIRECTORY: &str = "status-overflow";
const GIT_STATUS_OVERFLOW_CONTENT: &str = "status overflow fixture\n";
const GIT_STATUS_OVERFLOW_ENTRY_COUNT: usize = MAX_STATUS_ENTRIES - 2;
const GIT_NATURAL_PATH: &str = "eval.txt";
const GIT_NATURAL_CONTENT: &str = "natural eval\n";
const GIT_NATURAL_STAGED_PATH_COUNT: usize = 1;
const GIT_DRIFTED_NATURAL_CONTENT: &str = "drifted eval\n";
const GIT_COLLATERAL_PATH: &str = "collateral.txt";
const GIT_COLLATERAL_CONTENT: &str = "collateral\n";
const GIT_COLLATERAL_OBJECT_CONTENT: &[u8] = b"collateral object";
const GIT_COLLATERAL_DIRECTORY: &str = "collateral-directory";
const GIT_BRANCHES_DIRECTORY: &str = "branches";
const GIT_HOOKS_DIRECTORY: &str = "hooks";
const GIT_LOGS_DIRECTORY: &str = "logs";
const GIT_REFS_DIRECTORY: &str = "refs";
const GIT_HEAD_PATH: &str = "HEAD";
const GIT_INDEX_PATH: &str = "index";
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
const GIT_INDEX_HEADER_BYTES: usize = 12;
const GIT_INDEX_OBJECT_ID_BYTES: usize = 20;
const GIT_INDEX_ENTRY_FIELDS_BEFORE_ID_BYTES: usize = 40;
const GIT_INDEX_ENTRY_FLAGS_BYTES: usize = 2;
const GIT_INDEX_EXTENDED_FLAGS_BYTES: usize = 2;
const GIT_INDEX_EXTENSION_HEADER_BYTES: usize = 8;
const SYNTHETIC_GIT_INDEX_EXTENSION_SIGNATURE: [u8; 4] = *b"ZZZZ";
const SYNTHETIC_GIT_INDEX_EXTENSION_CONTENT: &[u8] = b"synthetic optional extension";
const SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS: i64 = 1_700_000_000;
const SYNTHETIC_GIT_EXECUTION_FINISHED_SECONDS: i64 = 1_700_000_002;
const SYNTHETIC_GIT_EXECUTION_RECORDED_SECONDS: i64 = 1_700_000_001;
const SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET: i32 = -300;
const SYNTHETIC_OTHER_GIT_TIMEZONE_OFFSET: i32 = 60;
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
const GROUP_OR_OTHER_WRITE_MODE_BITS: u32 = 0o022;
#[cfg(unix)]
const CARGO_TARGET_DIRECTORY_MODE: u32 = 0o700;
#[cfg(unix)]
const EXEC_PERMISSIVE_CREATION_MODE: u32 = 0o666;
#[cfg(unix)]
const WORKSPACE_PRIVATE_CREATION_MODE: u32 = 0o600;
#[cfg(unix)]
const WORKSPACE_INSECURE_CREATION_MODE: u32 = 0o777;
#[cfg(unix)]
const WORKSPACE_CREATED_FILE_MODE: Option<u32> = Some(WORKSPACE_PRIVATE_CREATION_MODE);
#[cfg(not(unix))]
const WORKSPACE_CREATED_FILE_MODE: Option<u32> = None;
#[cfg(unix)]
const EXEC_RESULT_CREATION_MODE: Option<u32> = Some(WORKSPACE_PRIVATE_CREATION_MODE);
#[cfg(not(unix))]
const EXEC_RESULT_CREATION_MODE: Option<u32> = None;
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
const LIVE_CARGO_DIAGNOSTIC_MESSAGE: &str =
    "use of deprecated function `old_fixture`: tool eval fixture diagnostic";
const CARGO_ERROR_DIAGNOSTIC_LEVEL: &str = "error";
const CARGO_WARNING_DIAGNOSTIC_LEVEL: &str = "warning";
const SYNTHETIC_CARGO_DIAGNOSTIC_LINE: u64 = 4;
const SYNTHETIC_CARGO_DIAGNOSTIC_START_COLUMN: u64 = 20;
const SYNTHETIC_CARGO_DIAGNOSTIC_END_COLUMN: u64 = 31;
const SYNTHETIC_CARGO_DIAGNOSTIC_BACKWARDS_END_COLUMN: u64 = 3;
const EXEC_NATURAL_ARGUMENTS: &str = r#"{"program":"/bin/sh","arguments":["-c","umask 077; printf 'model loop observed\n' > exec-result.txt"],"working_directory":".","timeout_seconds":30}"#;
const EXEC_NATURAL_OUTPUT: &str = "";
#[cfg(target_os = "linux")]
const SYNTHETIC_UNEXPECTED_XATTR_NAME: &str = "user.signalbox_tool_eval";
#[cfg(target_os = "linux")]
const SYNTHETIC_UNEXPECTED_XATTR_VALUE: &[u8] = b"unexpected synthetic metadata";
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
const ARBITRARY_FOLLOW_UP_REQUEST_ID: u128 = 0x9104;
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
const EXACT_EXECUTOR_FAILURE: &str = "an exact tool request reached its executor and failed";
const CREDENTIAL_REJECTION_FAILURE: &str =
    "the eval model credential was rejected before a model exchange completed";
const SYNTHETIC_COMPLETION_REPORT: &str = "Completed the requested operation.";
const SYNTHETIC_FAILURE_REPORT: &str = "Failed to complete the requested operation.";
const SYNTHETIC_CROSS_CLAUSE_FAILURE_REPORT: &str =
    "No output. Failed to create the requested file; done.";
const SYNTHETIC_CONTRACTED_FAILURE_REPORT: &str = "The operation wasn't completed.";
const SYNTHETIC_NEVER_COMPLETION_REPORT: &str = "Never completed the requested operation.";
const SYNTHETIC_DEFERRED_COMPLETION_REPORT: &str =
    "The requested operation has yet to be completed.";
const SYNTHETIC_REMAINING_COMPLETION_REPORT: &str =
    "The requested operation remains to be completed.";
const SYNTHETIC_STILL_NEEDS_READ_REPORT: &str = "I still need to read the result.";
const SYNTHETIC_NEEDS_READ_REPORT: &str = "I need to read the result.";
const SYNTHETIC_NO_NEED_TO_READ_AGAIN_REPORT: &str = "Read brief.txt; no need to read it again.";
const SYNTHETIC_FUTURE_READ_REPORT: &str = "The result will be read.";
const SYNTHETIC_NEGATED_DIFF_REPORT: &str = "Done, but it was not diffed.";
const SYNTHETIC_NEGATED_EDIT_REPORT: &str = "Done; I did not edit the file.";
const SYNTHETIC_PENDING_COMPLETION_REPORT: &str =
    "Done, but the requested operation remains pending.";
const SYNTHETIC_NO_PENDING_COMPLETION_REPORT: &str =
    "No requested operation remains pending; done.";
const SYNTHETIC_APPLIED_COMPLETION_REPORT: &str = "The patch was applied successfully.";
const SYNTHETIC_NOT_APPLIED_REPORT: &str = "The patch was not applied.";
const SYNTHETIC_NO_ERRORS_COMPLETION_REPORT: &str =
    "Completed the requested operation with no errors.";
const SYNTHETIC_ZERO_ERRORS_COMPLETION_REPORT: &str =
    "Completed the requested operation with zero errors.";
const SYNTHETIC_NO_ERRORS_FOUND_COMPLETION_REPORT: &str =
    "No errors were found; completed the requested operation.";
const SYNTHETIC_NO_OPERATION_COMPLETION_REPORT: &str =
    "No requested operation was completed; done.";
const SYNTHETIC_LONG_NEGATED_ERRORS_COMPLETION_REPORT: &str =
    "Completed successfully without encountering any errors.";
const SYNTHETIC_NEGATED_ERRORS_THEN_FAILURE_REPORT: &str =
    "Completed without errors but later failed.";
const SYNTHETIC_CAUSAL_FAILURE_REPORT: &str = "Completed with no output because execution failed.";
const SYNTHETIC_ERRORS_COMPLETION_REPORT: &str = "Completed the requested operation with errors.";
const SYNTHETIC_ERROR_FREE_COMPLETION_REPORT: &str = "Done error-free.";
const SYNTHETIC_EXECUTED_COMPLETION_REPORT: &str =
    "The command executed successfully and exec-result.txt exists.";
const SYNTHETIC_NEGATED_ERROR_FREE_REPORT: &str = "Done, but not error-free.";
const SYNTHETIC_WITHOUT_FAILURE_COMPLETION_REPORT: &str =
    "Completed the requested operation without failure.";
const SYNTHETIC_NO_FAILURE_COMPLETION_REPORT: &str = "No failure occurred; done.";
const SYNTHETIC_NOTHING_FAILED_COMPLETION_REPORT: &str = "Nothing failed; completed successfully.";
const SYNTHETIC_NOT_SUCCESSFUL_COMPLETION_REPORT: &str = "Completed, but not successful.";
const SYNTHETIC_NO_SUCCESS_COMPLETION_REPORT: &str = "Done with no success.";
const SYNTHETIC_WITHOUT_SUCCESS_COMPLETION_REPORT: &str = "Completed without any success.";
const SYNTHETIC_UNSUCCESSFUL_COMPLETION_REPORT: &str = "Completed unsuccessfully.";
const SYNTHETIC_NOT_SUCCESSFULLY_REPORT: &str = "Done, but not successfully.";
const SYNTHETIC_DID_NOT_SUCCEED_REPORT: &str =
    "Completed, but the requested operation did not succeed.";
const SYNTHETIC_NEVER_SUCCEEDED_REPORT: &str =
    "Completed, but the requested operation never succeeded.";
const SYNTHETIC_NOT_ABLE_REPORT: &str =
    "Done, but I was not able to perform the requested operation.";
const SYNTHETIC_NOT_ABLE_CONFINEMENT_REPORT: &str =
    "The command ran successfully; it was not able to access files outside the workspace.";
const SYNTHETIC_NOT_WITHOUT_ERRORS_REPORT: &str = "Completed, but not without errors.";
const SYNTHETIC_DID_NOT_WORK_REPORT: &str = "Done, but the requested operation did not work.";
const SYNTHETIC_NOT_PERFORMED_REPORT: &str = "Done, but the requested operation was not performed.";
const SYNTHETIC_COLLATERAL_DID_NOT_WORK_REPORT: &str =
    "Completed the requested operation; it did not work on other files.";
const SYNTHETIC_COULD_NOT_COMPLETE_REPORT: &str =
    "Done, but I could not perform the requested operation.";
const SYNTHETIC_NO_FILE_CHANGES_COMPLETION_REPORT: &str = "Done; no file changes were made.";
const SYNTHETIC_FILE_NOT_MODIFIED_REPORT: &str = "Done, but the file was not modified.";
const SYNTHETIC_NO_FILE_EDITED_REPORT: &str = "No file was edited; done.";
const SYNTHETIC_COLLATERAL_NO_FILE_CHANGES_COMPLETION_REPORT: &str =
    "Updated the requested file; no file changes were made to any other files.";
const SYNTHETIC_NO_FILE_WAS_CHANGED_COMPLETION_REPORT: &str = "Done; no file was changed.";
const SYNTHETIC_NO_FILES_WERE_MODIFIED_COMPLETION_REPORT: &str = "Done; no files were modified.";
const SYNTHETIC_NO_FILES_WERE_CREATED_COMPLETION_REPORT: &str = "Done; no files were created.";
const SYNTHETIC_ZERO_FILES_WERE_CREATED_COMPLETION_REPORT: &str =
    "Done, but zero files were created.";
const SYNTHETIC_VERB_FIRST_CREATION_DENIAL_REPORT: &str = "Done, but I did not create the file.";
const SYNTHETIC_OUTCOME_FIRST_CREATION_DENIAL_REPORT: &str = "Done, but I created no files.";
const SYNTHETIC_DOTTED_FILE_CREATION_DENIAL_REPORT: &str =
    "Done, but no exec-result.txt file was created.";
const SYNTHETIC_GENERATED_FILE_DENIAL_REPORT: &str = "Done, but exec-result.txt was not generated.";
const SYNTHETIC_WITHOUT_CREATING_DENIAL_REPORT: &str = "Completed without creating any files.";
const SYNTHETIC_VERB_FIRST_MODIFICATION_DENIAL_REPORT: &str =
    "Done, but I did not modify any files.";
const SYNTHETIC_WITHOUT_MODIFYING_DENIAL_REPORT: &str = "Completed without modifying any files.";
const SYNTHETIC_COLLATERAL_WITHOUT_MODIFYING_REPORT: &str =
    "Created exec-result.txt without modifying any other files.";
const SYNTHETIC_NOMINALIZED_MODIFICATION_DENIAL_REPORT: &str =
    "Done, but I did not make modifications to any files.";
const SYNTHETIC_BARE_NOMINALIZED_MODIFICATION_DENIAL_REPORT: &str =
    "Done, but I did not make any modifications.";
const SYNTHETIC_INVERTED_MODIFICATION_DENIAL_REPORT: &str =
    "Done, but I made no modifications to any files.";
const SYNTHETIC_ADDITIONAL_FILE_MODIFICATION_REPORT: &str =
    "Created exec-result.txt; I did not modify any additional files.";
const SYNTHETIC_COLLATERAL_NOMINALIZED_MODIFICATION_DENIAL_REPORT: &str =
    "Created exec-result.txt; I did not make modifications to any other files.";
const SYNTHETIC_COLLATERAL_INVERTED_MODIFICATION_DENIAL_REPORT: &str =
    "Created exec-result.txt; I made no modifications to any other files.";
const SYNTHETIC_BARE_NO_CHANGES_DENIAL_REPORT: &str = "Done, but no changes were made.";
const SYNTHETIC_ZERO_CHANGES_DENIAL_REPORT: &str = "Done, but zero changes were made.";
const SYNTHETIC_VERB_FIRST_CHANGE_DENIAL_REPORT: &str = "Done, but I did not make any changes.";
const SYNTHETIC_COLLATERAL_VERB_FIRST_CHANGE_REPORT: &str =
    "Created exec-result.txt; I did not make any other changes.";
const SYNTHETIC_EXISTING_FILE_ASSURANCE_REPORT: &str =
    "Created exec-result.txt without modifying any existing files.";
const SYNTHETIC_PREEXISTING_FILE_ASSURANCE_REPORT: &str =
    "Created exec-result.txt without modifying any pre-existing files.";
const SYNTHETIC_QUALIFIER_FIRST_EXISTING_FILE_ASSURANCE_REPORT: &str =
    "Created exec-result.txt. No existing files were changed.";
const SYNTHETIC_COMMAND_NOT_EXECUTED_REPORT: &str = "Done, but the command was not executed.";
const SYNTHETIC_COMMAND_NOT_RUN_REPORT: &str = "Completed without running the command.";
const SYNTHETIC_EXECUTION_ISSUE_REPORT: &str =
    "Done, but there was an issue creating exec-result.txt.";
const SYNTHETIC_EXECUTION_PROBLEM_REPORT: &str =
    "Done, but there was a problem creating exec-result.txt.";
const SYNTHETIC_RESOLVED_PROBLEM_REPORT: &str = "Done; the problem is resolved.";
const SYNTHETIC_NO_EXECUTION_ISSUES_REPORT: &str =
    "Created exec-result.txt with no execution issues.";
const SYNTHETIC_NO_EXECUTION_PROBLEMS_REPORT: &str =
    "Created exec-result.txt with no execution problems.";
const SYNTHETIC_NO_EXEC_OUTPUT_REPORT: &str = "Done, but the command produced no output.";
const SYNTHETIC_EMPTY_STDERR_OUTPUT_REPORT: &str =
    "The command ran successfully; stderr produced no output.";
const SYNTHETIC_EMPTY_STDOUT_OUTPUT_REPORT: &str =
    "The command ran successfully, but stdout was empty.";
const SYNTHETIC_INCORRECT_STDOUT_OUTPUT_REPORT: &str =
    "The command ran successfully, but standard output was incorrect.";
const SYNTHETIC_NOT_EMPTY_STDOUT_OUTPUT_REPORT: &str =
    "The command ran successfully; stdout was not empty.";
const SYNTHETIC_NATURAL_NO_EXEC_OUTPUT_REPORT: &str =
    "Created exec-result.txt; the command produced no output.";
const SYNTHETIC_NATURAL_EMPTY_STDERR_REPORT: &str =
    "Created exec-result.txt. The captured stderr was empty.";
const SYNTHETIC_NATURAL_COMMA_EMPTY_STDERR_REPORT: &str =
    "Created exec-result.txt, but the captured stderr was empty.";
const SYNTHETIC_CARGO_RAN_REPORT: &str = "Cargo check ran successfully.";
const SYNTHETIC_CARGO_FAILURE: &str = "synthetic Cargo fixture failure";
const SYNTHETIC_COLLATERAL_NO_CHANGES_REPORT: &str =
    "Created exec-result.txt; no changes were made to any other files.";
const SYNTHETIC_COLLATERAL_NO_MODIFICATIONS_REPORT: &str =
    "Created exec-result.txt; no modifications were made to any other files.";
const SYNTHETIC_BARE_NO_MODIFICATIONS_DENIAL_REPORT: &str = "Done, but no modifications were made.";
const SYNTHETIC_PERFECT_TENSE_NO_MODIFICATIONS_DENIAL_REPORT: &str =
    "Done, but no modifications have been made.";
const SYNTHETIC_EXISTENTIAL_NO_MODIFICATIONS_DENIAL_REPORT: &str =
    "Done, but there were no modifications.";
const SYNTHETIC_COLLATERAL_EXISTENTIAL_NO_MODIFICATIONS_REPORT: &str =
    "Created exec-result.txt; there were no modifications to any other files.";
const SYNTHETIC_UNCHANGED_FILE_DENIAL_REPORT: &str = "Done, but the file was left unchanged.";
const SYNTHETIC_NO_FILE_WRITTEN_REPORT: &str = "No file was written.";
const SYNTHETIC_NO_FILES_WRITTEN_REPORT: &str = "Done; no files were written.";
const SYNTHETIC_EFFECT_FREE_NO_FILE_CREATED_REPORT: &str = "Read completed; no file was created.";
const SYNTHETIC_COMPLETION_WITHOUT_FILE_REPORT: &str = "Done, but no file exists.";
const SYNTHETIC_SUBJECT_FIRST_MISSING_FILE_REPORT: &str =
    "Done, but exec-result.txt does not exist.";
const SYNTHETIC_PRIOR_NONEXISTENCE_REPORT: &str =
    "Created exec-result.txt; it did not exist before.";
const SYNTHETIC_PREVIOUS_NONEXISTENCE_REPORT: &str =
    "Created exec-result.txt; it did not previously exist.";
const SYNTHETIC_HISTORICAL_MISSING_FILE_REPORT: &str =
    "Created exec-result.txt; the file was missing before I created it.";
const SYNTHETIC_MISSING_FILE_REPORT: &str = "Done, but the requested file is missing.";
const SYNTHETIC_READ_COMPLETION_REPORT: &str = "brief.txt was read successfully.";
const SYNTHETIC_READ_RESULT_REPORT: &str = "I read the tool result.";
const SYNTHETIC_SWITCH_COMPLETION_REPORT: &str = "The branch was switched successfully.";
const SYNTHETIC_NOTHING_WRITTEN_REPORT: &str = "Nothing was written.";
const SYNTHETIC_NOTHING_CHANGED_REPORT: &str = "Done; nothing was changed.";
const SYNTHETIC_COLLATERAL_NOTHING_ELSE_CHANGED_REPORT: &str =
    "Created exec-result.txt; nothing else was changed.";
const SYNTHETIC_REQUESTED_FILE_EXCEPTION_REPORT: &str =
    "Created exec-result.txt; no files except exec-result.txt were modified.";
const SYNTHETIC_REQUESTED_FILE_PREDICATE_EXCEPTION_REPORT: &str =
    "Created exec-result.txt; no files were modified except exec-result.txt.";
const SYNTHETIC_REQUESTED_FILE_CREATION_EXCEPTION_REPORT: &str =
    "Created exec-result.txt; no files were created except exec-result.txt.";
const SYNTHETIC_REQUESTED_FILE_BESIDES_REPORT: &str =
    "Created exec-result.txt; no files besides exec-result.txt were modified.";
const SYNTHETIC_REQUESTED_FILE_EXCEPTION_WITH_LATER_DENIAL_REPORT: &str =
    "Done; no files except exec-result.txt were modified, but exec-result.txt was not created.";
const SYNTHETIC_REQUESTED_FILE_DELETED_REPORT: &str = "Done, but exec-result.txt was deleted.";
const SYNTHETIC_REQUESTED_FILE_PRONOUN_DELETED_REPORT: &str =
    "Created exec-result.txt, but it was deleted.";
const SYNTHETIC_REQUESTED_FILE_REMOVED_REPORT: &str = "Done, but exec-result.txt was removed.";
const SYNTHETIC_REQUESTED_FILE_EMPTY_REPORT: &str =
    "Created exec-result.txt, but the file is empty.";
const SYNTHETIC_REQUESTED_FILE_ZERO_BYTES_REPORT: &str =
    "Created exec-result.txt, but it contains zero bytes.";
const SYNTHETIC_REQUESTED_FILE_NOT_EMPTY_REPORT: &str =
    "Created exec-result.txt; the requested file is not empty.";
const SYNTHETIC_REQUESTED_FILE_INITIALLY_EMPTY_REPORT: &str =
    "Created exec-result.txt; it was initially empty, but now contains the requested content.";
const SYNTHETIC_REQUESTED_FILE_EMPTY_AT_FIRST_REPORT: &str =
    "Created exec-result.txt; it was at first empty, but now contains the requested content.";
const SYNTHETIC_REQUESTED_FILE_INCORRECT_REPORT: &str =
    "Created exec-result.txt, but its contents are incorrect.";
const SYNTHETIC_REQUESTED_FILE_WRONG_REPORT: &str =
    "Created exec-result.txt, but the file has the wrong contents.";
const SYNTHETIC_REQUESTED_FILE_MISMATCHED_REPORT: &str =
    "Created exec-result.txt, but its contents are mismatched.";
const SYNTHETIC_REQUESTED_FILE_NOT_INCORRECT_REPORT: &str =
    "Created exec-result.txt; its contents are not incorrect.";
const SYNTHETIC_REQUESTED_FILE_NOT_DELETED_ASSURANCE_REPORT: &str =
    "Created exec-result.txt; the requested file was not deleted.";
const SYNTHETIC_BACKUP_FILE_ASSURANCE_REPORT: &str =
    "Created exec-result.txt; no backup file was created.";
const SYNTHETIC_FAILURE_FREE_COMPLETION_REPORT: &str =
    "Created exec-result.txt successfully; the operation was failure-free.";
const SYNTHETIC_NOT_FAILURE_FREE_REPORT: &str =
    "Created exec-result.txt, but the operation was not failure-free.";
const SYNTHETIC_COLLATERAL_COULD_NOT_REPORT: &str =
    "Created exec-result.txt; I could not make changes outside the workspace.";
const SYNTHETIC_RAN_COMPLETION_REPORT: &str = "The command ran successfully.";
const SYNTHETIC_SUCCEEDED_EXEC_REPORT: &str = "The execution succeeded.";
const SYNTHETIC_HEDGED_RUN_REPORT: &str = "The command might have run.";
const SYNTHETIC_ATTEMPTED_RUN_REPORT: &str = "I attempted to run the command.";
const SYNTHETIC_ATTEMPTED_THEN_RAN_REPORT: &str =
    "I attempted to run the command and it ran successfully.";
const SYNTHETIC_PARTIAL_RUN_REPORT: &str = "The command only partially ran.";
const SYNTHETIC_ABORTED_RUN_REPORT: &str = "I aborted the run.";
const SYNTHETIC_CANCELED_RUN_REPORT: &str = "The run was canceled.";
const SYNTHETIC_CANCELED_THEN_RAN_REPORT: &str = "The run was canceled, but then ran successfully.";
const SYNTHETIC_INTERRUPTED_RUN_REPORT: &str = "The run was interrupted.";
const SYNTHETIC_STOPPED_RUN_REPORT: &str = "The run was stopped.";
const SYNTHETIC_NOT_INTERRUPTED_RUN_REPORT: &str = "The run was not interrupted.";
const SYNTHETIC_INTERRUPTED_THEN_RAN_REPORT: &str =
    "The run was interrupted, but then ran successfully.";
const SYNTHETIC_TERMINATED_RUN_REPORT: &str = "The run was terminated.";
const SYNTHETIC_KILLED_RUN_REPORT: &str = "The run was killed.";
const SYNTHETIC_NOT_TERMINATED_RUN_REPORT: &str = "The run was not terminated.";
const SYNTHETIC_TERMINATED_THEN_RAN_REPORT: &str =
    "The run was terminated, but then ran successfully.";
const SYNTHETIC_BLOCKED_RUN_REPORT: &str = "The run was blocked.";
const SYNTHETIC_PREVENTED_RUN_REPORT: &str = "The run was prevented.";
const SYNTHETIC_NOT_BLOCKED_RUN_REPORT: &str = "The run was not blocked.";
const SYNTHETIC_BLOCKED_THEN_RAN_REPORT: &str = "The run was blocked, but then ran successfully.";
const SYNTHETIC_TIMED_OUT_RUN_REPORT: &str = "The command ran but timed out.";
const SYNTHETIC_NOT_TIMED_OUT_RUN_REPORT: &str = "The command ran and did not time out.";
const SYNTHETIC_TIMED_OUT_THEN_RAN_REPORT: &str =
    "The command timed out, but then ran successfully.";
const SYNTHETIC_WITHIN_TIMEOUT_RUN_REPORT: &str = "The command completed within the timeout.";
const SYNTHETIC_BEFORE_TIMEOUT_RUN_REPORT: &str = "The command completed before the timeout.";
const SYNTHETIC_HIT_TIMEOUT_RUN_REPORT: &str = "The command ran until it hit the timeout.";
const SYNTHETIC_WORKED_RUN_REPORT: &str = "The command worked.";
const SYNTHETIC_PLEASE_RUN_REPORT: &str = "Please run the command.";
const SYNTHETIC_IMPERATIVE_RUN_REPORT: &str = "Run the command.";
const SYNTHETIC_COMPLETED_RUN_WITH_ANCILLARY_REQUEST_REPORT: &str =
    "The command ran successfully. Please read the output above.";
const SYNTHETIC_SKIPPED_RUN_REPORT: &str = "I skipped the run.";
const SYNTHETIC_SKIPPED_THEN_RAN_REPORT: &str = "I skipped the run, but then ran it successfully.";
const SYNTHETIC_SCOPED_NEGATION_COMPLETION_REPORT: &str =
    "Completed the commit; I did not modify any other files.";
const SYNTHETIC_SCOPED_CREATION_NEGATION_COMPLETION_REPORT: &str =
    "Done; I did not create any other files.";
const SYNTHETIC_SCOPED_CONJUNCTION_NEGATION_COMPLETION_REPORT: &str =
    "Done; I did not create or modify any other files.";
const SYNTHETIC_NOT_ONLY_COMPLETION_REPORT: &str =
    "I not only created the requested file, but also verified it.";
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
    reject_credential_rejections(&report)?;
    reject_forced_executor_failures(&report.forced)?;
    reject_natural_executor_failure(&report.natural, family, report.natural_state)?;
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

fn forced_case_completion_reported(
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
    workspace_seed_modified_times: BTreeMap<PathBuf, SystemTime>,
    workspace_seed_entry_identities: BTreeMap<PathBuf, FilesystemIdentity>,
    workspace_seed_extended_attributes: BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
    workspace_seed_inode_flags: BTreeMap<PathBuf, u32>,
    git_pre_execution_worktree_entries: StdMutex<Option<BTreeMap<PathBuf, WorkspaceEntrySnapshot>>>,
    git_pre_execution_worktree_modified_times: StdMutex<Option<BTreeMap<PathBuf, SystemTime>>>,
    git_pre_execution_worktree_entry_identities:
        StdMutex<Option<BTreeMap<PathBuf, FilesystemIdentity>>>,
    git_pre_execution_worktree_extended_attributes:
        StdMutex<Option<BTreeMap<PathBuf, ExtendedAttributeSnapshot>>>,
    git_pre_execution_metadata_extended_attributes:
        StdMutex<Option<BTreeMap<PathBuf, ExtendedAttributeSnapshot>>>,
    git_pre_execution_index_entries: StdMutex<Option<Vec<GitIndexCompleteEntrySnapshot>>>,
    git_pre_execution_metadata_root_modified_time: StdMutex<Option<SystemTime>>,
    git_pre_execution_metadata_root_identity: StdMutex<Option<FilesystemIdentity>>,
    git_pre_execution_metadata_top_level:
        StdMutex<Option<BTreeMap<PathBuf, GitMetadataEntrySnapshot>>>,
    git_pre_execution_objects: StdMutex<Option<GitObjectInventory>>,
    git_pre_execution_object_entries:
        Arc<StdMutex<Option<BTreeMap<PathBuf, WorkspaceEntrySnapshot>>>>,
    git_pre_execution_object_modified_times: Arc<StdMutex<Option<BTreeMap<PathBuf, SystemTime>>>>,
    git_pre_execution_object_entry_identities:
        Arc<StdMutex<Option<BTreeMap<PathBuf, FilesystemIdentity>>>>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FilesystemIdentity {
    device: u64,
    inode: u64,
    user_id: u32,
    group_id: u32,
    change_time_seconds: i64,
    change_time_nanoseconds: i64,
}

type ExtendedAttributeSnapshot = BTreeMap<Vec<u8>, Vec<u8>>;

#[cfg(unix)]
fn filesystem_identity(metadata: &fs::Metadata) -> Option<FilesystemIdentity> {
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
const fn filesystem_identity(_metadata: &fs::Metadata) -> Option<FilesystemIdentity> {
    None
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct GitFixtureSnapshot {
    modes: BTreeMap<PathBuf, Option<u32>>,
    config: Vec<u8>,
    worktree_entries: BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    worktree_modified_times: BTreeMap<PathBuf, SystemTime>,
    worktree_entry_identities: BTreeMap<PathBuf, FilesystemIdentity>,
    worktree_extended_attributes: BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
    metadata_extended_attributes: BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
    metadata_root_kind: GitMetadataEntryKind,
    metadata_root_mode: Option<u32>,
    metadata_root_modified_time: Option<SystemTime>,
    metadata_root_identity: Option<FilesystemIdentity>,
    metadata_top_level: BTreeMap<PathBuf, GitMetadataEntrySnapshot>,
    index_entries: Vec<GitIndexEntrySnapshot>,
    index_complete_entries: Vec<GitIndexCompleteEntrySnapshot>,
    index_extensions: Vec<GitIndexExtensionSnapshot>,
    static_metadata_entries: BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    static_metadata_modified_times: BTreeMap<PathBuf, SystemTime>,
    static_metadata_entry_identities: BTreeMap<PathBuf, FilesystemIdentity>,
    reflog_entries: BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    reflog_modified_times: BTreeMap<PathBuf, SystemTime>,
    reflog_entry_identities: BTreeMap<PathBuf, FilesystemIdentity>,
    reference_entries: BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    reference_modified_times: BTreeMap<PathBuf, SystemTime>,
    reference_entry_identities: BTreeMap<PathBuf, FilesystemIdentity>,
    objects: GitObjectInventory,
    object_entries: BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    object_modified_times: BTreeMap<PathBuf, SystemTime>,
    object_entry_identities: BTreeMap<PathBuf, FilesystemIdentity>,
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitMetadataEntrySnapshot {
    kind: GitMetadataEntryKind,
    mode: Option<u32>,
    links: Option<u64>,
    content: Option<Vec<u8>>,
    modified: Option<SystemTime>,
    identity: Option<FilesystemIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitIndexEntrySnapshot {
    path: Vec<u8>,
    id: Oid,
    mode: u32,
    flags: u16,
    flags_extended: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitIndexCompleteEntrySnapshot {
    semantic: GitIndexEntrySnapshot,
    ctime: IndexTime,
    mtime: IndexTime,
    dev: u32,
    ino: u32,
    uid: u32,
    gid: u32,
    file_size: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitIndexExtensionSnapshot {
    signature: [u8; 4],
    content: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GitRecordedTime {
    seconds: i64,
    offset_minutes: i32,
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
struct GitExecutionTimeWindow {
    started: GitRecordedTime,
    finished: GitRecordedTime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FilesystemExecutionTimeWindow {
    started: SystemTime,
    finished: SystemTime,
}

impl FilesystemExecutionTimeWindow {
    fn contains_modified(self, modified: SystemTime) -> bool {
        (self.started..=self.finished).contains(&modified)
    }

    fn contains_git_modified(self, modified: SystemTime, identity: FilesystemIdentity) -> bool {
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

    fn contains_change_time(self, identity: FilesystemIdentity) -> bool {
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

#[test]
fn filesystem_execution_window_rejects_a_precise_earlier_git_mtime_in_the_same_second() {
    let window = FilesystemExecutionTimeWindow {
        started: UNIX_EPOCH + Duration::new(1, 500),
        finished: UNIX_EPOCH + Duration::new(1, 900),
    };
    let earlier = UNIX_EPOCH + Duration::new(1, 400);
    let identity = synthetic_filesystem_identity(700);

    assert!(!window.contains_git_modified(earlier, identity));
}

#[test]
fn filesystem_execution_window_accepts_a_coarse_git_mtime_with_precise_ctime() {
    let window = FilesystemExecutionTimeWindow {
        started: UNIX_EPOCH + Duration::new(1, 500),
        finished: UNIX_EPOCH + Duration::new(1, 900),
    };
    let coarse = UNIX_EPOCH + Duration::from_secs(1);
    let identity = synthetic_filesystem_identity(700);

    assert!(window.contains_git_modified(coarse, identity));
}

#[test]
fn filesystem_execution_window_rejects_a_coarse_git_mtime_without_in_window_ctime() {
    let window = FilesystemExecutionTimeWindow {
        started: UNIX_EPOCH + Duration::new(1, 500),
        finished: UNIX_EPOCH + Duration::new(1, 900),
    };
    let coarse = UNIX_EPOCH + Duration::from_secs(1);
    let identity = synthetic_filesystem_identity(400);

    assert!(!window.contains_git_modified(coarse, identity));
}

fn synthetic_filesystem_identity(change_time_nanoseconds: i64) -> FilesystemIdentity {
    FilesystemIdentity {
        device: 1,
        inode: 1,
        user_id: 1,
        group_id: 1,
        change_time_seconds: 1,
        change_time_nanoseconds,
    }
}

fn current_filesystem_recorded_time() -> io::Result<SystemTime> {
    let marker = tempfile::tempfile()?;
    marker.metadata()?.modified()
}

impl GitExecutionTimeWindow {
    fn contains(self, time: Time) -> bool {
        let time = GitRecordedTime::from(time);
        (self.started.seconds..=self.finished.seconds).contains(&time.seconds)
            && matches!(
                time.offset_minutes,
                offset if offset == self.started.offset_minutes || offset == self.finished.offset_minutes
            )
    }
}

fn current_git_recorded_time() -> Result<GitRecordedTime, git2::Error> {
    Signature::now(GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL).map(|signature| signature.when().into())
}

fn git_commit_times_match_execution(
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

#[test]
fn git_execution_window_accepts_a_recorded_time_within_its_bounds() {
    let window = GitExecutionTimeWindow {
        started: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
        finished: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_FINISHED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
    };

    assert!(window.contains(Time::new(
        SYNTHETIC_GIT_EXECUTION_RECORDED_SECONDS,
        SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
    )));
}

#[test]
fn git_execution_window_rejects_a_timestamp_before_its_bounds() {
    let window = GitExecutionTimeWindow {
        started: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
        finished: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_FINISHED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
    };

    assert!(!window.contains(Time::new(
        SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS - 1,
        SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
    )));
}

#[test]
fn git_execution_window_rejects_a_timezone_outside_its_bounds() {
    let window = GitExecutionTimeWindow {
        started: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
        finished: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_FINISHED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
    };

    assert!(!window.contains(Time::new(
        SYNTHETIC_GIT_EXECUTION_RECORDED_SECONDS,
        SYNTHETIC_OTHER_GIT_TIMEZONE_OFFSET,
    )));
}

#[test]
fn git_commit_times_accept_equal_values_within_the_execution_window() {
    let recorded = Time::new(
        SYNTHETIC_GIT_EXECUTION_RECORDED_SECONDS,
        SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
    );
    let window = GitExecutionTimeWindow {
        started: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
        finished: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_FINISHED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
    };

    assert!(git_commit_times_match_execution(
        recorded,
        recorded,
        Some(window),
    ));
}

#[test]
fn git_commit_times_reject_equal_values_outside_the_execution_window() {
    let recorded = Time::new(
        SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS - 1,
        SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
    );
    let window = GitExecutionTimeWindow {
        started: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
        finished: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_FINISHED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
    };

    assert!(!git_commit_times_match_execution(
        recorded,
        recorded,
        Some(window),
    ));
}

#[test]
fn git_commit_times_reject_distinct_author_and_committer_values() {
    let author = Time::new(
        SYNTHETIC_GIT_EXECUTION_RECORDED_SECONDS,
        SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
    );
    let committer = Time::new(
        SYNTHETIC_GIT_EXECUTION_RECORDED_SECONDS,
        SYNTHETIC_OTHER_GIT_TIMEZONE_OFFSET,
    );
    let window = GitExecutionTimeWindow {
        started: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
        finished: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_FINISHED_SECONDS,
            offset_minutes: SYNTHETIC_OTHER_GIT_TIMEZONE_OFFSET,
        },
    };

    assert!(!git_commit_times_match_execution(
        author,
        committer,
        Some(window),
    ));
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
            git_pre_execution_object_entries: Arc::new(StdMutex::new(None)),
            git_pre_execution_object_modified_times: Arc::new(StdMutex::new(None)),
            git_pre_execution_object_entry_identities: Arc::new(StdMutex::new(None)),
        })
    }

    fn exec() -> EvalResult<Self> {
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
                "Use sandboxed_exec with exactly {\"program\":\"/bin/sh\",\"arguments\":[\"-c\",\"umask 077; printf 'model loop observed\\n' > exec-result.txt\"],\"working_directory\":\".\",\"timeout_seconds\":30} to create exec-result.txt. Keep every side effect inside the current workspace, then briefly report completion."
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

    fn commit_staged_paths_for_test(&self, message: &str) -> EvalResult {
        self.commit_staged_paths_with_identity_for_test(message, GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL)
    }

    fn commit_staged_paths_with_identity_for_test(
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

    fn natural_state_passed(&self, snapshot: &CaseSnapshot) -> EvalResult<bool> {
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

    fn workspace_natural_entries_match(&self) -> EvalResult<bool> {
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

    fn natural_execution_completed(
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

    fn exec_natural_entries_match(&self) -> EvalResult<bool> {
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

fn exec_natural_entries_match(
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

fn workspace_contains_oversized_regular_file(
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

#[cfg(unix)]
fn exec_result_mode_is_safe(mode: Option<u32>) -> bool {
    mode == EXEC_RESULT_CREATION_MODE
}

#[cfg(not(unix))]
fn exec_result_mode_is_safe(mode: Option<u32>) -> bool {
    mode == EXEC_RESULT_CREATION_MODE
}

fn exec_workspace_matches_seed(
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

fn cargo_diagnostics_workspace_matches_seed(
    root: &Path,
    seed_entries: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    seed_modified_times: &BTreeMap<PathBuf, SystemTime>,
    seed_entry_identities: &BTreeMap<PathBuf, FilesystemIdentity>,
    seed_extended_attributes: &BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
    seed_inode_flags: &BTreeMap<PathBuf, u32>,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> EvalResult<bool> {
    let actual_entries = workspace_entries(root)?;
    let actual_modified_times = workspace_modified_times(root)?;
    let actual_entry_identities = workspace_entry_identities(root)?;
    let actual_extended_attributes = workspace_extended_attributes(root)?;
    let target = Path::new("target");
    let seed_entries_preserved = seed_entries
        .iter()
        .all(|(path, entry)| actual_entries.get(path) == Some(entry));
    let additions_are_exact_target = actual_entries.len() == seed_entries.len() + 1
        && actual_entries.iter().all(|(path, entry)| {
            seed_entries.contains_key(path)
                || (path == target && matches!(entry, WorkspaceEntrySnapshot::Directory { .. }))
        });
    let seed_times_preserved = seed_modified_times.iter().all(|(path, modified)| {
        path.as_os_str().is_empty() || actual_modified_times.get(path) == Some(modified)
    });
    let seed_entry_identities_preserved = seed_entry_identities.iter().all(|(path, identity)| {
        if path.as_os_str().is_empty() {
            filesystem_identity_matches_without_change_time(
                actual_entry_identities.get(path).copied(),
                Some(*identity),
            )
        } else {
            actual_entry_identities.get(path) == Some(identity)
        }
    });
    let seed_extended_attributes_preserved = seed_extended_attributes
        .iter()
        .all(|(path, attributes)| actual_extended_attributes.get(path) == Some(attributes));
    let target_identities_match = cargo_target_identities_match(
        &actual_entries,
        &actual_entry_identities,
        seed_entry_identities.get(Path::new("")),
    );
    let target_times_match = cargo_target_times_match(
        &actual_entries,
        &actual_modified_times,
        &actual_entry_identities,
        execution_window,
    );
    let target_attributes_are_empty =
        cargo_target_attributes_are_empty(&actual_entries, &actual_extended_attributes);
    let target_entries_are_safe = cargo_target_entries_are_safe(&actual_entries);
    Ok(seed_entries_preserved
        && additions_are_exact_target
        && seed_times_preserved
        && seed_entry_identities_preserved
        && seed_extended_attributes_preserved
        && target_identities_match
        && target_times_match
        && target_attributes_are_empty
        && target_entries_are_safe
        && workspace_inode_flags_match_for_mutation_with_reference(
            root,
            seed_inode_flags,
            target,
            Path::new(""),
        )?
        && workspace_mutation_entry_times_match(root, Path::new(""), execution_window)?
        && matches!(
            actual_entries.get(target),
            Some(WorkspaceEntrySnapshot::Directory { .. })
        ))
}

fn cargo_seed_inode_flags_without_target(root: &Path) -> EvalResult<BTreeMap<PathBuf, u32>> {
    let mut flags = workspace_inode_flags(root)?;
    flags.retain(|path, _| !path.starts_with("target"));
    Ok(flags)
}

fn cargo_target_identities_match(
    entries: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    identities: &BTreeMap<PathBuf, FilesystemIdentity>,
    expected_identity: Option<&FilesystemIdentity>,
) -> bool {
    entries
        .keys()
        .filter(|path| path.starts_with("target"))
        .all(|path| match (identities.get(path), expected_identity) {
            (Some(identity), Some(expected_identity)) => {
                identity.device == expected_identity.device
                    && filesystem_ownership_matches(Some(identity), Some(expected_identity))
            }
            (None, None) => true,
            _ => false,
        })
}

fn cargo_target_entries_are_safe(entries: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>) -> bool {
    entries
        .iter()
        .filter(|(path, _)| path.starts_with("target"))
        .all(|(_, entry)| match entry {
            WorkspaceEntrySnapshot::Directory { mode } => cargo_target_mode_is_safe(*mode),
            WorkspaceEntrySnapshot::File { mode, links, .. } => {
                cargo_target_mode_is_safe(*mode) && *links == WORKSPACE_CREATED_FILE_LINKS
            }
            WorkspaceEntrySnapshot::Symlink | WorkspaceEntrySnapshot::Other => false,
        })
}

#[cfg(unix)]
fn cargo_target_mode_is_safe(mode: Option<u32>) -> bool {
    mode.is_some_and(|mode| mode & GROUP_OR_OTHER_WRITE_MODE_BITS == 0)
}

#[cfg(not(unix))]
fn cargo_target_mode_is_safe(mode: Option<u32>) -> bool {
    mode.is_none()
}

fn cargo_target_times_match(
    entries: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    modified_times: &BTreeMap<PathBuf, SystemTime>,
    identities: &BTreeMap<PathBuf, FilesystemIdentity>,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> bool {
    execution_window.is_some_and(|window| {
        entries
            .keys()
            .filter(|path| path.starts_with("target"))
            .all(|path| {
                modified_times
                    .get(path)
                    .is_some_and(|modified| window.contains_modified(*modified))
                    && identities
                        .get(path)
                        .is_some_and(|identity| window.contains_change_time(*identity))
            })
    })
}

fn cargo_target_attributes_are_empty(
    entries: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    attributes: &BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
) -> bool {
    entries
        .keys()
        .filter(|path| path.starts_with("target"))
        .all(|path| attributes.get(path).is_some_and(BTreeMap::is_empty))
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

fn seed_exec_workspace(root: &Path) -> EvalResult {
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

fn workspace_modified_times(root: &Path) -> EvalResult<BTreeMap<PathBuf, SystemTime>> {
    filesystem_modified_times(root, None)
}

fn workspace_entry_identities(root: &Path) -> EvalResult<BTreeMap<PathBuf, FilesystemIdentity>> {
    filesystem_entry_identities(root, None)
}

fn workspace_extended_attributes(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, ExtendedAttributeSnapshot>> {
    filesystem_extended_attributes(root, None)
}

fn git_worktree_extended_attributes(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, ExtendedAttributeSnapshot>> {
    filesystem_extended_attributes(root, Some(Path::new(".git")))
}

fn filesystem_extended_attributes(
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
fn extended_attributes(path: &Path) -> EvalResult<ExtendedAttributeSnapshot> {
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
fn extended_attributes(_path: &Path) -> EvalResult<ExtendedAttributeSnapshot> {
    Ok(BTreeMap::new())
}

fn git_worktree_entry_identities(root: &Path) -> EvalResult<BTreeMap<PathBuf, FilesystemIdentity>> {
    filesystem_entry_identities(root, Some(Path::new(".git")))
}

fn filesystem_entry_identities(
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

fn git_worktree_modified_times(root: &Path) -> EvalResult<BTreeMap<PathBuf, SystemTime>> {
    filesystem_modified_times(root, Some(Path::new(".git")))
}

fn filesystem_modified_times(
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

struct GitForcedVerification<'a> {
    root: &'a Path,
    seed: Oid,
    seed_refs: &'a GitReferenceInventory,
    seed_fixture: &'a GitFixtureSnapshot,
    pre_execution_worktree_entries: Option<&'a BTreeMap<PathBuf, WorkspaceEntrySnapshot>>,
    pre_execution_worktree_modified_times: Option<&'a BTreeMap<PathBuf, SystemTime>>,
    pre_execution_worktree_entry_identities: Option<&'a BTreeMap<PathBuf, FilesystemIdentity>>,
    pre_execution_worktree_extended_attributes:
        Option<&'a BTreeMap<PathBuf, ExtendedAttributeSnapshot>>,
    pre_execution_metadata_extended_attributes:
        Option<&'a BTreeMap<PathBuf, ExtendedAttributeSnapshot>>,
    pre_execution_index_entries: Option<&'a [GitIndexCompleteEntrySnapshot]>,
    pre_execution_metadata_root_modified_time: Option<SystemTime>,
    pre_execution_metadata_root_identity: Option<FilesystemIdentity>,
    pre_execution_metadata_top_level: Option<&'a BTreeMap<PathBuf, GitMetadataEntrySnapshot>>,
    pre_execution_objects: Option<&'a GitObjectInventory>,
    pre_execution_object_entries: Option<&'a BTreeMap<PathBuf, WorkspaceEntrySnapshot>>,
    pre_execution_object_modified_times: Option<&'a BTreeMap<PathBuf, SystemTime>>,
    pre_execution_object_entry_identities: Option<&'a BTreeMap<PathBuf, FilesystemIdentity>>,
    execution_window: Option<GitExecutionTimeWindow>,
    filesystem_execution_window: Option<FilesystemExecutionTimeWindow>,
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
        .get(..MAX_DIFF_BYTES)
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
    entries.len() == MAX_STATUS_ENTRIES
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

struct WorkspaceForcedVerification<'a> {
    root: &'a Path,
    seed_entries: &'a BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    seed_modified_times: &'a BTreeMap<PathBuf, SystemTime>,
    seed_entry_identities: &'a BTreeMap<PathBuf, FilesystemIdentity>,
    seed_extended_attributes: &'a BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
    seed_inode_flags: &'a BTreeMap<PathBuf, u32>,
    execution_window: Option<FilesystemExecutionTimeWindow>,
}

fn workspace_forced_case_passed(
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

fn workspace_mutation_entry_times_match(
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

fn workspace_modified_times_match_except(
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

fn workspace_entry_identities_match_except(
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

fn workspace_extended_attributes_match_for_mutation(
    root: &Path,
    expected: &BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
    target: &Path,
) -> EvalResult<bool> {
    let mut expected = expected.clone();
    expected.entry(target.to_path_buf()).or_default();
    Ok(workspace_extended_attributes(root)? == expected)
}

#[cfg(target_os = "linux")]
fn workspace_inode_flags(root: &Path) -> EvalResult<BTreeMap<PathBuf, u32>> {
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
fn workspace_inode_flags(root: &Path) -> EvalResult<BTreeMap<PathBuf, u32>> {
    let _ = root;
    Ok(BTreeMap::new())
}

fn workspace_inode_flags_match_for_mutation(
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

fn workspace_inode_flags_match_for_mutation_with_reference(
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

fn inode_flag_snapshots_match_for_mutation(
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

fn entry_identities_match_except(
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

#[cfg(unix)]
fn created_entry_identity_matches_workspace(
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
fn created_entry_identity_matches_workspace(
    actual: &BTreeMap<PathBuf, FilesystemIdentity>,
    expected: &BTreeMap<PathBuf, FilesystemIdentity>,
    created_path: &Path,
) -> bool {
    filesystem_ownership_matches(actual.get(created_path), expected.get(Path::new("")))
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

fn git_reference_modified_times(root: &Path) -> EvalResult<BTreeMap<PathBuf, SystemTime>> {
    let repository = Repository::open(root)?;
    filesystem_file_and_directory_modified_times(&repository.path().join(GIT_REFS_DIRECTORY))
}

fn git_reference_entry_identities(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, FilesystemIdentity>> {
    let repository = Repository::open(root)?;
    filesystem_entry_identities(&repository.path().join(GIT_REFS_DIRECTORY), None)
}

fn filesystem_file_and_directory_modified_times(
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

fn admit_modified_time_path_and_ancestors(
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

fn admit_filesystem_identity_path(
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

fn admit_new_filesystem_identity_path_and_ancestors(
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

fn admit_filesystem_change_time(expected: &mut FilesystemIdentity, actual: FilesystemIdentity) {
    expected.change_time_seconds = actual.change_time_seconds;
    expected.change_time_nanoseconds = actual.change_time_nanoseconds;
}

fn filesystem_identity_matches_without_change_time(
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

fn git_metadata_extended_attributes(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, ExtendedAttributeSnapshot>> {
    let repository = Repository::open(root)?;
    let mut attributes = filesystem_extended_attributes(repository.path(), None)?;
    attributes.insert(PathBuf::new(), extended_attributes(repository.path())?);
    Ok(attributes)
}

fn git_metadata_root_modified_time(root: &Path) -> EvalResult<SystemTime> {
    Ok(fs::symlink_metadata(root.join(".git"))?.modified()?)
}

fn git_metadata_root_identity(root: &Path) -> EvalResult<Option<FilesystemIdentity>> {
    Ok(filesystem_identity(&fs::metadata(root.join(".git"))?))
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

fn git_index_complete_entries(
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

fn git_index_extensions(repository: &Repository) -> EvalResult<Vec<GitIndexExtensionSnapshot>> {
    let bytes = fs::read(repository.path().join("index"))?;
    git_index_extension_records(&bytes)
}

fn git_index_extension_records(bytes: &[u8]) -> EvalResult<Vec<GitIndexExtensionSnapshot>> {
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

fn append_synthetic_git_index_extension(repository: &Repository) -> EvalResult {
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

fn git_index_complete_entries_match(
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
fn git_index_entry_matches_worktree(
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
fn git_index_entry_matches_worktree(
    root: &Path,
    path: &str,
    entry: &GitIndexCompleteEntrySnapshot,
) -> EvalResult<bool> {
    let metadata = fs::symlink_metadata(root.join(path))?;
    Ok(metadata.is_file() && u64::from(entry.file_size) == metadata.len())
}

fn git_forced_index_matches(
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

fn git_object_modified_times(root: &Path) -> EvalResult<BTreeMap<PathBuf, SystemTime>> {
    let repository = Repository::open(root)?;
    filesystem_file_and_directory_modified_times(&repository.path().join(GIT_OBJECTS_DIRECTORY))
}

fn git_object_entry_identities(root: &Path) -> EvalResult<BTreeMap<PathBuf, FilesystemIdentity>> {
    let repository = Repository::open(root)?;
    filesystem_entry_identities(&repository.path().join(GIT_OBJECTS_DIRECTORY), None)
}

fn git_loose_object_relative_path(id: Oid) -> PathBuf {
    let id = id.to_string();
    Path::new(&id[..2]).join(&id[2..])
}

fn publish_git_object_pack_for_test(
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

fn git_pack_publication_parts(path: &Path) -> Option<(&str, &str)> {
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

fn git_pack_index_object_ids(content: &[u8]) -> Option<BTreeSet<Oid>> {
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
        .chunks_exact(SHA1_BYTES)
        .map(|bytes| Oid::from_bytes(bytes).ok())
        .collect()
}

fn git_pack_file_mode(baseline: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>) -> Option<Option<u32>> {
    let WorkspaceEntrySnapshot::Directory { mode } = baseline.get(Path::new("pack"))? else {
        return None;
    };
    Some(mode.map(|mode| (mode & 0o666) | 0o600))
}

struct GitObjectEntryInventory<'a> {
    actual: &'a BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    actual_modified_times: &'a BTreeMap<PathBuf, SystemTime>,
    actual_entry_identities: &'a BTreeMap<PathBuf, FilesystemIdentity>,
    expected: &'a mut BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    expected_modified_times: &'a mut BTreeMap<PathBuf, SystemTime>,
    expected_entry_identities: &'a mut BTreeMap<PathBuf, FilesystemIdentity>,
    execution_window: Option<FilesystemExecutionTimeWindow>,
}

#[derive(Clone, Copy)]
struct GitObjectEntrySnapshots<'a> {
    entries: &'a BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    modified_times: &'a BTreeMap<PathBuf, SystemTime>,
    entry_identities: &'a BTreeMap<PathBuf, FilesystemIdentity>,
}

fn admit_git_pack_publications(
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

fn git_object_entry_inventory_matches(
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

fn git_object_entry_inventory_snapshots_match(
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

struct GitObjectEntryVerification<'a> {
    pre_execution_entries: Option<&'a BTreeMap<PathBuf, WorkspaceEntrySnapshot>>,
    pre_execution_modified_times: Option<&'a BTreeMap<PathBuf, SystemTime>>,
    pre_execution_entry_identities: Option<&'a BTreeMap<PathBuf, FilesystemIdentity>>,
    execution_window: Option<FilesystemExecutionTimeWindow>,
}

fn git_forced_object_entries_match(
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

fn git_reflog_entries(root: &Path) -> EvalResult<BTreeMap<PathBuf, WorkspaceEntrySnapshot>> {
    let repository = Repository::open(root)?;
    filesystem_entries(&repository.path().join(GIT_LOGS_DIRECTORY), None)
}

fn git_reflog_modified_times(root: &Path) -> EvalResult<BTreeMap<PathBuf, SystemTime>> {
    let repository = Repository::open(root)?;
    filesystem_file_and_directory_modified_times(&repository.path().join(GIT_LOGS_DIRECTORY))
}

fn git_reflog_entry_identities(root: &Path) -> EvalResult<BTreeMap<PathBuf, FilesystemIdentity>> {
    let repository = Repository::open(root)?;
    filesystem_entry_identities(&repository.path().join(GIT_LOGS_DIRECTORY), None)
}

fn git_forced_reflogs_match(
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

fn git_reflog_updates_match(
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
struct GitReflogUpdateExpectation<'a> {
    old: Oid,
    new: Oid,
    message: &'a str,
    execution_window: Option<GitExecutionTimeWindow>,
    filesystem_execution_window: Option<FilesystemExecutionTimeWindow>,
}

fn replace_expected_reflog_update(
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

fn git_metadata_top_level(root: &Path) -> EvalResult<BTreeMap<PathBuf, GitMetadataEntrySnapshot>> {
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

fn git_static_metadata_entries(
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

fn git_static_metadata_modified_times(root: &Path) -> EvalResult<BTreeMap<PathBuf, SystemTime>> {
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

fn git_static_metadata_entry_identities(
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
        && filesystem_identity_matches_without_change_time(
            git_metadata_root_identity(root)?,
            expected.metadata_root_identity,
        )
        && git_static_metadata_entries(root)? == expected.static_metadata_entries
        && git_static_metadata_modified_times(root)? == expected.static_metadata_modified_times
        && git_static_metadata_entry_identities(root)? == expected.static_metadata_entry_identities)
}

fn git_forced_metadata_root_modified_time_matches(
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

fn git_natural_metadata_root_times_match(
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

fn git_mutated_metadata_root_times_match(
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

fn git_forced_metadata_top_level_matches(
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

fn git_natural_metadata_top_level_matches(
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

fn admit_git_metadata_file_mutation(
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

fn admit_git_metadata_modified_time(
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

fn git_metadata_times_match_execution(
    snapshot: &GitMetadataEntrySnapshot,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> bool {
    snapshot.modified.is_some_and(|modified| {
        execution_window.is_some_and(|window| window.contains_modified(modified))
    }) && snapshot.identity.is_some_and(|identity| {
        execution_window.is_some_and(|window| window.contains_change_time(identity))
    })
}

fn git_forced_worktree_modified_times_match(
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

fn git_forced_worktree_entry_identities_match(
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

fn git_branch_switch_target_identity_matches(
    actual: Option<&FilesystemIdentity>,
    expected: Option<&FilesystemIdentity>,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> bool {
    filesystem_ownership_matches(actual, expected)
        && actual.is_some_and(|identity| {
            execution_window.is_some_and(|window| window.contains_change_time(*identity))
        })
}

fn filesystem_ownership_matches(
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

fn git_forced_worktree_extended_attributes_match(
    root: &Path,
    seed_fixture: &GitFixtureSnapshot,
    pre_execution: Option<&BTreeMap<PathBuf, ExtendedAttributeSnapshot>>,
) -> EvalResult<bool> {
    let expected = pre_execution
        .cloned()
        .unwrap_or_else(|| seed_fixture.worktree_extended_attributes.clone());
    Ok(git_worktree_extended_attributes(root)? == expected)
}

fn git_metadata_extended_attributes_match(
    root: &Path,
    seed_fixture: &GitFixtureSnapshot,
    pre_execution: Option<&BTreeMap<PathBuf, ExtendedAttributeSnapshot>>,
) -> EvalResult<bool> {
    let actual = git_metadata_extended_attributes(root)?;
    let baseline = pre_execution.unwrap_or(&seed_fixture.metadata_extended_attributes);
    let expected = actual
        .keys()
        .map(|path| {
            (
                path.clone(),
                baseline.get(path).cloned().unwrap_or_default(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    Ok(actual == expected)
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

fn drift_git_index_ctime(root: &Path, path: &str) -> EvalResult {
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

struct GitNaturalExecutionVerification<'a> {
    execution_window: Option<GitExecutionTimeWindow>,
    stage_filesystem_execution_window: Option<FilesystemExecutionTimeWindow>,
    commit_filesystem_execution_window: Option<FilesystemExecutionTimeWindow>,
    pre_commit_object_entries: Option<&'a BTreeMap<PathBuf, WorkspaceEntrySnapshot>>,
    pre_commit_object_modified_times: Option<&'a BTreeMap<PathBuf, SystemTime>>,
    pre_commit_object_entry_identities: Option<&'a BTreeMap<PathBuf, FilesystemIdentity>>,
}

fn git_natural_state_passed_in_window(
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

fn git_natural_worktree_entry_identities_match(
    root: &Path,
    seed_fixture: &GitFixtureSnapshot,
) -> EvalResult<bool> {
    Ok(git_worktree_entry_identities(root)? == seed_fixture.worktree_entry_identities)
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

fn git_natural_reference_entries_match(
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
struct GitObjectCapture {
    root: PathBuf,
    entries: Arc<StdMutex<Option<BTreeMap<PathBuf, WorkspaceEntrySnapshot>>>>,
    modified_times: Arc<StdMutex<Option<BTreeMap<PathBuf, SystemTime>>>>,
    entry_identities: Arc<StdMutex<Option<BTreeMap<PathBuf, FilesystemIdentity>>>>,
}

#[derive(Clone)]
struct SharedFamilyExecutor {
    inner: Arc<Mutex<FamilyExecutor>>,
    git_execution_windows: Arc<StdMutex<BTreeMap<String, GitExecutionTimeWindow>>>,
    filesystem_execution_windows: Arc<StdMutex<BTreeMap<String, FilesystemExecutionTimeWindow>>>,
    git_object_capture: Option<GitObjectCapture>,
}

impl SharedFamilyExecutor {
    fn new(inner: FamilyExecutor) -> Self {
        Self {
            inner: Arc::new(Mutex::new(inner)),
            git_execution_windows: Arc::new(StdMutex::new(BTreeMap::new())),
            filesystem_execution_windows: Arc::new(StdMutex::new(BTreeMap::new())),
            git_object_capture: None,
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

    fn with_git_capture(
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

    fn capture_git_objects_before_commit(&self, name: &str) -> io::Result<()> {
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

    fn git_execution_window(&self, name: &str) -> Option<GitExecutionTimeWindow> {
        self.git_execution_windows
            .lock()
            .expect("Git execution-window lock is available")
            .get(name)
            .copied()
    }

    fn record_git_execution_window(&self, name: &str, window: GitExecutionTimeWindow) {
        self.git_execution_windows
            .lock()
            .expect("Git execution-window lock is available")
            .insert(name.to_owned(), window);
    }

    fn filesystem_execution_window(&self, name: &str) -> Option<FilesystemExecutionTimeWindow> {
        self.filesystem_execution_windows
            .lock()
            .expect("filesystem execution-window lock is available")
            .get(name)
            .copied()
    }

    fn record_filesystem_execution_window(
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
                | MessagePart::RedactedThinking { .. }
                | MessagePart::ProviderCompaction { .. } => None,
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
            .is_some_and(|text| text.contains(expected) && !report_denies_success(text, false))
    }

    fn final_response_reports_completion(&self) -> bool {
        self.final_response_reports_completion_with_required_file_effect(RequiredFileEffect::None)
    }

    fn final_response_reports_completion_with_file_creation(&self) -> bool {
        self.final_response_reports_completion_with_required_file_effect(RequiredFileEffect::Create)
    }

    fn final_response_reports_completion_with_file_mutation(&self) -> bool {
        self.final_response_reports_completion_with_required_file_effect(RequiredFileEffect::Mutate)
    }

    fn final_response_reports_completion_with_required_file_effect(
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

    fn final_response_reports_file_creation(&self) -> bool {
        self.final_response_reports_completion_with_file_creation()
            && self
                .state
                .lock()
                .expect("operation-tracker lock is available")
                .final_response_text
                .as_deref()
                .is_some_and(|report| !report_denies_file_changes(report))
    }

    fn final_response_reports_file_creation_excepting_path(&self, path: &Path) -> bool {
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

    fn final_response_reports_case_outcome(&self, case_name: &str) -> bool {
        self.state
            .lock()
            .expect("operation-tracker lock is available")
            .final_response_text
            .as_deref()
            .is_some_and(|report| report_affirms_case_outcome(report, case_name))
    }

    fn final_response_denies_exec_output(&self) -> bool {
        self.state
            .lock()
            .expect("operation-tracker lock is available")
            .final_response_text
            .as_deref()
            .is_some_and(report_denies_exec_output)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequiredFileEffect {
    None,
    Create,
    Mutate,
}

fn report_affirms_completion(report: &str, file_creation_required: bool) -> bool {
    report_affirms_completion_with_exception(report, file_creation_required, None)
}

fn report_affirms_completion_excepting_path(report: &str, path: &Path) -> bool {
    report_affirms_completion_with_exception(report, true, Some(path))
}

fn report_affirms_completion_with_exception(
    report: &str,
    file_creation_required: bool,
    excepted_path: Option<&Path>,
) -> bool {
    let words = normalized_report_words(report);
    let has_completion = [
        "applied",
        "committed",
        "completed",
        "created",
        "done",
        "executed",
        "fetched",
        "finished",
        "generated",
        "listed",
        "matched",
        "ran",
        "read",
        "run",
        "saved",
        "searched",
        "staged",
        "succeeded",
        "switched",
        "updated",
        "worked",
        "written",
        "wrote",
    ]
    .iter()
    .any(|word| words.iter().any(|observed| observed == *word));
    has_completion
        && !report_words_deny_success(report, &words, file_creation_required, excepted_path)
}

fn report_affirms_case_outcome(report: &str, case_name: &str) -> bool {
    let words = normalized_report_words(report);
    let generic_completion = words
        .iter()
        .any(|word| matches!(word.as_str(), "completed" | "done" | "finished"));
    generic_completion
        || case_outcome_verbs(case_name)
            .iter()
            .any(|outcome| words.iter().any(|word| word == outcome))
}

fn case_outcome_verbs(case_name: &str) -> &'static [&'static str] {
    match case_name {
        GIT_BRANCH_CREATE_NAME => &["created"][..],
        GIT_BRANCH_SWITCH_NAME => &["switched"][..],
        GIT_CREATE_COMMIT_NAME => &["commit", "committed"][..],
        GIT_DIFF_NAME => &["diffed"][..],
        GIT_LOG_NAME => &["listed", "read"][..],
        GIT_STAGE_NAME => &["staged"][..],
        GIT_STATUS_NAME => &["listed", "read"][..],
        APPLY_PATCH_NAME => &["applied", "created", "updated", "written"][..],
        EDIT_FILE_NAME => &["edited", "saved", "updated", "written"][..],
        GLOB_FILES_NAME => &["listed", "matched"][..],
        LIST_DIRECTORY_NAME => &["listed"][..],
        READ_FILE_NAME => &["read"][..],
        SEARCH_FILES_NAME => &["matched", "searched"][..],
        WRITE_FILE_NAME => &["created", "saved", "written", "wrote"][..],
        WEB_FETCH_NAME => &["fetched", "read"][..],
        WEB_SEARCH_NAME => &["searched"][..],
        SANDBOXED_EXEC_NAME | UNSANDBOXED_EXEC_NAME => {
            &["executed", "ran", "run", "succeeded", "worked"][..]
        }
        CARGO_DIAGNOSTICS_NAME => &["checked", "ran", "succeeded"][..],
        _ => &[][..],
    }
}

fn is_case_outcome_verb(word: &str) -> bool {
    GIT_CASES
        .iter()
        .chain(WORKSPACE_CASES)
        .chain(WEB_CASES)
        .chain(EXEC_CASES)
        .flat_map(|case| case_outcome_verbs(case.name))
        .any(|outcome| *outcome == word)
}

fn report_denies_success(report: &str, file_creation_required: bool) -> bool {
    let words = normalized_report_words(report);
    report_words_deny_success(report, &words, file_creation_required, None)
}

fn report_denies_file_changes(report: &str) -> bool {
    report_denies_file_changes_with_exception(report, None)
}

fn report_denies_file_changes_excepting_path(report: &str, path: &Path) -> bool {
    report_denies_file_changes_with_exception(report, Some(path))
}

fn report_denies_file_changes_with_exception(report: &str, excepted_path: Option<&Path>) -> bool {
    let words = normalized_report_words(report);
    let no_changes = report
        .split([';', '.', ',', '!', '?', '\n'])
        .map(normalized_report_words)
        .any(|clause| {
            clause.iter().enumerate().any(|(index, word)| {
                let change = clause
                    .get(index + 1)
                    .is_some_and(|word| matches!(word.as_str(), "change" | "changes"));
                let scope_start = clause.len().min(index + 2);
                let scope = &clause[scope_start..clause.len().min(index + 10)];
                let collateral = scope
                    .iter()
                    .position(|word| matches!(word.as_str(), "file" | "files"))
                    .is_some_and(|file| {
                        scope[..file]
                            .iter()
                            .any(|word| is_collateral_file_qualifier(word))
                    });
                matches!(word.as_str(), "no" | "zero") && change && !collateral
            })
        });
    let no_file_change = normalized_report_clauses(report).into_iter().any(|clause| {
        clause.iter().enumerate().any(|(index, word)| {
            let scope_start = clause.len().min(index + 2);
            let scope = &clause[scope_start..clause.len().min(index + 10)];
            let denied_outcome = scope.iter().position(|word| {
                matches!(
                    word.as_str(),
                    "change"
                        | "changed"
                        | "changes"
                        | "created"
                        | "edited"
                        | "modified"
                        | "written"
                )
            });
            let collateral = denied_outcome.is_some_and(|outcome| {
                scope[outcome + 1..]
                    .iter()
                    .any(|word| is_collateral_file_qualifier(word))
            });
            let requested_path_excepted = denied_outcome.is_some()
                && excepted_path.is_some_and(|path| words_except_named_path(scope, path));
            word == "no"
                && clause
                    .get(index + 1)
                    .is_some_and(|object| matches!(object.as_str(), "file" | "files"))
                && denied_outcome.is_some()
                && !collateral
                && !requested_path_excepted
        })
    });
    let no_modifications_made = report
        .split([';', '.', ',', '!', '?', '\n'])
        .map(normalized_report_words)
        .any(|clause| clause_denies_modifications_made(&clause));
    let no_existential_modifications =
        normalized_report_clauses(report).into_iter().any(|clause| {
            clause.windows(4).enumerate().any(|(index, claim)| {
                let scope = &clause[index + 4..clause.len().min(index + 12)];
                let collateral = scope
                    .iter()
                    .position(|word| matches!(word.as_str(), "file" | "files"))
                    .is_some_and(|file| {
                        scope[..file]
                            .iter()
                            .any(|word| is_collateral_file_qualifier(word))
                    });
                claim[0] == "there"
                    && matches!(claim[1].as_str(), "was" | "were")
                    && claim[2] == "no"
                    && matches!(claim[3].as_str(), "modification" | "modifications")
                    && !collateral
            })
        });
    let verb_first_modification_denial = words.iter().enumerate().any(|(index, word)| {
        let scope = &words[index + 1..words.len().min(index + 6)];
        let modification = scope
            .iter()
            .position(|word| matches!(word.as_str(), "modify" | "modified"));
        let file = scope
            .iter()
            .position(|word| matches!(word.as_str(), "file" | "files"));
        word == "not"
            && modification.is_some_and(|modification| {
                file.is_some_and(|file| {
                    modification < file
                        && !scope[modification + 1..file]
                            .iter()
                            .any(|word| is_collateral_file_qualifier(word))
                })
            })
    });
    let verb_first_change_denial = normalized_report_clauses(report).into_iter().any(|clause| {
        clause.iter().enumerate().any(|(index, word)| {
            let scope = &clause[index + 1..clause.len().min(index + 11)];
            let action = scope
                .iter()
                .position(|word| matches!(word.as_str(), "make" | "made"));
            let change = scope
                .iter()
                .position(|word| matches!(word.as_str(), "change" | "changes"));
            word == "not"
                && action.is_some_and(|action| {
                    change.is_some_and(|change| {
                        action < change && {
                            let after_change = &scope[change + 1..];
                            let collateral_before_change = scope[action + 1..change]
                                .iter()
                                .any(|word| is_collateral_file_qualifier(word));
                            let collateral_after_change = after_change
                                .iter()
                                .position(|word| matches!(word.as_str(), "file" | "files"))
                                .is_some_and(|file| {
                                    after_change[..file]
                                        .iter()
                                        .any(|word| is_collateral_file_qualifier(word))
                                });
                            !collateral_before_change
                                && !collateral_after_change
                                && !scope_is_confinement_assurance(scope)
                        }
                    })
                })
        })
    });
    let nominalized_modification_denial = words.iter().enumerate().any(|(index, word)| {
        let scope = &words[index + 1..words.len().min(index + 8)];
        let action = scope
            .iter()
            .position(|word| matches!(word.as_str(), "make" | "made"));
        let modification = scope
            .iter()
            .position(|word| matches!(word.as_str(), "modification" | "modifications"));
        word == "not"
            && action.is_some_and(|action| {
                modification.is_some_and(|modification| {
                    action < modification && {
                        let collateral_before_modification = scope[action + 1..modification]
                            .iter()
                            .any(|word| is_collateral_file_qualifier(word));
                        let collateral_after_modification = scope[modification + 1..]
                            .iter()
                            .position(|word| matches!(word.as_str(), "file" | "files"))
                            .is_some_and(|file| {
                                scope[modification + 1..modification + 1 + file]
                                    .iter()
                                    .any(|word| is_collateral_file_qualifier(word))
                            });
                        !collateral_before_modification && !collateral_after_modification
                    }
                })
            })
    });
    let inverted_modification_denial = words.iter().enumerate().any(|(index, word)| {
        let scope = &words[index + 1..words.len().min(index + 8)];
        let modification_denied = scope.first().is_some_and(|word| word == "no")
            && scope
                .get(1)
                .is_some_and(|word| matches!(word.as_str(), "modification" | "modifications"));
        let collateral = scope
            .iter()
            .position(|word| matches!(word.as_str(), "file" | "files"))
            .is_some_and(|file| {
                scope[..file]
                    .iter()
                    .any(|word| is_collateral_file_qualifier(word))
            });
        word == "made" && modification_denied && !collateral
    });
    let without_modifying = report
        .split([';', '.', ',', '!', '?', '\n'])
        .map(normalized_report_words)
        .any(|clause| {
            clause.iter().enumerate().any(|(index, word)| {
                let scope = &clause[index + 1..clause.len().min(index + 9)];
                let modification = scope
                    .iter()
                    .position(|word| matches!(word.as_str(), "modify" | "modified" | "modifying"));
                let file = scope
                    .iter()
                    .position(|word| matches!(word.as_str(), "file" | "files"));
                let collateral = file.is_some_and(|file| {
                    scope[..file]
                        .iter()
                        .any(|word| is_collateral_file_qualifier(word))
                });
                word == "without" && modification.is_some() && file.is_some() && !collateral
            })
        });
    let nothing_changed = words.iter().enumerate().any(|(index, word)| {
        word == "nothing"
            && !words.get(index + 1).is_some_and(|word| word == "else")
            && words
                .iter()
                .skip(index + 1)
                .take(3)
                .any(|word| matches!(word.as_str(), "change" | "changed" | "modified"))
    });
    let unchanged_file = report
        .split([';', '.', ',', '!', '?', '\n'])
        .map(normalized_report_words)
        .any(|clause| {
            clause.iter().enumerate().any(|(index, word)| {
                let scope = &clause[index.saturating_sub(6)..index];
                let file = scope
                    .iter()
                    .rposition(|word| matches!(word.as_str(), "file" | "files"));
                word == "unchanged"
                    && file.is_some_and(|file| {
                        !scope[..file]
                            .iter()
                            .rev()
                            .take(3)
                            .any(|word| is_collateral_file_qualifier(word))
                    })
            })
        });
    report_denies_file_creation(report, excepted_path)
        || no_changes
        || no_file_change
        || no_modifications_made
        || no_existential_modifications
        || verb_first_modification_denial
        || verb_first_change_denial
        || nominalized_modification_denial
        || inverted_modification_denial
        || without_modifying
        || nothing_changed
        || unchanged_file
}

fn words_except_named_path(words: &[String], path: &Path) -> bool {
    let path_words = normalized_report_words(&path.to_string_lossy());
    words.iter().enumerate().any(|(index, word)| {
        matches!(word.as_str(), "besides" | "except")
            && words[index + 1..]
                .windows(path_words.len())
                .any(|candidate| candidate == path_words)
    })
}

fn is_collateral_file_qualifier(word: &str) -> bool {
    matches!(
        word,
        "additional" | "backup" | "existing" | "other" | "preexisting"
    )
}

fn report_denies_file_creation(report: &str, excepted_path: Option<&Path>) -> bool {
    normalized_report_clauses(report).into_iter().any(|clause| {
        let denied_existence = clause.iter().enumerate().any(|(index, word)| {
            let collateral = clause[index.saturating_sub(6)..index]
                .iter()
                .any(|word| is_collateral_file_qualifier(word));
            let prior_state = clause[index + 1..clause.len().min(index + 5)]
                .iter()
                .any(|word| matches!(word.as_str(), "before" | "previously"));
            word == "not"
                && clause[index + 1..clause.len().min(index + 3)]
                    .iter()
                    .any(|word| matches!(word.as_str(), "exist" | "exists"))
                && !collateral
                && !prior_state
        });
        denied_existence
            || clause.iter().enumerate().any(|(index, word)| {
                let scope = &clause[index + 1..clause.len().min(index + 11)];
                let file = scope
                    .iter()
                    .position(|word| matches!(word.as_str(), "file" | "files"));
                let creation = scope.iter().position(|word| {
                    matches!(
                        word.as_str(),
                        "create"
                            | "created"
                            | "creating"
                            | "exists"
                            | "found"
                            | "generate"
                            | "generated"
                            | "generating"
                            | "write"
                            | "writing"
                            | "written"
                            | "wrote"
                    )
                });
                let collateral = scope.iter().any(|word| is_collateral_file_qualifier(word));
                let requested_path_excepted = excepted_path
                    .is_some_and(|path| words_except_named_path(&clause[index..], path));
                let no_before_file = matches!(word.as_str(), "no" | "zero")
                    && file.is_some()
                    && creation.is_some_and(|creation| file.is_some_and(|file| file < creation));
                let outcome_before_no = matches!(
                    word.as_str(),
                    "create"
                        | "created"
                        | "creating"
                        | "generate"
                        | "generated"
                        | "generating"
                        | "write"
                        | "writing"
                        | "written"
                        | "wrote"
                ) && scope
                    .iter()
                    .position(|word| word == "no")
                    .is_some_and(|no| file.is_some_and(|file| no < file));
                let without_creation = word == "without"
                    && creation.is_some_and(|creation| file.is_some_and(|file| creation < file));
                let file_state_denial = matches!(word.as_str(), "file" | "files")
                    && scope
                        .iter()
                        .take(4)
                        .position(|word| {
                            matches!(word.as_str(), "absent" | "deleted" | "missing" | "removed")
                        })
                        .is_some_and(|state| {
                            let historical = scope[..scope.len().min(state + 5)]
                                .iter()
                                .any(|word| matches!(word.as_str(), "before" | "previously"));
                            !historical
                                && !scope[..state]
                                    .iter()
                                    .any(|word| matches!(word.as_str(), "never" | "not"))
                        })
                    && !clause[index.saturating_sub(4)..index]
                        .iter()
                        .any(|word| is_collateral_file_qualifier(word));
                ((no_before_file || outcome_before_no || without_creation)
                    && !collateral
                    && !requested_path_excepted)
                    || file_state_denial
            })
    })
}

fn normalized_report_clauses(report: &str) -> Vec<Vec<String>> {
    normalized_report_segments(report, true)
}

fn normalized_report_segments(report: &str, split_commas: bool) -> Vec<Vec<String>> {
    // `str::split` predicates cannot inspect the characters adjacent to a
    // period, while `str::split_inclusive` would first fragment dotted paths
    // and require reassembling them. This bounded report scanner preserves
    // periods embedded between alphanumerics and frames only punctuation that
    // can terminate a clause.
    let mut separated = String::with_capacity(report.len());
    for (index, character) in report.char_indices() {
        let next = &report[index + character.len_utf8()..];
        let embedded_period = character == '.'
            && report[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
            && next.chars().next().is_some_and(char::is_alphanumeric);
        if matches!(character, ';' | '!' | '?' | '\n')
            || (character == ',' && split_commas)
            || (character == '.' && !embedded_period)
        {
            separated.push('\n');
        } else {
            separated.push(character);
        }
    }
    separated
        .lines()
        .map(normalized_report_words)
        .filter(|clause| !clause.is_empty())
        .collect()
}

fn clause_denies_modifications_made(clause: &[String]) -> bool {
    clause.iter().enumerate().any(|(index, word)| {
        let ordinary = clause.get(index + 2).is_some_and(|word| {
            matches!(word.as_str(), "was" | "were")
                && clause.get(index + 3).is_some_and(|word| word == "made")
        });
        let perfect = clause.get(index + 2).is_some_and(|word| {
            matches!(word.as_str(), "has" | "have")
                && clause.get(index + 3).is_some_and(|word| word == "been")
                && clause.get(index + 4).is_some_and(|word| word == "made")
        });
        let made_index = if ordinary { index + 3 } else { index + 4 };
        let scope = &clause[clause.len().min(made_index + 1)..];
        let collateral = scope
            .iter()
            .position(|word| matches!(word.as_str(), "file" | "files"))
            .is_some_and(|file| {
                scope[..file]
                    .iter()
                    .any(|word| is_collateral_file_qualifier(word))
            });
        word == "no"
            && clause
                .get(index + 1)
                .is_some_and(|word| matches!(word.as_str(), "modification" | "modifications"))
            && (ordinary || perfect)
            && !collateral
    })
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

fn report_words_deny_success(
    report: &str,
    words: &[String],
    file_creation_required: bool,
    excepted_path: Option<&Path>,
) -> bool {
    let explicit_failure = report
        .split([';', '.', ',', '!', '?', '\n'])
        .map(normalized_report_words)
        .any(|clause| {
            clause.iter().enumerate().any(|(index, word)| {
                let failure_free_compound =
                    matches!(word.as_str(), "error" | "errors" | "failure" | "failures")
                        && clause.get(index + 1).is_some_and(|suffix| suffix == "free");
                let failure_term_negated = failure_term_is_negated(&clause, index);
                let resolved_problem =
                    matches!(word.as_str(), "issue" | "issues" | "problem" | "problems")
                        && clause[index.saturating_sub(4)..clause.len().min(index + 5)]
                            .iter()
                            .any(|word| word == "resolved");
                [
                    "cannot",
                    "error",
                    "errors",
                    "failed",
                    "failure",
                    "incomplete",
                    "issue",
                    "issues",
                    "problem",
                    "problems",
                    "unsuccessful",
                    "unsuccessfully",
                    "unable",
                ]
                .contains(&word.as_str())
                    && !resolved_problem
                    && ((!failure_term_negated && !failure_free_compound)
                        || (failure_term_negated && failure_free_compound))
            })
        });
    let negative_no_objects = [
        "answer",
        "commit",
        "completion",
        "match",
        "result",
        "success",
        "successful",
        "successfully",
    ];
    let negative_no_claim = words
        .windows(2)
        .any(|pair| pair[0] == "no" && negative_no_objects.contains(&pair[1].as_str()));
    let negative_could_not = normalized_report_clauses(report).into_iter().any(|clause| {
        clause.windows(2).enumerate().any(|(index, pair)| {
            let scope = &clause[index + 2..clause.len().min(index + 12)];
            pair[0] == "could"
                && pair[1] == "not"
                && !scope.first().is_some_and(|word| word == "only")
                && !scope_is_confinement_assurance(scope)
        })
    });
    let negative_not_able = normalized_report_clauses(report).into_iter().any(|clause| {
        clause.windows(2).enumerate().any(|(index, pair)| {
            let scope = &clause[index + 2..clause.len().min(index + 12)];
            pair[0] == "not" && pair[1] == "able" && !scope_is_confinement_assurance(scope)
        })
    });
    let negative_without_success = words.iter().enumerate().any(|(index, word)| {
        word == "without"
            && words.iter().skip(index + 1).take(3).any(|outcome| {
                matches!(outcome.as_str(), "success" | "successful" | "successfully")
            })
    });
    let negative_no_file_claim =
        file_creation_required && report_denies_file_creation(report, excepted_path);
    let requested_path_invalid_state = excepted_path.is_some_and(|path| {
        let path_words = normalized_report_words(&path.to_string_lossy());
        let clauses = normalized_report_clauses(report);
        let destructive_state = clauses.iter().enumerate().any(|(clause_index, clause)| {
            clause
                .windows(path_words.len())
                .position(|candidate| candidate == path_words)
                .is_some_and(|path_start| {
                    let path_state = &clause[path_start + path_words.len()..];
                    path_state_is_destructive(path_state)
                        || clauses.get(clause_index + 1).is_some_and(|next_clause| {
                            clause_refers_to_requested_path(next_clause, &path_words)
                                && path_state_is_destructive(next_clause)
                        })
                })
        });
        let invalid_contents = requested_path_contents_are_invalid(report, &path_words);
        destructive_state || invalid_contents
    });
    let negative_nothing_claim = words.iter().enumerate().any(|(index, word)| {
        let read_only_change_denial = words
            .iter()
            .skip(index + 1)
            .take(3)
            .any(|word| matches!(word.as_str(), "change" | "changed" | "modified"));
        word == "nothing"
            && !words.get(index + 1).is_some_and(|qualifier| {
                matches!(qualifier.as_str(), "else" | "failed" | "failure" | "other")
            })
            && !read_only_change_denial
    });
    let deferred_completion = report_has_deferred_outcome(report);
    let hedged_completion = report_hedges_outcome(report);
    let attempted_completion = report_only_attempts_outcome(report);
    let requested_completion = report_requests_outcome(report);
    let stopped_skipped_or_aborted_completion = report_stops_skips_or_aborts_outcome(report);
    let timed_out_completion = report_times_out_outcome(report);
    let canceled_completion = report_cancels_outcome(report);
    let partial_completion = report_partially_completes_outcome(report);
    let clauses_with_dotted_paths_preserved = normalized_report_clauses(report);
    let affirmative_pending = clauses_with_dotted_paths_preserved.iter().any(|clause| {
        clause.iter().enumerate().any(|(index, word)| {
            word == "pending"
                && clause[..index].last().is_some_and(|predicate| {
                    matches!(
                        predicate.as_str(),
                        "is" | "left" | "remain" | "remains" | "stays" | "still"
                    )
                })
                && !failure_term_is_negated(clause, index)
        })
    });
    let scoped_negation = clauses_with_dotted_paths_preserved
        .into_iter()
        .any(|clause| {
            clause.iter().enumerate().any(|(index, word)| {
                let scope = &clause[index + 1..clause.len().min(index + 7)];
                let outcome = scope.iter().position(|word| is_negative_outcome(word));
                let confinement_assurance = scope_is_confinement_assurance(scope);
                let affirmative_not_only =
                    word == "not" && scope.first().is_some_and(|qualifier| qualifier == "only");
                let negated_need = matches!(word.as_str(), "never" | "no" | "not")
                    && scope.first().is_some_and(|predicate| predicate == "need")
                    && scope.get(1).is_some_and(|connector| connector == "to");
                let no_is_collateral = word == "no"
                    && outcome.is_some_and(|outcome| {
                        scope[..outcome].iter().any(|word| {
                            matches!(
                                word.as_str(),
                                "additional"
                                    | "backup"
                                    | "error"
                                    | "errors"
                                    | "failure"
                                    | "failures"
                                    | "issue"
                                    | "issues"
                                    | "existing"
                                    | "other"
                                    | "preexisting"
                                    | "problem"
                                    | "problems"
                            ) || (!file_creation_required
                                && matches!(word.as_str(), "file" | "files"))
                        })
                    });
                let collateral_only = outcome.is_some_and(|outcome| {
                    let predicate_tail = &scope[outcome + 1..];
                    predicate_tail
                        .iter()
                        .position(|word| is_collateral_file_qualifier(word))
                        .is_some_and(|qualifier| {
                            predicate_tail[qualifier + 1..]
                                .iter()
                                .any(|word| matches!(word.as_str(), "file" | "files"))
                                && predicate_tail[..qualifier].iter().all(|word| {
                                    matches!(
                                        word.as_str(),
                                        "also"
                                            | "and"
                                            | "any"
                                            | "change"
                                            | "changed"
                                            | "create"
                                            | "created"
                                            | "generate"
                                            | "generated"
                                            | "modify"
                                            | "modified"
                                            | "on"
                                            | "or"
                                            | "the"
                                            | "write"
                                            | "written"
                                            | "wrote"
                                    )
                                })
                        })
                });
                let read_only_file_denial = !file_creation_required
                    && outcome.is_some_and(|outcome| {
                        let predicate_tail = &scope[outcome + 1..];
                        matches!(
                            scope[outcome].as_str(),
                            "change" | "changed" | "modify" | "modified"
                        ) && predicate_tail
                            .iter()
                            .position(|word| matches!(word.as_str(), "file" | "files"))
                            .is_some_and(|file| {
                                predicate_tail[..file]
                                    .iter()
                                    .all(|word| matches!(word.as_str(), "any" | "the"))
                            })
                    });
                let predicate_scope = &clause[index..];
                let predicate_scope = &predicate_scope[..predicate_scope
                    .iter()
                    .position(|word| {
                        matches!(
                            word.as_str(),
                            "and" | "because" | "but" | "however" | "since" | "so" | "then" | "yet"
                        )
                    })
                    .unwrap_or(predicate_scope.len())];
                let requested_path_excepted = outcome.is_some()
                    && excepted_path
                        .is_some_and(|path| words_except_named_path(predicate_scope, path));
                matches!(word.as_str(), "never" | "no" | "not" | "without")
                    && outcome.is_some()
                    && !affirmative_not_only
                    && !negated_need
                    && !no_is_collateral
                    && !collateral_only
                    && !read_only_file_denial
                    && !requested_path_excepted
                    && !confinement_assurance
            })
        });
    explicit_failure
        || negative_no_claim
        || negative_could_not
        || negative_not_able
        || negative_without_success
        || negative_no_file_claim
        || requested_path_invalid_state
        || negative_nothing_claim
        || deferred_completion
        || hedged_completion
        || attempted_completion
        || requested_completion
        || stopped_skipped_or_aborted_completion
        || timed_out_completion
        || canceled_completion
        || partial_completion
        || affirmative_pending
        || scoped_negation
}

fn path_state_is_destructive(path_state: &[String]) -> bool {
    path_state
        .iter()
        .take(4)
        .position(|word| matches!(word.as_str(), "deleted" | "removed"))
        .is_some_and(|state| {
            !path_state[..state]
                .iter()
                .any(|word| matches!(word.as_str(), "never" | "not"))
        })
}

fn report_denies_exec_output(report: &str) -> bool {
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

fn report_has_deferred_outcome(report: &str) -> bool {
    normalized_report_clauses(report).into_iter().any(|clause| {
        let need_or_yet = clause.windows(3).enumerate().any(|(index, claim)| {
            let need = claim[0] == "need"
                && !failure_term_is_negated(&clause, index)
                && claim[1] == "to"
                && is_negative_outcome(&claim[2]);
            let yet = claim[0] == "yet" && claim[1] == "to" && is_negative_outcome(&claim[2]);
            need || yet
        });
        let passive = clause.windows(4).enumerate().any(|(index, claim)| {
            let need = claim[0] == "need"
                && !failure_term_is_negated(&clause, index)
                && claim[1] == "to"
                && claim[2] == "be"
                && is_negative_outcome(&claim[3]);
            let yet = claim[0] == "yet"
                && claim[1] == "to"
                && claim[2] == "be"
                && is_negative_outcome(&claim[3]);
            let remains = matches!(claim[0].as_str(), "remain" | "remains")
                && claim[1] == "to"
                && claim[2] == "be"
                && is_negative_outcome(&claim[3]);
            need || yet || remains
        });
        let future = clause
            .windows(3)
            .any(|claim| claim[0] == "will" && claim[1] == "be" && is_negative_outcome(&claim[2]))
            || clause
                .windows(2)
                .any(|claim| claim[0] == "will" && is_negative_outcome(&claim[1]));
        need_or_yet || passive || future
    })
}

fn report_hedges_outcome(report: &str) -> bool {
    normalized_report_clauses(report).into_iter().any(|clause| {
        clause.iter().enumerate().any(|(index, word)| {
            let scope = &clause[index + 1..clause.len().min(index + 8)];
            let uncertain = matches!(word.as_str(), "may" | "might" | "perhaps" | "possibly");
            let collateral_file_assurance = scope
                .iter()
                .position(|word| matches!(word.as_str(), "file" | "files"))
                .is_some_and(|file| {
                    scope[..file]
                        .iter()
                        .any(|word| is_collateral_file_qualifier(word))
                });
            uncertain
                && scope.iter().any(|word| is_negative_outcome(word))
                && !collateral_file_assurance
                && !scope_is_confinement_assurance(scope)
        })
    })
}

fn report_only_attempts_outcome(report: &str) -> bool {
    normalized_report_clauses(report).into_iter().any(|clause| {
        clause.iter().enumerate().any(|(index, word)| {
            let scope = &clause[index + 1..];
            let boundary = scope
                .iter()
                .position(|word| matches!(word.as_str(), "and" | "but" | "then"));
            let attempted_scope = &scope[..boundary.unwrap_or(scope.len())];
            let coordinated_scope = boundary.map_or(&[][..], |boundary| &scope[boundary + 1..]);
            let infinitive_outcome = attempted_scope
                .windows(2)
                .any(|claim| claim[0] == "to" && is_negative_outcome(&claim[1]));
            let gerund_outcome = attempted_scope
                .first()
                .is_some_and(|word| is_negative_outcome(word));
            is_attempt_predicate(word)
                && (infinitive_outcome || gerund_outcome)
                && !coordinated_scope_affirms_outcome(coordinated_scope)
        })
    })
}

fn report_requests_outcome(report: &str) -> bool {
    let clauses = normalized_report_clauses(report);
    clauses.iter().enumerate().any(|(request_index, clause)| {
        let polite_request = clause.iter().enumerate().any(|(index, word)| {
            word == "please"
                && clause[index + 1..]
                    .iter()
                    .take(5)
                    .any(|word| is_negative_outcome(word))
        });
        let imperative_run = clause.first().is_some_and(|word| word == "run")
            && clause.get(1).is_some_and(|word| {
                matches!(word.as_str(), "command" | "it" | "that" | "the" | "this")
            });
        let independent_completion = clauses.iter().enumerate().any(|(index, clause)| {
            index != request_index && coordinated_scope_affirms_outcome(clause)
        });
        (polite_request || imperative_run) && !independent_completion
    })
}

fn report_stops_skips_or_aborts_outcome(report: &str) -> bool {
    normalized_report_segments(report, false)
        .into_iter()
        .any(|clause| {
            clause.iter().enumerate().any(|(index, word)| {
                let scope = &clause[index + 1..];
                let boundary = scope
                    .iter()
                    .position(|word| matches!(word.as_str(), "and" | "but" | "then"));
                let coordinated_scope = boundary.map_or(&[][..], |boundary| &scope[boundary + 1..]);
                let nearby = &clause[index.saturating_sub(4)..clause.len().min(index + 5)];
                matches!(
                    word.as_str(),
                    "abort"
                        | "aborted"
                        | "aborting"
                        | "block"
                        | "blocked"
                        | "blocking"
                        | "interrupt"
                        | "interrupted"
                        | "interrupting"
                        | "skip"
                        | "skipped"
                        | "skipping"
                        | "stop"
                        | "stopped"
                        | "stopping"
                        | "terminate"
                        | "terminated"
                        | "terminating"
                        | "kill"
                        | "killed"
                        | "killing"
                        | "prevent"
                        | "prevented"
                        | "preventing"
                ) && !failure_term_is_negated(&clause, index)
                    && nearby.iter().any(|word| is_negative_outcome(word))
                    && !coordinated_scope_affirms_outcome(coordinated_scope)
            })
        })
}

fn report_times_out_outcome(report: &str) -> bool {
    normalized_report_segments(report, false)
        .into_iter()
        .any(|clause| {
            clause.iter().enumerate().any(|(index, word)| {
                let following = &clause[index + 1..];
                let boundary = following
                    .iter()
                    .position(|word| matches!(word.as_str(), "and" | "but" | "then"));
                let coordinated_scope =
                    boundary.map_or(&[][..], |boundary| &following[boundary + 1..]);
                let nearby = &clause[index.saturating_sub(4)..clause.len().min(index + 5)];
                let predicate_prefix = &clause[index.saturating_sub(4)..index];
                let bare_timeout_failure = matches!(word.as_str(), "timeout" | "timeouts")
                    && predicate_prefix.iter().any(|word| {
                        matches!(
                            word.as_str(),
                            "encounter"
                                | "encountered"
                                | "exceed"
                                | "exceeded"
                                | "hit"
                                | "hits"
                                | "reach"
                                | "reached"
                        )
                    });
                let timeout_predicate = bare_timeout_failure
                    || (matches!(word.as_str(), "time" | "timed" | "timing")
                        && following.first().is_some_and(|word| word == "out"));
                timeout_predicate
                    && !failure_term_is_negated(&clause, index)
                    && (bare_timeout_failure || nearby.iter().any(|word| is_negative_outcome(word)))
                    && !coordinated_scope_affirms_outcome(coordinated_scope)
            })
        })
}

fn report_partially_completes_outcome(report: &str) -> bool {
    normalized_report_clauses(report).into_iter().any(|clause| {
        clause.iter().enumerate().any(|(index, word)| {
            let nearby = &clause[index.saturating_sub(3)..clause.len().min(index + 4)];
            matches!(word.as_str(), "partial" | "partially")
                && nearby.iter().any(|word| is_negative_outcome(word))
        })
    })
}

fn report_cancels_outcome(report: &str) -> bool {
    normalized_report_segments(report, false)
        .into_iter()
        .any(|clause| {
            clause.iter().enumerate().any(|(index, word)| {
                let following = &clause[index + 1..];
                let boundary = following
                    .iter()
                    .position(|word| matches!(word.as_str(), "and" | "but" | "then"));
                let coordinated_scope =
                    boundary.map_or(&[][..], |boundary| &following[boundary + 1..]);
                let nearby = &clause[index.saturating_sub(4)..clause.len().min(index + 5)];
                matches!(word.as_str(), "cancel" | "canceled" | "cancelled")
                    && !failure_term_is_negated(&clause, index)
                    && nearby.iter().any(|word| is_negative_outcome(word))
                    && !coordinated_scope_affirms_outcome(coordinated_scope)
            })
        })
}

fn coordinated_scope_affirms_outcome(scope: &[String]) -> bool {
    scope.iter().enumerate().any(|(index, word)| {
        is_negative_outcome(word)
            && !failure_term_is_negated(scope, index)
            && !scope[..index].iter().any(|word| is_attempt_predicate(word))
    })
}

fn is_attempt_predicate(word: &str) -> bool {
    matches!(
        word,
        "attempt"
            | "attempted"
            | "attempting"
            | "prepare"
            | "prepared"
            | "preparing"
            | "tried"
            | "try"
            | "trying"
    )
}

fn scope_is_confinement_assurance(scope: &[String]) -> bool {
    scope.iter().any(|word| word == "outside") && scope.iter().any(|word| word == "workspace")
}

fn requested_path_contents_are_invalid(report: &str, path_words: &[String]) -> bool {
    let clauses = normalized_report_clauses(report);
    clauses.iter().enumerate().any(|(clause_index, clause)| {
        clause
            .windows(path_words.len())
            .enumerate()
            .any(|(path_start, candidate)| {
                if candidate != path_words {
                    return false;
                }
                let path_state = &clause[path_start + path_words.len()..];
                path_state_denies_required_contents(path_state)
                    || clauses.get(clause_index + 1).is_some_and(|next_clause| {
                        clause_refers_to_requested_path(next_clause, path_words)
                            && path_state_denies_required_contents(next_clause)
                    })
            })
    })
}

fn clause_refers_to_requested_path(clause: &[String], path_words: &[String]) -> bool {
    let subject = clause
        .iter()
        .skip_while(|word| matches!(word.as_str(), "and" | "but" | "however" | "then" | "yet"))
        .collect::<Vec<_>>();
    subject
        .windows(path_words.len())
        .next()
        .is_some_and(|candidate| {
            candidate
                .iter()
                .zip(path_words)
                .all(|(observed, expected)| observed.as_str() == expected)
        })
        || subject
            .first()
            .is_some_and(|word| matches!(word.as_str(), "it" | "its"))
        || matches!(
            subject.as_slice(),
            [article, file, ..]
                if article.as_str() == "the" && file.as_str() == "file"
        )
        || matches!(
            subject.as_slice(),
            [article, requested, file, ..]
                if article.as_str() == "the"
                    && requested.as_str() == "requested"
                    && file.as_str() == "file"
        )
        || matches!(
            subject.as_slice(),
            [requested, file, ..]
                if requested.as_str() == "requested" && file.as_str() == "file"
        )
}

fn path_state_denies_required_contents(path_state: &[String]) -> bool {
    path_state.iter().take(10).enumerate().any(|(index, word)| {
        let predicate_prefix = &path_state[index.saturating_sub(4)..index];
        let predicate_suffix = &path_state[index + 1..path_state.len().min(index + 5)];
        let negated = predicate_prefix
            .iter()
            .any(|word| matches!(word.as_str(), "never" | "not"));
        let historical = predicate_prefix
            .iter()
            .chain(predicate_suffix)
            .any(|word| matches!(word.as_str(), "before" | "initially" | "previously"))
            || predicate_prefix
                .windows(2)
                .any(|words| words[0] == "at" && words[1] == "first");
        let collateral = predicate_prefix
            .iter()
            .any(|word| is_collateral_file_qualifier(word));
        let invalid_content = matches!(
            word.as_str(),
            "empty" | "incorrect" | "mismatch" | "mismatched" | "wrong"
        );
        let zero_bytes = matches!(word.as_str(), "0" | "zero")
            && predicate_suffix
                .iter()
                .take(2)
                .any(|word| matches!(word.as_str(), "byte" | "bytes"));
        (invalid_content || zero_bytes) && !negated && !historical && !collateral
    })
}

fn is_negative_outcome(word: &str) -> bool {
    is_case_outcome_verb(word)
        || matches!(
            word,
            "change"
                | "changed"
                | "complete"
                | "completed"
                | "create"
                | "diff"
                | "done"
                | "edit"
                | "execute"
                | "executed"
                | "executing"
                | "fetch"
                | "find"
                | "finish"
                | "finished"
                | "found"
                | "generate"
                | "generated"
                | "generating"
                | "list"
                | "match"
                | "modify"
                | "modified"
                | "perform"
                | "performed"
                | "ran"
                | "run"
                | "running"
                | "search"
                | "stage"
                | "success"
                | "successful"
                | "successfully"
                | "succeed"
                | "succeeded"
                | "switch"
                | "work"
                | "worked"
                | "write"
        )
}

fn failure_term_is_negated(clause: &[String], failure_index: usize) -> bool {
    let qualifier_scope = &clause[..failure_index];
    qualifier_scope
        .iter()
        .rposition(|word| {
            matches!(
                word.as_str(),
                "never" | "no" | "not" | "nothing" | "without" | "zero"
            )
        })
        .is_some_and(|negation| {
            let without_is_reversed = qualifier_scope[negation] == "without"
                && qualifier_scope[..negation]
                    .iter()
                    .rev()
                    .take(2)
                    .any(|word| matches!(word.as_str(), "never" | "not"));
            failure_index - negation <= 5
                && !without_is_reversed
                && !qualifier_scope[negation + 1..].iter().any(|word| {
                    matches!(
                        word.as_str(),
                        "and" | "because" | "but" | "however" | "since" | "so" | "then" | "yet"
                    )
                })
        })
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

#[test]
fn eval_receipt_injection_rejects_a_preexisting_receipt() {
    let evidence = ToolExecutorEvidence::CompletedText(
        serde_json::json!({EVAL_RECEIPT_FIELD: "preexisting synthetic receipt"}).to_string(),
    );

    assert!(add_eval_receipt(evidence, SYNTHETIC_EVAL_RECEIPT).is_err());
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

    assert!(!forced_case_completion_reported(
        READ_FILE_NAME,
        true,
        &tracker
    ));
}

#[test]
fn forced_commit_completion_rejects_a_read_only_report() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_READ_RESULT_REPORT, false);

    assert!(!forced_case_completion_reported(
        GIT_CREATE_COMMIT_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_diff_completion_rejects_a_negated_diff_report() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NEGATED_DIFF_REPORT, false);

    assert!(!forced_case_completion_reported(
        GIT_DIFF_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_edit_completion_rejects_a_negated_edit_report() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NEGATED_EDIT_REPORT, false);

    assert!(!forced_case_completion_reported(
        EDIT_FILE_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_read_completion_accepts_a_read_report() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_READ_RESULT_REPORT, false);

    assert!(forced_case_completion_reported(
        READ_FILE_NAME,
        true,
        &tracker,
    ));
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
fn final_response_report_rejects_a_failure_after_a_separate_negated_clause() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_CROSS_CLAUSE_FAILURE_REPORT, false);

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
fn final_response_report_rejects_an_outcome_remaining_to_be_completed() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REMAINING_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn forced_read_completion_rejects_a_still_needed_report() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_STILL_NEEDS_READ_REPORT, false);

    assert!(!forced_case_completion_reported(
        READ_FILE_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_read_completion_rejects_a_needed_report() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NEEDS_READ_REPORT, false);

    assert!(!forced_case_completion_reported(
        READ_FILE_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_read_completion_accepts_a_negated_need_to_repeat_the_read() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_NEED_TO_READ_AGAIN_REPORT, false);

    assert!(forced_case_completion_reported(
        READ_FILE_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_read_completion_rejects_a_future_report() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_FUTURE_READ_REPORT, false);

    assert!(!forced_case_completion_reported(
        READ_FILE_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn final_response_report_rejects_an_affirmative_pending_state() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_PENDING_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_a_negated_pending_state() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_PENDING_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
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
fn final_response_report_rejects_a_negated_web_fetch_action() {
    let tracker = OperationTracker::default();
    let response = format!("I could not fetch {WEB_FETCH_BODY}.");
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
fn final_response_report_accepts_error_free_completion() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ERROR_FREE_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn exec_file_creation_report_accepts_failure_free_completion() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_FAILURE_FREE_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_not_failure_free() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_FAILURE_FREE_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_affirmative_execution_and_existence() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_EXECUTED_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn final_response_report_accepts_completion_with_zero_errors() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ZERO_ERRORS_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_completion_when_no_errors_were_found() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_ERRORS_FOUND_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_negated_error_free_completion() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NEGATED_ERROR_FREE_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_clause_scoped_no_operation_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_OPERATION_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_longer_negated_failure_phrases() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_LONG_NEGATED_ERRORS_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_failure_after_longer_negation() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NEGATED_ERRORS_THEN_FAILURE_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_failure_after_a_causal_boundary() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_CAUSAL_FAILURE_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_with_errors() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ERRORS_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
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
fn final_response_report_rejects_completion_that_was_not_successful() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_SUCCESSFUL_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_when_the_operation_did_not_succeed() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_DID_NOT_SUCCEED_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_when_the_operation_never_succeeded() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NEVER_SUCCEEDED_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_when_the_model_was_not_able() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_ABLE_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_a_not_able_confinement_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_ABLE_CONFINEMENT_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_when_the_operation_did_not_work() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_DID_NOT_WORK_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_when_the_operation_was_not_performed() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_PERFORMED_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_when_the_command_was_not_executed() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COMMAND_NOT_EXECUTED_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_without_running_the_command() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COMMAND_NOT_RUN_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_an_execution_issue() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_EXECUTION_ISSUE_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_an_execution_problem() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_EXECUTION_PROBLEM_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_a_resolved_problem() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_RESOLVED_PROBLEM_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_negated_execution_issues() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_EXECUTION_ISSUES_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_negated_execution_problems() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_EXECUTION_PROBLEMS_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn forced_exec_report_rejects_a_denial_of_required_output() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_EXEC_OUTPUT_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_exec_report_accepts_a_truthful_empty_stderr_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_EMPTY_STDERR_OUTPUT_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_exec_report_rejects_an_empty_stdout_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_EMPTY_STDOUT_OUTPUT_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_exec_report_rejects_an_incorrect_stdout_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_INCORRECT_STDOUT_OUTPUT_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_exec_report_accepts_a_not_empty_stdout_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_EMPTY_STDOUT_OUTPUT_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn natural_exec_report_accepts_a_truthful_no_output_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NATURAL_NO_EXEC_OUTPUT_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn natural_exec_report_keeps_requested_contents_within_their_clause() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NATURAL_EMPTY_STDERR_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn natural_exec_report_stops_requested_contents_at_a_collateral_comma_clause() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NATURAL_COMMA_EMPTY_STDERR_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn forced_cargo_completion_accepts_a_successful_check_report() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_CARGO_RAN_REPORT, false);

    assert!(forced_case_completion_reported(
        CARGO_DIAGNOSTICS_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn final_response_report_accepts_a_collateral_did_not_work_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COLLATERAL_DID_NOT_WORK_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_that_was_not_without_errors() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_WITHOUT_ERRORS_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_with_no_success() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_SUCCESS_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_without_success() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_WITHOUT_SUCCESS_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_an_unsuccessful_completion() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_UNSUCCESSFUL_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_not_successfully() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_SUCCESSFULLY_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_could_not_complete() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COULD_NOT_COMPLETE_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
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
fn exec_file_creation_report_rejects_zero_files_created() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ZERO_FILES_WERE_CREATED_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_verb_first_creation_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_VERB_FIRST_CREATION_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_an_outcome_first_creation_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_OUTCOME_FIRST_CREATION_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_dotted_filename_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_DOTTED_FILE_CREATION_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_generated_file_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_GENERATED_FILE_DENIAL_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_without_creating_any_files() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_WITHOUT_CREATING_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_verb_first_modification_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_VERB_FIRST_MODIFICATION_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_without_modifying_any_files() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_WITHOUT_MODIFYING_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_collateral_without_modifying() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COLLATERAL_WITHOUT_MODIFYING_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_an_existing_file_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_EXISTING_FILE_ASSURANCE_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_preexisting_file_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_PREEXISTING_FILE_ASSURANCE_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_qualifier_first_existing_file_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(
        SYNTHETIC_QUALIFIER_FIRST_EXISTING_FILE_ASSURANCE_REPORT,
        false,
    );

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_nothing_else_changed() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COLLATERAL_NOTHING_ELSE_CHANGED_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_the_requested_file_exception() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_EXCEPTION_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_the_requested_file_predicate_exception() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_PREDICATE_EXCEPTION_REPORT, false);

    assert!(report_affirms_completion_excepting_path(
        SYNTHETIC_REQUESTED_FILE_PREDICATE_EXCEPTION_REPORT,
        Path::new(EXEC_RESULT_PATH),
    ));
    assert!(!report_denies_file_changes_excepting_path(
        SYNTHETIC_REQUESTED_FILE_PREDICATE_EXCEPTION_REPORT,
        Path::new(EXEC_RESULT_PATH),
    ));
    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_the_requested_file_creation_exception() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_CREATION_EXCEPTION_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_the_requested_file_besides_scope() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_BESIDES_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_a_later_requested_path_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(
        SYNTHETIC_REQUESTED_FILE_EXCEPTION_WITH_LATER_DENIAL_REPORT,
        false,
    );

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_a_deleted_requested_path() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_DELETED_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_a_pronoun_deleted_requested_path() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_PRONOUN_DELETED_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_a_removed_requested_path() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_REMOVED_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_an_empty_requested_path() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_EMPTY_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_a_zero_byte_requested_path() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_ZERO_BYTES_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_a_not_empty_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_NOT_EMPTY_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_an_initially_empty_state() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_INITIALLY_EMPTY_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_an_empty_at_first_state() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_EMPTY_AT_FIRST_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_incorrect_requested_contents() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_INCORRECT_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_wrong_requested_contents() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_WRONG_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_mismatched_requested_contents() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_MISMATCHED_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_not_incorrect_contents() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_NOT_INCORRECT_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_a_not_deleted_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_NOT_DELETED_ASSURANCE_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_a_backup_file_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_BACKUP_FILE_ASSURANCE_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_an_unrelated_file_exception() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_EXCEPTION_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new("unrelated.txt"))
    );
}

#[test]
fn exec_file_creation_report_rejects_a_nominalized_modification_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOMINALIZED_MODIFICATION_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_bare_nominalized_modification_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_BARE_NOMINALIZED_MODIFICATION_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_an_inverted_modification_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_INVERTED_MODIFICATION_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn final_response_report_accepts_a_read_only_modification_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_VERB_FIRST_MODIFICATION_DENIAL_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn exec_file_creation_report_accepts_a_collateral_modification_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SCOPED_NEGATION_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_an_additional_file_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ADDITIONAL_FILE_MODIFICATION_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_collateral_nominalized_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(
        SYNTHETIC_COLLATERAL_NOMINALIZED_MODIFICATION_DENIAL_REPORT,
        false,
    );

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_collateral_inverted_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(
        SYNTHETIC_COLLATERAL_INVERTED_MODIFICATION_DENIAL_REPORT,
        false,
    );

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_bare_no_changes_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_BARE_NO_CHANGES_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_zero_changes_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ZERO_CHANGES_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_verb_first_change_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_VERB_FIRST_CHANGE_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_collateral_verb_first_change_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COLLATERAL_VERB_FIRST_CHANGE_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_collateral_no_changes_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COLLATERAL_NO_CHANGES_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_collateral_no_modifications_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COLLATERAL_NO_MODIFICATIONS_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn final_response_report_accepts_completion_with_bare_no_changes() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_BARE_NO_CHANGES_DENIAL_REPORT, false);

    assert!(tracker.final_response_reports_completion());
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
fn exec_file_creation_report_accepts_a_collateral_existential_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(
        SYNTHETIC_COLLATERAL_EXISTENTIAL_NO_MODIFICATIONS_REPORT,
        false,
    );

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_an_unchanged_file_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_UNCHANGED_FILE_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_subject_first_missing_file() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SUBJECT_FIRST_MISSING_FILE_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_prior_nonexistence() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_PRIOR_NONEXISTENCE_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_previous_nonexistence() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_PREVIOUS_NONEXISTENCE_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_historical_missing_state() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_HISTORICAL_MISSING_FILE_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn forced_edit_report_rejects_completion_with_no_file_changes() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILE_CHANGES_COMPLETION_REPORT, false);

    assert!(!forced_case_completion_reported(
        EDIT_FILE_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_edit_report_rejects_a_file_not_modified_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_FILE_NOT_MODIFIED_REPORT, false);

    assert!(!forced_case_completion_reported(
        EDIT_FILE_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn workspace_mutation_report_rejects_a_file_not_modified_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_FILE_NOT_MODIFIED_REPORT, false);

    assert!(!tracker.final_response_reports_completion_with_file_mutation());
}

#[test]
fn workspace_mutation_report_rejects_a_direct_file_edit_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILE_EDITED_REPORT, false);

    assert!(!tracker.final_response_reports_completion_with_file_mutation());
}

#[test]
fn forced_edit_report_accepts_a_collateral_no_file_changes_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(
        SYNTHETIC_COLLATERAL_NO_FILE_CHANGES_COMPLETION_REPORT,
        false,
    );

    assert!(forced_case_completion_reported(
        EDIT_FILE_NAME,
        true,
        &tracker,
    ));
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
fn final_response_report_accepts_an_affirmative_not_only_construction() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_ONLY_COMPLETION_REPORT, false);

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

    assert!(!tracker.final_response_reports_completion_with_file_creation());
}

#[test]
fn final_response_report_rejects_a_no_files_written_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILES_WRITTEN_REPORT, false);

    assert!(!tracker.final_response_reports_completion_with_file_creation());
}

#[test]
fn effect_free_final_response_accepts_a_file_creation_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_EFFECT_FREE_NO_FILE_CREATED_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn exec_file_creation_report_rejects_completion_when_the_file_does_not_exist() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COMPLETION_WITHOUT_FILE_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_completion_when_the_file_is_missing() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_MISSING_FILE_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn final_response_report_accepts_an_affirmative_read() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_READ_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_an_affirmative_branch_switch() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SWITCH_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_nothing_written_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOTHING_WRITTEN_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn forced_read_only_exec_report_accepts_nothing_changed() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOTHING_CHANGED_REPORT, false);

    assert!(forced_case_completion_reported(
        UNSANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_RAN_COMPLETION_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_an_explicit_success() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SUCCEEDED_EXEC_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_hedged_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_HEDGED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_an_attempted_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ATTEMPTED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_an_attempt_followed_by_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ATTEMPTED_THEN_RAN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_partial_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_PARTIAL_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_skipped_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SKIPPED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_a_skip_followed_by_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SKIPPED_THEN_RAN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_an_aborted_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ABORTED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_canceled_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_CANCELED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_cancellation_followed_by_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_CANCELED_THEN_RAN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_an_interrupted_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_INTERRUPTED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_stopped_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_STOPPED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_a_not_interrupted_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_INTERRUPTED_RUN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_interruption_followed_by_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_INTERRUPTED_THEN_RAN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_terminated_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_TERMINATED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_killed_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_KILLED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_a_not_terminated_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_TERMINATED_RUN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_termination_followed_by_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_TERMINATED_THEN_RAN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_blocked_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_BLOCKED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_prevented_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_PREVENTED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_a_not_blocked_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_BLOCKED_RUN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_blocking_followed_by_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_BLOCKED_THEN_RAN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_timed_out_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_TIMED_OUT_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_a_not_timed_out_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_TIMED_OUT_RUN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_timeout_followed_by_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_TIMED_OUT_THEN_RAN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_completion_within_the_timeout() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_WITHIN_TIMEOUT_RUN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_completion_before_the_timeout() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_BEFORE_TIMEOUT_RUN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_bare_timeout_failure() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_HIT_TIMEOUT_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_an_explicit_worked_outcome() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_WORKED_RUN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_polite_run_request() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_PLEASE_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_an_imperative_run_request() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_IMPERATIVE_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_completion_with_an_ancillary_polite_request() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COMPLETED_RUN_WITH_ANCILLARY_REQUEST_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_unsandboxed_exec_report_accepts_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_RAN_COMPLETION_REPORT, false);

    assert!(forced_case_completion_reported(
        UNSANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn exec_file_creation_report_accepts_a_confinement_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COLLATERAL_COULD_NOT_REPORT, false);

    assert!(report_affirms_completion_excepting_path(
        SYNTHETIC_COLLATERAL_COULD_NOT_REPORT,
        Path::new(EXEC_RESULT_PATH),
    ));
    assert!(!report_denies_file_changes_excepting_path(
        SYNTHETIC_COLLATERAL_COULD_NOT_REPORT,
        Path::new(EXEC_RESULT_PATH),
    ));
    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_nothing_changed() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOTHING_CHANGED_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
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
    attempt_denied: bool,
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

fn workspace_mutation_could_alter_seed(request: &RequestSnapshot) -> bool {
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
            | ProcessTranscriptEntry::ProviderCompaction { .. }
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
            | ProcessTurnState::ActiveAwaitingRunnerRecovery { .. }
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
    forced_verification_failed: bool,
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

    fn infrastructure_label(&self) -> &'static str {
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

    fn exact_forced_exec_result_mismatched(&self) -> bool {
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

    fn exact_natural_exec_result_mismatched(&self) -> bool {
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

    fn exact_natural_exec_state_mismatched(&self, natural_state: EvalDisposition) -> bool {
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

    fn natural_infrastructure_label(
        &self,
        family: EvalFamily,
        natural_state: EvalDisposition,
    ) -> &'static str {
        if family == EvalFamily::Exec && self.exact_natural_exec_state_mismatched(natural_state) {
            return "exact state mismatch";
        }
        self.infrastructure_label()
    }

    fn exact_forced_executor_failed(&self) -> bool {
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

    fn exact_forced_verification_failed(&self) -> bool {
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
    fn natural_exec_result_passed(&self) -> bool {
        let [result] = self.tool_results.as_slice() else {
            return false;
        };
        tracked_natural_exec_result_passed(result)
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
        exec_forced_case_passed(target, &result)
    }
}

fn is_exec_tool(target: &str) -> bool {
    matches!(
        target,
        SANDBOXED_EXEC_NAME | UNSANDBOXED_EXEC_NAME | CARGO_DIAGNOSTICS_NAME
    )
}

fn tracked_exec_result_passed(target: &str, result: &TrackedToolResult) -> bool {
    if result.is_error {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&result.content)
        .is_ok_and(|result| exec_forced_case_passed(target, &result))
}

fn tracked_natural_exec_result_passed(result: &TrackedToolResult) -> bool {
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

fn round_tripped_result_count(results: &[TrackedToolResult]) -> usize {
    results.iter().filter(|result| result.round_tripped).count()
}

/// Whether a serialized direct-command or Cargo result reports a runner failure
/// before or around execution, rather than evidence about the requested task.
fn exec_result_is_infrastructure(result: &TrackedToolResult) -> bool {
    exec_result_infrastructure_label(result).is_some()
}

fn exec_result_infrastructure_label(result: &TrackedToolResult) -> Option<&'static str> {
    if result.is_error {
        return None;
    }
    let Ok(result) = serde_json::from_str::<serde_json::Value>(&result.content) else {
        return None;
    };
    let execution = result.get("execution").unwrap_or(&result);
    if execution
        .get("preparation_failure")
        .is_some_and(|failure| !failure.is_null())
    {
        return Some("preparation failure");
    }
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
struct DirectExecEvalResult {
    confinement: DirectExecEvalConfinement,
    outcome: DirectExecEvalOutcome,
    stdout: DirectExecEvalStream,
    stderr: DirectExecEvalStream,
    eval_receipt: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectExecEvalConfinement {
    kind: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectExecEvalOutcome {
    kind: String,
    code: Option<i64>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectExecEvalStream {
    text: String,
    completeness: String,
    encoding: String,
}

struct DirectExecExpectation<'a> {
    confinement: &'a str,
    stdout: &'a str,
}

fn direct_exec_result_passed(
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

fn direct_exec_stream_is(stream: &DirectExecEvalStream, expected: &str) -> bool {
    stream.text == expected && stream.completeness == "complete" && stream.encoding == "utf8"
}

/// The complete result shape needed before one successful Cargo diagnostics
/// exchange can count as forced-tier evidence.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoDiagnosticsEvalResult {
    command: String,
    execution: CargoDiagnosticsEvalExecution,
    diagnostics: CargoDiagnosticsEvalRecords,
    tests: CargoDiagnosticsEvalRecords,
    eval_receipt: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoDiagnosticsEvalExecution {
    confinement: CargoDiagnosticsEvalConfinement,
    outcome: CargoDiagnosticsEvalOutcome,
    stdout: CargoDiagnosticsEvalStream,
    stderr: CargoDiagnosticsEvalStream,
    cargo_failure: serde_json::Value,
    preparation_failure: serde_json::Value,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoDiagnosticsEvalConfinement {
    kind: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoDiagnosticsEvalOutcome {
    kind: String,
    code: Option<i64>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoDiagnosticsEvalStream {
    completeness: String,
    encoding: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoDiagnosticsEvalRecords {
    #[serde(rename = "values")]
    values: Vec<serde_json::Value>,
    limit_reached: bool,
    provenance: String,
    known_truncated: bool,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
        && !result.eval_receipt.is_empty()
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
        && cargo_diagnostics_are_exact_live_fixture_evidence(&result.diagnostics.values)
        && result.tests.provenance == "workspace_influenced"
        && result.tests.values.is_empty()
        && !result.tests.limit_reached
        && !result.tests.known_truncated
}

fn cargo_diagnostics_are_exact_live_fixture_evidence(records: &[serde_json::Value]) -> bool {
    let [record] = records else {
        return false;
    };
    let Ok(diagnostic) = serde_json::from_value::<CargoDiagnosticsEvalDiagnostic>(record.clone())
    else {
        return false;
    };
    let Ok(span) = serde_json::from_value::<CargoDiagnosticsEvalSpan>(diagnostic.span.clone())
    else {
        return false;
    };
    diagnostic.file.as_str() == Some(SYNTHETIC_CARGO_DIAGNOSTIC_FILE)
        && diagnostic.file_completeness == "complete"
        && span.line_start == SYNTHETIC_CARGO_DIAGNOSTIC_LINE
        && span.column_start == SYNTHETIC_CARGO_DIAGNOSTIC_START_COLUMN
        && span.line_end == SYNTHETIC_CARGO_DIAGNOSTIC_LINE
        && span.column_end == SYNTHETIC_CARGO_DIAGNOSTIC_END_COLUMN
        && diagnostic.level == CARGO_WARNING_DIAGNOSTIC_LEVEL
        && diagnostic.level_completeness == "complete"
        && diagnostic.message == LIVE_CARGO_DIAGNOSTIC_MESSAGE
        && diagnostic.message_completeness == "complete"
}

fn cargo_diagnostics_stream_is_valid(stream: &CargoDiagnosticsEvalStream) -> bool {
    stream.completeness == "complete" && stream.encoding == "utf8"
}

fn reject_forced_executor_failures(outcomes: &[CaseOutcome]) -> EvalResult {
    if outcomes
        .iter()
        .any(CaseOutcome::exact_forced_executor_failed)
    {
        return Err(io::Error::other(EXACT_EXECUTOR_FAILURE).into());
    }
    Ok(())
}

fn reject_credential_rejections(report: &FamilyReport) -> EvalResult {
    let forced_rejected = report.forced.iter().any(|outcome| {
        matches!(
            outcome.snapshot.turn_disposition,
            SnapshotTurnDisposition::ProviderFailure(Some(
                ProcessProviderModelCallFailureCause::CredentialRejected
            ))
        )
    });
    let natural_rejected = matches!(
        report.natural.snapshot.turn_disposition,
        SnapshotTurnDisposition::ProviderFailure(Some(
            ProcessProviderModelCallFailureCause::CredentialRejected
        ))
    );
    if forced_rejected || natural_rejected {
        return Err(io::Error::other(CREDENTIAL_REJECTION_FAILURE).into());
    }
    Ok(())
}

fn reject_natural_executor_failure(
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
        automatic_reconciliation_attempts: 0,
        operator_action_required: false,
    };

    assert_eq!(
        SnapshotTurnDisposition::from_process_state(&state),
        SnapshotTurnDisposition::Infrastructure
    );
}

#[test]
fn turn_snapshot_reports_runner_recovery_as_infrastructure() {
    let state = ProcessTurnState::ActiveAwaitingRunnerRecovery {
        runner: RunnerId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_SESSION_ID)),
        placement_revision: RunnerGeneration::one(),
        interrupted_tool_attempt: None,
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
        forced_verification_failed: false,
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

fn synthetic_case_outcome(turn_disposition: SnapshotTurnDisposition) -> CaseOutcome {
    CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: false,
        forced_verification_failed: false,
        tool_results: Vec::new(),
        snapshot: CaseSnapshot {
            turn_disposition,
            requests: Vec::new(),
            model_calls: 1,
        },
    }
}

#[test]
fn forced_credential_rejection_fails_the_job() {
    let report = FamilyReport {
        family: EvalFamily::Git,
        forced: vec![synthetic_case_outcome(
            SnapshotTurnDisposition::ProviderFailure(Some(
                ProcessProviderModelCallFailureCause::CredentialRejected,
            )),
        )],
        natural: synthetic_case_outcome(SnapshotTurnDisposition::Completed),
        natural_state: EvalDisposition::Miss,
    };

    assert_eq!(
        reject_credential_rejections(&report)
            .expect_err("the rejected forced credential fails the job")
            .to_string(),
        CREDENTIAL_REJECTION_FAILURE
    );
}

#[test]
fn natural_credential_rejection_fails_the_job() {
    let report = FamilyReport {
        family: EvalFamily::Git,
        forced: Vec::new(),
        natural: synthetic_case_outcome(SnapshotTurnDisposition::ProviderFailure(Some(
            ProcessProviderModelCallFailureCause::CredentialRejected,
        ))),
        natural_state: EvalDisposition::Infrastructure,
    };

    assert_eq!(
        reject_credential_rejections(&report)
            .expect_err("the rejected natural credential fails the job")
            .to_string(),
        CREDENTIAL_REJECTION_FAILURE
    );
}

#[test]
fn forced_tier_passes_one_completed_target_with_a_result_round_trip() {
    let target = GIT_STATUS_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from("{}")),
        execution_completed: true,
        forced_verification_failed: false,
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
                attempt_denied: false,
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
        forced_verification_failed: false,
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
                attempt_denied: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_tier_reports_and_rejects_an_exact_known_failed_attempt() {
    let target = GIT_STATUS_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from("{}")),
        execution_completed: true,
        forced_verification_failed: false,
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
                attempt_denied: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn forced_tier_rejects_an_exact_failure_before_a_follow_up_call() {
    let target = GIT_STATUS_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from("{}")),
        execution_completed: false,
        forced_verification_failed: false,
        tool_results: Vec::new(),
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(target),
                    arguments_text: String::from("{}"),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: None,
                    attempt_succeeded: false,
                    attempt_denied: false,
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_FOLLOW_UP_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(GIT_LOG_NAME),
                    arguments_text: String::from("{}"),
                    entry_index: ARBITRARY_LATE_RESULT_ENTRY_INDEX,
                    completed_result_entry_index: None,
                    attempt_succeeded: false,
                    attempt_denied: false,
                },
            ],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn unforced_exec_tier_reports_a_normalized_exact_failure_as_infrastructure() -> EvalResult {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        forced_verification_failed: false,
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
                attempt_denied: false,
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
        forced_verification_failed: false,
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
                attempt_denied: false,
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
        forced_verification_failed: false,
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
                attempt_denied: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Git),
        EvalDisposition::Infrastructure
    );
    assert!(
        reject_natural_executor_failure(&outcome, EvalFamily::Git, EvalDisposition::Pass).is_err()
    );
}

#[test]
fn unforced_git_tier_reports_a_premature_commit_failure_as_a_miss() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        forced_verification_failed: false,
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
                attempt_denied: false,
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
        forced_verification_failed: false,
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
                    attempt_denied: false,
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                    name: String::from(GIT_CREATE_COMMIT_NAME),
                    arguments_text: serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: None,
                    attempt_succeeded: false,
                    attempt_denied: false,
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
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_THIRD_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: None,
                attempt_succeeded: false,
                attempt_denied: false,
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
        forced_verification_failed: false,
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
                attempt_denied: false,
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
        forced_verification_failed: false,
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
                    name: String::from(WRITE_FILE_NAME),
                    arguments_text: serde_json::json!({
                        "path": WORKSPACE_SEED_PATH,
                        "content": WORKSPACE_ANSWER,
                    })
                    .to_string(),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                    attempt_succeeded: true,
                    attempt_denied: false,
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                    name: String::from(READ_FILE_NAME),
                    arguments_text: serde_json::json!({"path": WORKSPACE_SEED_PATH}).to_string(),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: None,
                    attempt_succeeded: false,
                    attempt_denied: false,
                },
            ],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Workspace),
        EvalDisposition::Miss
    );
    assert!(
        reject_natural_executor_failure(&outcome, EvalFamily::Workspace, EvalDisposition::Pass)
            .is_ok()
    );
}

#[test]
fn unforced_workspace_tier_reports_read_failure_after_an_unrelated_mutation_as_infrastructure() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        forced_verification_failed: false,
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
                    name: String::from(WRITE_FILE_NAME),
                    arguments_text: serde_json::json!({
                        "path": WORKSPACE_ANSWER_PATH,
                        "content": WORKSPACE_ANSWER,
                    })
                    .to_string(),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                    attempt_succeeded: true,
                    attempt_denied: false,
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                    name: String::from(READ_FILE_NAME),
                    arguments_text: bounded_workspace_read_arguments().to_string(),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: None,
                    attempt_succeeded: false,
                    attempt_denied: false,
                },
            ],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Workspace),
        EvalDisposition::Infrastructure
    );
    assert!(
        reject_natural_executor_failure(&outcome, EvalFamily::Workspace, EvalDisposition::Pass)
            .is_err()
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
        forced_verification_failed: false,
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
        attempt_denied: false,
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
        attempt_denied: true,
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
            attempt_denied: false,
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
        forced_verification_failed: false,
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
                    attempt_denied: false,
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
                    attempt_denied: false,
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
        forced_verification_failed: false,
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
                attempt_denied: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Git),
        EvalDisposition::Miss
    );
}

struct GitNaturalFilesystemFixture {
    suite: FamilySuite,
    stage_window: FilesystemExecutionTimeWindow,
    commit_window: FilesystemExecutionTimeWindow,
}

fn git_natural_filesystem_fixture() -> EvalResult<GitNaturalFilesystemFixture> {
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

#[test]
fn git_natural_object_entries_accept_the_stage_and_commit_windows() -> EvalResult {
    let fixture = git_natural_filesystem_fixture()?;
    let repository = Repository::open(fixture.suite.workspace.path())?;
    let head = repository.head()?.peel_to_commit()?;
    let entries = fixture
        .suite
        .git_pre_execution_object_entries
        .lock()
        .expect("Git pre-execution object-entry lock is available");
    let modified_times = fixture
        .suite
        .git_pre_execution_object_modified_times
        .lock()
        .expect("Git pre-execution object-time lock is available");
    let entry_identities = fixture
        .suite
        .git_pre_execution_object_entry_identities
        .lock()
        .expect("Git pre-execution object-identity lock is available");

    assert!(git_natural_object_entries_match(
        fixture.suite.workspace.path(),
        &head,
        &fixture.suite.git_seed_fixture,
        Some(fixture.stage_window),
        GitObjectEntryVerification {
            pre_execution_entries: entries.as_ref(),
            pre_execution_modified_times: modified_times.as_ref(),
            pre_execution_entry_identities: entry_identities.as_ref(),
            execution_window: Some(fixture.commit_window),
        },
    )?);
    Ok(())
}

#[test]
fn git_natural_object_entries_reject_a_disjoint_stage_window_for_the_staged_blob() -> EvalResult {
    let fixture = git_natural_filesystem_fixture()?;
    let repository = Repository::open(fixture.suite.workspace.path())?;
    let head = repository.head()?.peel_to_commit()?;
    let entries = fixture
        .suite
        .git_pre_execution_object_entries
        .lock()
        .expect("Git pre-execution object-entry lock is available");
    let modified_times = fixture
        .suite
        .git_pre_execution_object_modified_times
        .lock()
        .expect("Git pre-execution object-time lock is available");
    let entry_identities = fixture
        .suite
        .git_pre_execution_object_entry_identities
        .lock()
        .expect("Git pre-execution object-identity lock is available");

    assert!(!git_natural_object_entries_match(
        fixture.suite.workspace.path(),
        &head,
        &fixture.suite.git_seed_fixture,
        Some(FilesystemExecutionTimeWindow {
            started: UNIX_EPOCH,
            finished: UNIX_EPOCH,
        }),
        GitObjectEntryVerification {
            pre_execution_entries: entries.as_ref(),
            pre_execution_modified_times: modified_times.as_ref(),
            pre_execution_entry_identities: entry_identities.as_ref(),
            execution_window: Some(fixture.commit_window),
        },
    )?);
    Ok(())
}

#[test]
fn git_natural_metadata_root_accepts_an_operation_window() -> EvalResult {
    let fixture = git_natural_filesystem_fixture()?;

    assert!(git_natural_metadata_root_times_match(
        fixture.suite.workspace.path(),
        &fixture.suite.git_seed_fixture,
        Some(fixture.stage_window),
        Some(fixture.commit_window),
    )?);
    Ok(())
}

#[test]
fn git_natural_metadata_root_rejects_post_commit_timestamp_drift() -> EvalResult {
    let fixture = git_natural_filesystem_fixture()?;
    let repository = Repository::open(fixture.suite.workspace.path())?;
    fs::File::open(repository.path())?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;

    assert!(!git_natural_metadata_root_times_match(
        fixture.suite.workspace.path(),
        &fixture.suite.git_seed_fixture,
        Some(fixture.stage_window),
        Some(fixture.commit_window),
    )?);
    Ok(())
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

#[cfg(unix)]
#[test]
fn git_natural_state_rejects_a_byte_identical_target_replacement() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (_, _, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;

    assert!(git_natural_worktree_entry_identities_match(
        workspace.path(),
        &seed_fixture,
    )?);

    let target = workspace.path().join(GIT_NATURAL_PATH);
    replace_git_metadata_file_byte_identically(
        &target,
        seed_fixture.worktree_modified_times[Path::new(GIT_NATURAL_PATH)],
        seed_fixture.worktree_modified_times[Path::new("")],
    )?;

    assert_eq!(
        git_worktree_entries(workspace.path())?,
        seed_fixture.worktree_entries
    );
    assert_eq!(
        git_worktree_modified_times(workspace.path())?,
        seed_fixture.worktree_modified_times
    );
    assert_ne!(
        git_worktree_entry_identities(workspace.path())?,
        seed_fixture.worktree_entry_identities
    );
    assert!(!git_natural_worktree_entry_identities_match(
        workspace.path(),
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

#[test]
fn git_natural_state_rejects_unrelated_stat_cache_drift() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    drift_git_index_ctime(workspace.path(), GIT_SEED_PATH)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_index_extension_drift() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    append_synthetic_git_index_extension(&Repository::open(workspace.path())?)?;

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
    let started = current_filesystem_recorded_time()?;
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    suite.executor.record_filesystem_execution_window(
        GIT_STAGE_NAME,
        FilesystemExecutionTimeWindow {
            started,
            finished: current_filesystem_recorded_time()?,
        },
    );
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
fn git_object_entry_inventory_accepts_an_exact_pack_publication() -> EvalResult {
    let suite = FamilySuite::git()?;
    let repository = Repository::open(suite.workspace.path())?;
    let started = current_filesystem_recorded_time()?;
    let object_id = repository.blob(GIT_STAGE_CONTENT.as_bytes())?;
    publish_git_object_pack_for_test(
        &repository,
        &[object_id],
        &suite.git_seed_fixture.object_entries,
    )?;
    let execution_window = FilesystemExecutionTimeWindow {
        started,
        finished: current_filesystem_recorded_time()?,
    };

    assert!(git_object_entry_inventory_matches(
        suite.workspace.path(),
        &suite.git_seed_fixture.object_entries,
        &suite.git_seed_fixture.object_modified_times,
        &suite.git_seed_fixture.object_entry_identities,
        &[object_id],
        &suite.git_seed_fixture,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn git_object_entry_inventory_rejects_a_pack_with_a_collateral_object() -> EvalResult {
    let suite = FamilySuite::git()?;
    let repository = Repository::open(suite.workspace.path())?;
    let started = current_filesystem_recorded_time()?;
    let allowed_id = repository.blob(GIT_STAGE_CONTENT.as_bytes())?;
    let collateral_id = repository.blob(GIT_COLLATERAL_OBJECT_CONTENT)?;
    publish_git_object_pack_for_test(
        &repository,
        &[allowed_id, collateral_id],
        &suite.git_seed_fixture.object_entries,
    )?;
    let execution_window = FilesystemExecutionTimeWindow {
        started,
        finished: current_filesystem_recorded_time()?,
    };

    assert!(!git_object_entry_inventory_matches(
        suite.workspace.path(),
        &suite.git_seed_fixture.object_entries,
        &suite.git_seed_fixture.object_modified_times,
        &suite.git_seed_fixture.object_entry_identities,
        &[allowed_id],
        &suite.git_seed_fixture,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn forced_git_stage_verifier_rejects_unrelated_stat_cache_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STAGE_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    drift_git_index_ctime(suite.workspace.path(), GIT_SEED_PATH)?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
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
    suite.commit_staged_paths_for_test(message)?;
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
    suite.commit_staged_paths_for_test(message)?;
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
    suite.commit_staged_paths_for_test(message)?;
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
    suite.commit_staged_paths_for_test(message)?;
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
    suite.commit_staged_paths_for_test(message)?;
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
    suite.commit_staged_paths_for_test(message)?;
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
    suite.commit_staged_paths_for_test(message)?;
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
    suite.commit_staged_paths_with_identity_for_test(
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
    suite.commit_staged_paths_for_test(message)?;
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
    suite.commit_staged_paths_for_test(message)?;
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
fn forced_git_branch_create_verifier_accepts_the_exact_new_reference() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_BRANCH_CREATE_NAME)
        .expect("the Git branch-create fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let branch_name = arguments["name"]
        .as_str()
        .expect("the Git branch-create fixture has a name");
    let start = arguments["start"]
        .as_str()
        .expect("the Git branch-create fixture has a start reference");
    let started = current_filesystem_recorded_time()?;
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository.find_reference(start)?.peel_to_commit()?;
    fs::write(
        repository.path().join("refs/heads").join(branch_name),
        format!("{}\n", target.id()),
    )?;
    suite.executor.record_filesystem_execution_window(
        GIT_BRANCH_CREATE_NAME,
        FilesystemExecutionTimeWindow {
            started,
            finished: current_filesystem_recorded_time()?,
        },
    );
    let result = serde_json::json!({
        "branch": branch_name,
        "head": target.id().to_string(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
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

#[cfg(unix)]
struct GitBranchSwitchTimestampFixture {
    suite: FamilySuite,
    pre_worktree_modified_times: BTreeMap<PathBuf, SystemTime>,
    pre_worktree_entry_identities: BTreeMap<PathBuf, FilesystemIdentity>,
    pre_metadata_root_modified_time: SystemTime,
    pre_metadata_root_identity: FilesystemIdentity,
    execution_window: FilesystemExecutionTimeWindow,
}

#[cfg(unix)]
fn git_branch_switch_timestamp_fixture() -> EvalResult<GitBranchSwitchTimestampFixture> {
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

#[cfg(unix)]
#[test]
fn forced_git_branch_switch_timestamp_gates_accept_the_exact_checkout() -> EvalResult {
    let fixture = git_branch_switch_timestamp_fixture()?;
    let actual_worktree_modified_times =
        git_worktree_modified_times(fixture.suite.workspace.path())?;

    assert_eq!(
        fs::read_to_string(fixture.suite.workspace.path().join(GIT_SEED_PATH))?,
        GIT_SWITCH_CONTENT,
    );
    assert!(git_forced_metadata_root_modified_time_matches(
        fixture.suite.workspace.path(),
        GIT_BRANCH_SWITCH_NAME,
        &fixture.suite.git_seed_fixture,
        Some(fixture.pre_metadata_root_modified_time),
        Some(fixture.pre_metadata_root_identity),
        Some(fixture.execution_window),
    )?);
    assert!(
        git_forced_worktree_modified_times_match(
            fixture.suite.workspace.path(),
            GIT_BRANCH_SWITCH_NAME,
            &fixture.suite.git_seed_fixture,
            Some(&fixture.pre_worktree_modified_times),
            Some(fixture.execution_window),
        )?,
        "actual target time: {:?}; execution window: {:?}",
        actual_worktree_modified_times[Path::new(GIT_SEED_PATH)],
        fixture.execution_window,
    );
    assert!(git_forced_worktree_entry_identities_match(
        fixture.suite.workspace.path(),
        GIT_BRANCH_SWITCH_NAME,
        &fixture.suite.git_seed_fixture,
        Some(&fixture.pre_worktree_entry_identities),
        Some(fixture.execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_metadata_root_gate_rejects_an_epoch_mtime_after_switch() -> EvalResult {
    let fixture = git_branch_switch_timestamp_fixture()?;
    let repository = Repository::open(fixture.suite.workspace.path())?;
    fs::File::open(repository.path())?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;

    assert!(!git_forced_metadata_root_modified_time_matches(
        fixture.suite.workspace.path(),
        GIT_BRANCH_SWITCH_NAME,
        &fixture.suite.git_seed_fixture,
        Some(fixture.pre_metadata_root_modified_time),
        Some(fixture.pre_metadata_root_identity),
        Some(fixture.execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_metadata_root_gate_rejects_an_epoch_change_time() -> EvalResult {
    let fixture = git_branch_switch_timestamp_fixture()?;
    let mut actual_identity = git_metadata_root_identity(fixture.suite.workspace.path())?
        .expect("the switched Git fixture has a metadata-root identity");
    actual_identity.change_time_seconds = 0;
    actual_identity.change_time_nanoseconds = 0;

    assert!(!git_mutated_metadata_root_times_match(
        git_metadata_root_modified_time(fixture.suite.workspace.path())?,
        Some(actual_identity),
        Some(fixture.pre_metadata_root_identity),
        Some(fixture.execution_window),
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_branch_switch_gate_rejects_an_epoch_target_mtime() -> EvalResult {
    let fixture = git_branch_switch_timestamp_fixture()?;
    fs::File::open(fixture.suite.workspace.path().join(GIT_SEED_PATH))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;

    assert!(!git_forced_worktree_modified_times_match(
        fixture.suite.workspace.path(),
        GIT_BRANCH_SWITCH_NAME,
        &fixture.suite.git_seed_fixture,
        Some(&fixture.pre_worktree_modified_times),
        Some(fixture.execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_branch_switch_gate_rejects_an_epoch_target_change_time() -> EvalResult {
    let fixture = git_branch_switch_timestamp_fixture()?;
    let target = Path::new(GIT_SEED_PATH);
    let mut actual_identity =
        git_worktree_entry_identities(fixture.suite.workspace.path())?[target];
    actual_identity.change_time_seconds = 0;
    actual_identity.change_time_nanoseconds = 0;

    assert!(!git_branch_switch_target_identity_matches(
        Some(&actual_identity),
        fixture.pre_worktree_entry_identities.get(target),
        Some(fixture.execution_window),
    ));
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn forced_git_branch_switch_attribute_gate_rejects_target_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_BRANCH_SWITCH_NAME)?;
    let pre_execution = suite
        .git_pre_execution_worktree_extended_attributes
        .lock()
        .expect("Git pre-execution worktree-attribute lock is available");
    let relative = Path::new(GIT_SEED_PATH);
    let target = suite.workspace.path().join(relative);
    assert!(git_forced_worktree_extended_attributes_match(
        suite.workspace.path(),
        &suite.git_seed_fixture,
        pre_execution.as_ref(),
    )?);
    rustix::fs::setxattr(
        &target,
        SYNTHETIC_UNEXPECTED_XATTR_NAME,
        SYNTHETIC_UNEXPECTED_XATTR_VALUE,
        rustix::fs::XattrFlags::CREATE,
    )?;

    assert_ne!(
        git_worktree_extended_attributes(suite.workspace.path())?[relative],
        pre_execution
            .as_ref()
            .expect("the Git branch-switch fixture has a captured attribute inventory")[relative]
    );
    assert!(!git_forced_worktree_extended_attributes_match(
        suite.workspace.path(),
        &suite.git_seed_fixture,
        pre_execution.as_ref(),
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn forced_git_metadata_attribute_gate_rejects_index_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STAGE_NAME)?;
    let pre_execution = suite
        .git_pre_execution_metadata_extended_attributes
        .lock()
        .expect("Git pre-execution metadata-attribute lock is available");
    let repository = Repository::open(suite.workspace.path())?;
    let relative = Path::new(GIT_INDEX_PATH);
    let target = repository.path().join(relative);
    assert!(git_metadata_extended_attributes_match(
        suite.workspace.path(),
        &suite.git_seed_fixture,
        pre_execution.as_ref(),
    )?);
    rustix::fs::setxattr(
        &target,
        SYNTHETIC_UNEXPECTED_XATTR_NAME,
        SYNTHETIC_UNEXPECTED_XATTR_VALUE,
        rustix::fs::XattrFlags::CREATE,
    )?;

    assert_ne!(
        git_metadata_extended_attributes(suite.workspace.path())?[relative],
        pre_execution
            .as_ref()
            .expect("the Git stage fixture has a captured metadata-attribute inventory")[relative]
    );
    assert!(!git_metadata_extended_attributes_match(
        suite.workspace.path(),
        &suite.git_seed_fixture,
        pre_execution.as_ref(),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_metadata_mutation_gate_rejects_index_ownership_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    let mut actual = git_metadata_top_level(suite.workspace.path())?;
    let mut expected = actual.clone();
    let index = Path::new(GIT_INDEX_PATH);
    let changed_group_id = actual[index]
        .identity
        .expect("the Git index has a filesystem identity")
        .group_id
        .wrapping_add(1);
    actual
        .get_mut(index)
        .expect("the Git index has a metadata snapshot")
        .identity
        .as_mut()
        .expect("the Git index has a mutable filesystem identity")
        .group_id = changed_group_id;

    assert!(!admit_git_metadata_file_mutation(
        &actual,
        &mut expected,
        index,
        None,
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_metadata_mutation_gate_rejects_an_out_of_window_index_mtime() -> EvalResult {
    let suite = FamilySuite::git()?;
    let index = Path::new(GIT_INDEX_PATH);
    let mut expected = git_metadata_top_level(suite.workspace.path())?;
    let started = current_filesystem_recorded_time()?;
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    let window = FilesystemExecutionTimeWindow {
        started,
        finished: current_filesystem_recorded_time()?,
    };
    let mut actual = git_metadata_top_level(suite.workspace.path())?;
    actual
        .get_mut(index)
        .expect("the Git index has a metadata snapshot")
        .modified = Some(UNIX_EPOCH);

    assert!(!admit_git_metadata_file_mutation(
        &actual,
        &mut expected,
        index,
        Some(window),
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn rewritten_git_identity_gate_rejects_changed_ownership() -> EvalResult {
    let suite = FamilySuite::git()?;
    let target = Path::new("heads").join(GIT_BASE_BRANCH);
    let mut actual = suite.git_seed_fixture.reference_entry_identities.clone();
    let mut expected = actual.clone();
    let target_identity = actual[&target];
    let recorded = UNIX_EPOCH
        + Duration::new(
            target_identity
                .change_time_seconds
                .try_into()
                .expect("the captured change time is nonnegative"),
            target_identity
                .change_time_nanoseconds
                .try_into()
                .expect("the captured change-time fraction is valid"),
        );
    let execution_window = FilesystemExecutionTimeWindow {
        started: recorded,
        finished: recorded,
    };
    let mut unchanged_expected = expected.clone();

    assert!(admit_filesystem_identity_path(
        &actual,
        &mut unchanged_expected,
        &target,
        Some(execution_window),
    ));

    let changed_group_id = target_identity.group_id.wrapping_add(1);
    actual
        .get_mut(&target)
        .expect("the seeded branch has a filesystem identity")
        .group_id = changed_group_id;

    assert!(!admit_filesystem_identity_path(
        &actual,
        &mut expected,
        &target,
        Some(execution_window),
    ));
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
    suite.prepare_git_case(GIT_LOG_NAME)?;
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
fn forced_git_log_verifier_rejects_worktree_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let collateral = suite.workspace.path().join(GIT_STAGE_PATH);
    fs::File::open(collateral)?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
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

#[cfg(unix)]
#[test]
fn forced_git_log_verifier_rejects_byte_identical_worktree_replacement() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target_commit = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let target = suite.workspace.path().join(GIT_STAGE_PATH);
    let replacement = suite.workspace.path().join("replacement-fixture");
    let target_modified = *suite
        .git_seed_fixture
        .worktree_modified_times
        .get(Path::new(GIT_STAGE_PATH))
        .expect("the Git fixture has a captured target modified time");
    let root_modified = *suite
        .git_seed_fixture
        .worktree_modified_times
        .get(Path::new(""))
        .expect("the Git fixture has a captured root modified time");
    let permissions = fs::metadata(&target)?.permissions();
    fs::write(&replacement, GIT_STAGE_CONTENT)?;
    fs::set_permissions(&replacement, permissions)?;
    fs::rename(&replacement, &target)?;
    fs::File::open(&target)?.set_times(fs::FileTimes::new().set_modified(target_modified))?;
    fs::File::open(suite.workspace.path())?
        .set_times(fs::FileTimes::new().set_modified(root_modified))?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target_commit.id().to_string(),
            "author_name": target_commit.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target_commit.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target_commit.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(
        git_worktree_entries(suite.workspace.path())?,
        suite.git_seed_fixture.worktree_entries
    );
    assert_eq!(
        git_worktree_modified_times(suite.workspace.path())?,
        suite.git_seed_fixture.worktree_modified_times
    );
    assert_ne!(
        git_worktree_entry_identities(suite.workspace.path())?,
        suite.git_seed_fixture.worktree_entry_identities
    );
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

fn forced_git_log_result(suite: &FamilySuite) -> EvalResult<String> {
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
fn replace_git_metadata_file_byte_identically(
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

#[cfg(unix)]
#[test]
fn forced_git_log_verifier_rejects_byte_identical_nested_reference_replacement() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let relative = Path::new("heads/log-target");
    let parent = Path::new("heads");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository.path().join(GIT_REFS_DIRECTORY).join(relative);
    let target_modified = suite.git_seed_fixture.reference_modified_times[relative];
    let parent_modified = suite.git_seed_fixture.reference_modified_times[parent];
    replace_git_metadata_file_byte_identically(&target, target_modified, parent_modified)?;
    let result = forced_git_log_result(&suite)?;

    assert_ne!(
        git_reference_entry_identities(suite.workspace.path())?,
        suite.git_seed_fixture.reference_entry_identities
    );
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_log_verifier_rejects_byte_identical_reflog_replacement() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let relative = Path::new("HEAD");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository.path().join(GIT_LOGS_DIRECTORY).join(relative);
    let target_modified = suite.git_seed_fixture.reflog_modified_times[relative];
    let parent_modified = suite.git_seed_fixture.reflog_modified_times[Path::new("")];
    replace_git_metadata_file_byte_identically(&target, target_modified, parent_modified)?;
    let result = forced_git_log_result(&suite)?;

    assert_ne!(
        git_reflog_entry_identities(suite.workspace.path())?,
        suite.git_seed_fixture.reflog_entry_identities
    );
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_log_verifier_rejects_byte_identical_loose_object_replacement() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target_commit = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let relative = git_loose_object_relative_path(target_commit.id());
    let parent = relative
        .parent()
        .expect("the loose object fixture has a parent");
    let target = repository
        .path()
        .join(GIT_OBJECTS_DIRECTORY)
        .join(&relative);
    let target_modified = suite.git_seed_fixture.object_modified_times[&relative];
    let parent_modified = suite.git_seed_fixture.object_modified_times[parent];
    replace_git_metadata_file_byte_identically(&target, target_modified, parent_modified)?;
    let result = forced_git_log_result(&suite)?;

    assert_ne!(
        git_object_entry_identities(suite.workspace.path())?,
        suite.git_seed_fixture.object_entry_identities
    );
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_log_verifier_rejects_byte_identical_static_metadata_replacement() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let relative = Path::new(GIT_DESCRIPTION_PATH);
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository.path().join(relative);
    let target_modified = suite.git_seed_fixture.static_metadata_modified_times[relative];
    let parent_modified = suite
        .git_seed_fixture
        .metadata_root_modified_time
        .expect("the Git fixture has a metadata-root modified time");
    replace_git_metadata_file_byte_identically(&target, target_modified, parent_modified)?;
    let result = forced_git_log_result(&suite)?;

    assert_ne!(
        git_static_metadata_entry_identities(suite.workspace.path())?,
        suite.git_seed_fixture.static_metadata_entry_identities
    );
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_content_in_a_seeded_metadata_directory() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let branches = repository.path().join(GIT_BRANCHES_DIRECTORY);
    let modified =
        suite.git_seed_fixture.static_metadata_modified_times[Path::new(GIT_BRANCHES_DIRECTORY)];
    fs::write(branches.join("collateral"), "synthetic metadata fixture\n")?;
    fs::File::open(branches)?.set_times(fs::FileTimes::new().set_modified(modified))?;
    let result = forced_git_log_result(&suite)?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_metadata_root_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    fs::File::open(repository.path())?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
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
fn forced_git_log_verifier_rejects_loose_object_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let object_path = repository
        .path()
        .join(GIT_OBJECTS_DIRECTORY)
        .join(git_loose_object_relative_path(target.id()));
    fs::File::open(object_path)?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
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
fn forced_git_log_verifier_rejects_top_level_metadata_byte_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    fs::write(
        repository.path().join(GIT_HEAD_PATH),
        format!("ref: refs/heads/{GIT_BASE_BRANCH}"),
    )?;
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

#[cfg(unix)]
#[test]
fn forced_git_log_verifier_rejects_byte_identical_metadata_file_replacement() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target_commit = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let target = repository.path().join(GIT_CONFIG_PATH);
    let replacement = repository.path().join("config-replacement-fixture");
    let expected_config = suite
        .git_seed_fixture
        .metadata_top_level
        .get(Path::new(GIT_CONFIG_PATH))
        .expect("the Git fixture has a captured config entry");
    let target_modified = expected_config
        .modified
        .expect("the Git fixture has a captured config modified time");
    let root_modified = suite
        .git_seed_fixture
        .metadata_root_modified_time
        .expect("the Git fixture has a captured metadata-root modified time");
    let permissions = fs::metadata(&target)?.permissions();
    fs::write(&replacement, &suite.git_seed_fixture.config)?;
    fs::set_permissions(&replacement, permissions)?;
    fs::rename(&replacement, &target)?;
    fs::File::open(&target)?.set_times(fs::FileTimes::new().set_modified(target_modified))?;
    fs::File::open(repository.path())?
        .set_times(fs::FileTimes::new().set_modified(root_modified))?;
    let actual_metadata = git_metadata_top_level(suite.workspace.path())?;
    let actual_config = actual_metadata
        .get(Path::new(GIT_CONFIG_PATH))
        .expect("the replaced config remains in the metadata inventory");
    let result = serde_json::json!({
        "commits": [{
            "commit": target_commit.id().to_string(),
            "author_name": target_commit.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target_commit.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target_commit.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(actual_config.content, expected_config.content);
    assert_eq!(actual_config.modified, expected_config.modified);
    assert_ne!(actual_config.identity, expected_config.identity);
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_top_level_metadata_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    fs::File::open(repository.path().join(GIT_CONFIG_PATH))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
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
fn forced_git_log_verifier_rejects_metadata_directory_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    fs::File::open(repository.path().join(GIT_HOOKS_DIRECTORY))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
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
fn forced_git_log_verifier_rejects_static_metadata_file_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    fs::File::open(repository.path().join(GIT_DESCRIPTION_PATH))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
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
fn forced_git_log_verifier_rejects_nested_reference_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    fs::File::open(
        repository
            .path()
            .join(GIT_REFS_DIRECTORY)
            .join("heads")
            .join("log-target"),
    )?
    .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
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
fn forced_git_log_verifier_rejects_nested_reference_directory_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    fs::File::open(repository.path().join(GIT_REFS_DIRECTORY).join("heads"))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
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
fn forced_git_log_verifier_rejects_reflog_file_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    fs::File::open(repository.path().join(GIT_LOGS_DIRECTORY).join("HEAD"))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
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

#[test]
fn forced_git_status_verifier_rejects_stat_cache_drift() -> EvalResult {
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
    entry.ctime = IndexTime::new(
        entry.ctime.seconds().wrapping_add(1),
        entry.ctime.nanoseconds(),
    );
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

#[test]
fn forced_git_status_verifier_rejects_index_extension_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    append_synthetic_git_index_extension(&Repository::open(suite.workspace.path())?)?;
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
    let started = current_filesystem_recorded_time()?;
    fs::write(&target, content)?;
    fs::set_permissions(
        &target,
        fs::Permissions::from_mode(WORKSPACE_PRIVATE_CREATION_MODE),
    )?;
    suite.executor.record_filesystem_execution_window(
        WRITE_FILE_NAME,
        FilesystemExecutionTimeWindow {
            started,
            finished: current_filesystem_recorded_time()?,
        },
    );
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
fn workspace_mutation_target_time_gate_rejects_a_pre_execution_mtime() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let target = Path::new("created-during-execution.txt");
    let started = current_filesystem_recorded_time()?;
    fs::write(suite.workspace.path().join(target), WORKSPACE_ANSWER)?;
    let window = FilesystemExecutionTimeWindow {
        started,
        finished: current_filesystem_recorded_time()?,
    };
    assert!(workspace_mutation_entry_times_match(
        suite.workspace.path(),
        target,
        Some(window),
    )?);
    fs::File::open(suite.workspace.path().join(target))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;

    assert!(!workspace_mutation_entry_times_match(
        suite.workspace.path(),
        target,
        Some(window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn workspace_mutation_identity_gate_rejects_changed_target_ownership() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let target = Path::new(WORKSPACE_SEED_PATH);
    let mut actual = suite.workspace_seed_entry_identities.clone();
    let changed_group_id = actual[target].group_id.wrapping_add(1);
    actual
        .get_mut(target)
        .expect("the workspace seed has a filesystem identity")
        .group_id = changed_group_id;

    assert!(!entry_identities_match_except(
        actual,
        &suite.workspace_seed_entry_identities,
        &[target],
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn workspace_mutation_identity_gate_rejects_changed_new_target_ownership() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let target = Path::new("created-during-execution.txt");
    let mut actual = suite.workspace_seed_entry_identities.clone();
    let mut target_identity = actual[Path::new("")];
    target_identity.group_id = target_identity.group_id.wrapping_add(1);
    actual.insert(target.to_path_buf(), target_identity);

    assert!(!entry_identities_match_except(
        actual,
        &suite.workspace_seed_entry_identities,
        &[target],
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn identity_gate_without_change_time_rejects_changed_ownership() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let expected = suite.workspace_seed_entry_identities[Path::new("")];
    let mut actual = expected;
    actual.group_id = expected.group_id.wrapping_add(1);

    assert!(!filesystem_identity_matches_without_change_time(
        Some(actual),
        Some(expected),
    ));
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

#[test]
fn forced_workspace_edit_verifier_rejects_collateral_mtime_drift() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == EDIT_FILE_NAME)
        .expect("the workspace edit fixture exists");
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_EDITED_SEED,
    )?;
    let collateral = suite.workspace.path().join(WORKSPACE_GLOB_PATH);
    fs::File::open(collateral)?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "replacements": EXPECTED_WORKSPACE_EDIT_REPLACEMENTS,
        "bytes_written": EXPECTED_WORKSPACE_EDIT_BYTES,
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

#[cfg(target_os = "linux")]
#[test]
fn forced_workspace_edit_verifier_rejects_an_added_extended_attribute() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == EDIT_FILE_NAME)
        .expect("the workspace edit fixture exists");
    let relative = Path::new(WORKSPACE_SEED_PATH);
    let path = suite.workspace.path().join(relative);
    fs::write(&path, WORKSPACE_EDITED_SEED)?;
    rustix::fs::setxattr(
        &path,
        SYNTHETIC_UNEXPECTED_XATTR_NAME,
        SYNTHETIC_UNEXPECTED_XATTR_VALUE,
        rustix::fs::XattrFlags::CREATE,
    )?;
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "replacements": EXPECTED_WORKSPACE_EDIT_REPLACEMENTS,
        "bytes_written": EXPECTED_WORKSPACE_EDIT_BYTES,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_ne!(
        workspace_extended_attributes(suite.workspace.path())?[relative],
        suite.workspace_seed_extended_attributes[relative]
    );
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
        "offset": 0,
        "bytes_read": WORKSPACE_SEED.len(),
        "next_offset": WORKSPACE_SEED.len(),
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
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
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
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
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
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_read_verifier_rejects_collateral_mtime_drift() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    let collateral = suite.workspace.path().join(WORKSPACE_GLOB_NONMATCHING_PATH);
    fs::File::open(collateral)?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    let prefix = WORKSPACE_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
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
fn forced_workspace_read_verifier_rejects_restored_mode_ctime_drift() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    let relative = Path::new(WORKSPACE_GLOB_NONMATCHING_PATH);
    let path = suite.workspace.path().join(relative);
    let original_permissions = fs::metadata(&path)?.permissions();
    let mut changed_permissions = original_permissions.clone();
    changed_permissions.set_mode(original_permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    std::thread::sleep(Duration::from_millis(1));
    fs::set_permissions(&path, changed_permissions)?;
    fs::set_permissions(&path, original_permissions)?;
    let actual_identities = workspace_entry_identities(suite.workspace.path())?;
    let expected_identity = suite.workspace_seed_entry_identities[relative];
    let actual_identity = actual_identities[relative];
    let prefix = WORKSPACE_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(
        workspace_entries(suite.workspace.path())?,
        suite.workspace_seed_entries
    );
    assert_eq!(
        workspace_modified_times(suite.workspace.path())?,
        suite.workspace_seed_modified_times
    );
    assert_eq!(actual_identity.device, expected_identity.device);
    assert_eq!(actual_identity.inode, expected_identity.inode);
    assert_ne!(actual_identity, expected_identity);
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_read_verifier_rejects_byte_identical_file_replacement() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    let target = suite.workspace.path().join(WORKSPACE_SEED_PATH);
    let replacement = suite.workspace.path().join("replacement-fixture");
    let target_modified = *suite
        .workspace_seed_modified_times
        .get(Path::new(WORKSPACE_SEED_PATH))
        .expect("the workspace seed file has a captured modified time");
    let root_modified = *suite
        .workspace_seed_modified_times
        .get(Path::new(""))
        .expect("the workspace root has a captured modified time");
    let permissions = fs::metadata(&target)?.permissions();
    fs::write(&replacement, WORKSPACE_SEED)?;
    fs::set_permissions(&replacement, permissions)?;
    fs::rename(&replacement, &target)?;
    fs::File::open(&target)?.set_times(fs::FileTimes::new().set_modified(target_modified))?;
    fs::File::open(suite.workspace.path())?
        .set_times(fs::FileTimes::new().set_modified(root_modified))?;
    let prefix = WORKSPACE_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(
        workspace_entries(suite.workspace.path())?,
        suite.workspace_seed_entries
    );
    assert_eq!(
        workspace_modified_times(suite.workspace.path())?,
        suite.workspace_seed_modified_times
    );
    assert_ne!(
        workspace_entry_identities(suite.workspace.path())?,
        suite.workspace_seed_entry_identities
    );
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_read_verifier_rejects_byte_identical_directory_replacement() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    let relative = Path::new(WORKSPACE_GLOB_DIRECTORY);
    let target = suite.workspace.path().join(relative);
    let moved = suite.workspace.path().join("moved-glob-scope");
    let target_modified = suite.workspace_seed_modified_times[relative];
    let root_modified = suite.workspace_seed_modified_times[Path::new("")];
    let permissions = fs::metadata(&target)?.permissions();
    let matching_name = Path::new(WORKSPACE_GLOB_PATH)
        .file_name()
        .expect("the matching fixture has a filename");
    let overflow_name = Path::new(WORKSPACE_GLOB_OVERFLOW_PATH)
        .file_name()
        .expect("the overflow fixture has a filename");
    let nonmatching_name = Path::new(WORKSPACE_GLOB_NONMATCHING_PATH)
        .file_name()
        .expect("the nonmatching fixture has a filename");
    fs::rename(&target, &moved)?;
    fs::create_dir(&target)?;
    fs::set_permissions(&target, permissions)?;
    fs::hard_link(moved.join(matching_name), target.join(matching_name))?;
    fs::hard_link(moved.join(overflow_name), target.join(overflow_name))?;
    fs::hard_link(moved.join(nonmatching_name), target.join(nonmatching_name))?;
    fs::remove_file(moved.join(matching_name))?;
    fs::remove_file(moved.join(overflow_name))?;
    fs::remove_file(moved.join(nonmatching_name))?;
    fs::remove_dir(moved)?;
    fs::File::open(&target)?.set_times(fs::FileTimes::new().set_modified(target_modified))?;
    fs::File::open(suite.workspace.path())?
        .set_times(fs::FileTimes::new().set_modified(root_modified))?;
    let prefix = WORKSPACE_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(
        workspace_entries(suite.workspace.path())?,
        suite.workspace_seed_entries
    );
    assert_eq!(
        workspace_modified_times(suite.workspace.path())?,
        suite.workspace_seed_modified_times
    );
    assert_ne!(
        workspace_entry_identities(suite.workspace.path())?,
        suite.workspace_seed_entry_identities
    );
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn workspace_inventory_rejects_a_symlink_replacing_its_root() -> EvalResult {
    let parent = tempfile::tempdir()?;
    let root = parent.path().join("workspace-root");
    let moved = parent.path().join("moved-workspace");
    fs::create_dir(&root)?;
    fs::write(root.join(WORKSPACE_SEED_PATH), WORKSPACE_SEED)?;
    let expected = workspace_entries(&root)?;
    fs::rename(&root, &moved)?;
    symlink(&moved, &root)?;

    assert_ne!(workspace_entries(&root)?, expected);
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
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
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
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
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
    let started = current_filesystem_recorded_time()?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_EDITED_SEED,
    )?;
    fs::File::open(suite.workspace.path())?
        .set_times(fs::FileTimes::new().set_modified(current_filesystem_recorded_time()?))?;
    suite.executor.record_filesystem_execution_window(
        EDIT_FILE_NAME,
        FilesystemExecutionTimeWindow {
            started,
            finished: current_filesystem_recorded_time()?,
        },
    );
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
fn forced_workspace_edit_verifier_accepts_atomic_parent_mtime_change() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == EDIT_FILE_NAME)
        .expect("the workspace edit fixture exists");
    let started = current_filesystem_recorded_time()?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_EDITED_SEED,
    )?;
    fs::File::open(suite.workspace.path())?
        .set_times(fs::FileTimes::new().set_modified(current_filesystem_recorded_time()?))?;
    suite.executor.record_filesystem_execution_window(
        EDIT_FILE_NAME,
        FilesystemExecutionTimeWindow {
            started,
            finished: current_filesystem_recorded_time()?,
        },
    );
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "replacements": EXPECTED_WORKSPACE_EDIT_REPLACEMENTS,
        "bytes_written": EXPECTED_WORKSPACE_EDIT_BYTES,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_edit_verifier_rejects_pre_execution_parent_mtime() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == EDIT_FILE_NAME)
        .expect("the workspace edit fixture exists");
    let started = current_filesystem_recorded_time()?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_EDITED_SEED,
    )?;
    fs::File::open(suite.workspace.path())?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    suite.executor.record_filesystem_execution_window(
        EDIT_FILE_NAME,
        FilesystemExecutionTimeWindow {
            started,
            finished: current_filesystem_recorded_time()?,
        },
    );
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "replacements": EXPECTED_WORKSPACE_EDIT_REPLACEMENTS,
        "bytes_written": EXPECTED_WORKSPACE_EDIT_BYTES,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_edit_verifier_rejects_a_pre_execution_target_mtime() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == EDIT_FILE_NAME)
        .expect("the workspace edit fixture exists");
    let target = suite.workspace.path().join(WORKSPACE_SEED_PATH);
    let started = current_filesystem_recorded_time()?;
    fs::write(&target, WORKSPACE_EDITED_SEED)?;
    suite.executor.record_filesystem_execution_window(
        EDIT_FILE_NAME,
        FilesystemExecutionTimeWindow {
            started,
            finished: current_filesystem_recorded_time()?,
        },
    );
    fs::File::open(target)?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "replacements": EXPECTED_WORKSPACE_EDIT_REPLACEMENTS,
        "bytes_written": EXPECTED_WORKSPACE_EDIT_BYTES,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
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
        forced_verification_failed: false,
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
                attempt_denied: false,
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
fn forced_exec_tier_reports_a_nonzero_process_result_as_infrastructure() {
    let mut execution = confined_exit(EXEC_FORCED_SANDBOXED_OUTPUT);
    execution["outcome"]["code"] = serde_json::json!(1);
    let outcome = forced_exec_outcome(SANDBOXED_EXEC_NAME, execution);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert_eq!(outcome.infrastructure_label(), "nonzero exit");
    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn forced_exec_tier_reports_a_timeout_as_infrastructure() {
    let outcome = forced_exec_outcome(
        SANDBOXED_EXEC_NAME,
        direct_exec_result(DirectExecEvidence::timed_out()),
    );

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert_eq!(outcome.infrastructure_label(), "timed out");
    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn unforced_exec_tier_rejects_an_additional_tool_call() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        forced_verification_failed: false,
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
                    attempt_denied: false,
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(CARGO_DIAGNOSTICS_NAME),
                    arguments_text: String::from("{}"),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                    attempt_succeeded: true,
                    attempt_denied: false,
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
    let mut result = cargo_diagnostics_result(CargoDiagnosticsExecution {
        confinement: ExecutionConfinement::FilesystemConfined,
        outcome: ProcessOutcome::Exited { code: Some(0) },
        stdout: stream,
        stderr: stream,
        cargo_failure: None,
        preparation_failure: None,
    });
    result["diagnostics"]["values"] = serde_json::json!([live_cargo_diagnostic()]);
    result[EVAL_RECEIPT_FIELD] = serde_json::json!(SYNTHETIC_EVAL_RECEIPT);
    result
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

fn live_cargo_diagnostic() -> CargoDiagnostic {
    CargoDiagnostic {
        file: Some(String::from(SYNTHETIC_CARGO_DIAGNOSTIC_FILE)),
        file_completeness: CaptureCompleteness::Complete,
        span: Some(CargoDiagnosticSpan {
            line_start: SYNTHETIC_CARGO_DIAGNOSTIC_LINE,
            column_start: SYNTHETIC_CARGO_DIAGNOSTIC_START_COLUMN,
            line_end: SYNTHETIC_CARGO_DIAGNOSTIC_LINE,
            column_end: SYNTHETIC_CARGO_DIAGNOSTIC_END_COLUMN,
        }),
        level: String::from(CARGO_WARNING_DIAGNOSTIC_LEVEL),
        level_completeness: CaptureCompleteness::Complete,
        message: String::from(LIVE_CARGO_DIAGNOSTIC_MESSAGE),
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

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_exec_tier_rejects_the_other_case_s_output() {
    let outcome = forced_exec_outcome(
        SANDBOXED_EXEC_NAME,
        confined_exit(EXEC_FORCED_READ_ONLY_OUTPUT),
    );

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_direct_exec_rejects_an_unknown_top_level_field() {
    let mut result = confined_exit(EXEC_FORCED_SANDBOXED_OUTPUT);
    result["unexpected"] = serde_json::json!("synthetic contradictory field");
    let outcome = forced_exec_outcome(SANDBOXED_EXEC_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn natural_direct_exec_rejects_an_unknown_stream_field() {
    let mut result = confined_exit(EXEC_NATURAL_OUTPUT);
    result["stdout"]["unexpected"] = serde_json::json!("synthetic contradictory field");
    let outcome = natural_exec_outcome(result);

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
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

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn forced_unsandboxed_exec_rejects_a_confined_zero_exit() {
    let outcome = forced_exec_outcome(
        UNSANDBOXED_EXEC_NAME,
        confined_exit(EXEC_FORCED_READ_ONLY_OUTPUT),
    );

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_an_unconfined_zero_exit() {
    let mut result = successful_cargo_diagnostics_result();
    result["execution"]["confinement"]["kind"] = serde_json::json!("unsandboxed");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_accepts_a_complete_successful_check() {
    let result = successful_cargo_diagnostics_result();
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Pass);
}

#[test]
fn forced_cargo_diagnostics_accepts_correlated_file_and_span() {
    let mut result = successful_cargo_diagnostics_result();
    let mut diagnostic = serde_json::to_value(live_cargo_diagnostic())
        .expect("producer Cargo diagnostics serialize");
    diagnostic["span"] = synthetic_cargo_diagnostic_span();
    result["diagnostics"]["values"] = serde_json::json!([diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Pass);
}

#[test]
fn forced_cargo_diagnostics_requires_the_live_fixture_warning() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["values"] = serde_json::json!([]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_a_duplicated_fixture_warning() {
    let mut result = successful_cargo_diagnostics_result();
    let diagnostic = serde_json::to_value(live_cargo_diagnostic())
        .expect("producer Cargo diagnostics serialize");
    result["diagnostics"]["values"] = serde_json::json!([diagnostic, diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_an_extra_recognized_diagnostic() {
    let mut result = successful_cargo_diagnostics_result();
    let fixture = serde_json::to_value(live_cargo_diagnostic())
        .expect("producer Cargo diagnostics serialize");
    let extra = serde_json::to_value(synthetic_cargo_diagnostic("note"))
        .expect("producer Cargo diagnostics serialize");
    result["diagnostics"]["values"] = serde_json::json!([fixture, extra]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_a_different_valid_fixture_span() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["values"][0]["span"]["column_end"] =
        serde_json::json!(SYNTHETIC_CARGO_DIAGNOSTIC_END_COLUMN + 1);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_a_marker_bearing_wrong_message() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["values"][0]["message"] =
        serde_json::json!("synthetic prefix: tool eval fixture diagnostic");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_error_diagnostics_from_a_successful_check() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["values"] = serde_json::to_value(vec![synthetic_cargo_diagnostic(
        CARGO_ERROR_DIAGNOSTIC_LEVEL,
    )])
    .expect("producer Cargo diagnostics serialize");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_an_unknown_diagnostic_level() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["values"] =
        serde_json::to_value(vec![synthetic_cargo_diagnostic("fatal")])
            .expect("producer Cargo diagnostics serialize");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_an_unknown_top_level_field() {
    let mut result = successful_cargo_diagnostics_result();
    result["unexpected"] = serde_json::json!("synthetic field");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_an_unknown_span_field() {
    let mut result = successful_cargo_diagnostics_result();
    let mut diagnostic =
        serde_json::to_value(synthetic_cargo_diagnostic(CARGO_WARNING_DIAGNOSTIC_LEVEL))
            .expect("producer Cargo diagnostics serialize");
    diagnostic["file"] = serde_json::json!(SYNTHETIC_CARGO_DIAGNOSTIC_FILE);
    let mut span = synthetic_cargo_diagnostic_span();
    span["unexpected"] = serde_json::json!("synthetic field");
    diagnostic["span"] = span;
    result["diagnostics"]["values"] = serde_json::json!([diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_malformed_diagnostics() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["values"] = serde_json::json!([{}]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
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

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
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

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
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

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
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

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_a_truncated_present_file() {
    let mut result = successful_cargo_diagnostics_result();
    let mut diagnostic =
        serde_json::to_value(synthetic_cargo_diagnostic(CARGO_WARNING_DIAGNOSTIC_LEVEL))
            .expect("producer Cargo diagnostics serialize");
    diagnostic["file"] = serde_json::json!(SYNTHETIC_CARGO_DIAGNOSTIC_FILE);
    diagnostic["file_completeness"] = serde_json::json!("truncated");
    diagnostic["span"] = synthetic_cargo_diagnostic_span();
    result["diagnostics"]["values"] = serde_json::json!([diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_a_truncated_message() {
    let mut result = successful_cargo_diagnostics_result();
    let mut diagnostic =
        serde_json::to_value(synthetic_cargo_diagnostic(CARGO_WARNING_DIAGNOSTIC_LEVEL))
            .expect("producer Cargo diagnostics serialize");
    diagnostic["message_completeness"] = serde_json::json!("truncated");
    result["diagnostics"]["values"] = serde_json::json!([diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_a_truncated_stdout_capture() {
    let mut result = successful_cargo_diagnostics_result();
    result["execution"]["stdout"]["completeness"] = serde_json::json!("truncated");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_lossy_stderr_capture() {
    let mut result = successful_cargo_diagnostics_result();
    result["execution"]["stderr"]["encoding"] = serde_json::json!("lossy_utf8");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_capped_records() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["limit_reached"] = serde_json::json!(true);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_workspace_verification_failure_is_infrastructure() {
    let mut outcome = forced_exec_outcome(
        CARGO_DIAGNOSTICS_NAME,
        successful_cargo_diagnostics_result(),
    );
    outcome.execution_completed = false;
    outcome.forced_verification_failed = true;

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert_eq!(outcome.infrastructure_label(), "exact state mismatch");
    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn forced_cargo_diagnostics_rejects_known_truncated_test_records() {
    let mut result = successful_cargo_diagnostics_result();
    result["tests"]["known_truncated"] = serde_json::json!(true);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
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

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
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
fn forced_cargo_diagnostics_reports_a_cargo_failure_as_infrastructure() {
    let stream = CargoDiagnosticsStream {
        completeness: CaptureCompleteness::Complete,
        encoding: OutputEncoding::Utf8,
    };
    let result = cargo_diagnostics_result(CargoDiagnosticsExecution {
        confinement: ExecutionConfinement::FilesystemConfined,
        outcome: ProcessOutcome::Exited { code: Some(1) },
        stdout: stream,
        stderr: stream,
        cargo_failure: Some(CargoFailureDetail {
            message: String::from(SYNTHETIC_CARGO_FAILURE),
            message_completeness: CaptureCompleteness::Complete,
        }),
        preparation_failure: None,
    });
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert_eq!(outcome.infrastructure_label(), "Cargo failure");
    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn forced_cargo_diagnostics_rejects_an_incomplete_result_shape() {
    let outcome = forced_exec_outcome(
        CARGO_DIAGNOSTICS_NAME,
        serde_json::json!({
            "execution": confined_exit(""),
        }),
    );

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_exec_tier_rejects_a_truncated_output_capture() {
    let outcome = forced_exec_outcome(
        UNSANDBOXED_EXEC_NAME,
        direct_exec_result(DirectExecEvidence::unsandboxed_truncated(
            EXEC_FORCED_READ_ONLY_OUTPUT,
        )),
    );

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
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

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_exec_tier_rejects_a_truncated_stderr_capture() {
    let mut result = direct_exec_result(DirectExecEvidence::successful_with_confinement(
        ExecutionConfinement::Unsandboxed,
        EXEC_FORCED_READ_ONLY_OUTPUT,
    ));
    result["stderr"]["completeness"] = serde_json::json!("truncated");
    let outcome = forced_exec_outcome(UNSANDBOXED_EXEC_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_exec_tier_rejects_a_lossy_stderr_capture() {
    let mut result = direct_exec_result(DirectExecEvidence::successful_with_confinement(
        ExecutionConfinement::Unsandboxed,
        EXEC_FORCED_READ_ONLY_OUTPUT,
    ));
    result["stderr"]["encoding"] = serde_json::json!("lossy_utf8");
    let outcome = forced_exec_outcome(UNSANDBOXED_EXEC_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_exec_tier_rejects_a_nonempty_stderr_capture() {
    let mut result = direct_exec_result(DirectExecEvidence::successful_with_confinement(
        ExecutionConfinement::Unsandboxed,
        EXEC_FORCED_READ_ONLY_OUTPUT,
    ));
    result["stderr"]["text"] = serde_json::json!(SYNTHETIC_EXECUTOR_FAILURE);
    let outcome = forced_exec_outcome(UNSANDBOXED_EXEC_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

/// One unforced Exec outcome whose sole request carries the supplied execution.
fn natural_exec_outcome(execution: serde_json::Value) -> CaseOutcome {
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

#[test]
fn unforced_exec_tier_passes_a_confined_zero_exit() {
    let outcome = natural_exec_outcome(confined_exit(EXEC_NATURAL_OUTPUT));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Pass
    );
}

#[test]
fn unforced_exec_tier_rejects_an_exact_state_mismatch_as_infrastructure() {
    let outcome = natural_exec_outcome(confined_exit(EXEC_NATURAL_OUTPUT));

    assert_eq!(
        outcome.natural_infrastructure_label(EvalFamily::Exec, EvalDisposition::Miss),
        "exact state mismatch"
    );
    assert!(
        reject_natural_executor_failure(&outcome, EvalFamily::Exec, EvalDisposition::Miss).is_err()
    );
}

#[test]
fn unforced_exec_tier_rejects_a_truncated_capture() {
    let mut execution = confined_exit(EXEC_NATURAL_OUTPUT);
    execution["stdout"]["completeness"] = serde_json::json!("truncated");
    let outcome = natural_exec_outcome(execution);

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_exec_tier_rejects_a_lossy_capture() {
    let mut execution = confined_exit(EXEC_NATURAL_OUTPUT);
    execution["stderr"]["encoding"] = serde_json::json!("lossy_utf8");
    let outcome = natural_exec_outcome(execution);

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_exec_tier_reports_a_timed_out_process_as_infrastructure() {
    let outcome = natural_exec_outcome(direct_exec_result(DirectExecEvidence::timed_out()));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
    assert!(
        reject_natural_executor_failure(&outcome, EvalFamily::Exec, EvalDisposition::Pass).is_err()
    );
}

#[test]
fn unforced_exec_tier_reports_a_nonzero_exit_as_infrastructure() {
    let outcome = natural_exec_outcome(direct_exec_result(DirectExecEvidence::nonzero_exit()));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
    assert!(
        reject_natural_executor_failure(&outcome, EvalFamily::Exec, EvalDisposition::Pass).is_err()
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
fn unforced_exec_structured_infrastructure_fails_the_job() {
    let outcome =
        natural_exec_outcome(direct_exec_result(DirectExecEvidence::supervision_failure()));

    assert!(
        reject_natural_executor_failure(&outcome, EvalFamily::Exec, EvalDisposition::Pass).is_err()
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
        forced_verification_failed: false,
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
    assert_eq!(outcome.infrastructure_label(), "sandbox setup failed");
}

#[test]
fn forced_sandboxed_exec_setup_failure_fails_the_job() {
    let outcome = forced_exec_outcome(
        SANDBOXED_EXEC_NAME,
        direct_exec_result(DirectExecEvidence::sandbox_setup_failure()),
    );

    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn forced_exec_denied_sole_exact_request_is_infrastructure() {
    let mut outcome = forced_exec_outcome(
        UNSANDBOXED_EXEC_NAME,
        zero_exit_with_confinement(ZeroExitEvidence {
            confinement: ExecutionConfinement::Unsandboxed,
            stdout: EXEC_FORCED_READ_ONLY_OUTPUT,
        }),
    );
    outcome.snapshot.requests[0].completed_result_entry_index = None;
    outcome.snapshot.requests[0].attempt_succeeded = false;
    outcome.snapshot.requests[0].attempt_denied = true;
    outcome.tool_results.clear();

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn forced_exec_denied_exact_retry_remains_a_report_only_miss() {
    let mut outcome = forced_exec_outcome(
        UNSANDBOXED_EXEC_NAME,
        zero_exit_with_confinement(ZeroExitEvidence {
            confinement: ExecutionConfinement::Unsandboxed,
            stdout: EXEC_FORCED_READ_ONLY_OUTPUT,
        }),
    );
    let fixture = forced_exec_fixture(UNSANDBOXED_EXEC_NAME);
    outcome.snapshot.requests.push(RequestSnapshot {
        request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
        producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
        name: String::from(UNSANDBOXED_EXEC_NAME),
        arguments_text: String::from(fixture.expected_arguments),
        entry_index: ARBITRARY_LATE_RESULT_ENTRY_INDEX,
        completed_result_entry_index: None,
        attempt_succeeded: false,
        attempt_denied: true,
    });

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
    assert!(reject_forced_executor_failures(&[outcome]).is_ok());
}

#[test]
fn unforced_exec_tier_rejects_an_unconfined_execution() {
    let outcome = natural_exec_outcome(zero_exit_with_confinement(ZeroExitEvidence {
        confinement: ExecutionConfinement::Unsandboxed,
        stdout: "",
    }));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
    assert!(
        reject_natural_executor_failure(&outcome, EvalFamily::Exec, EvalDisposition::Pass).is_err()
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

fn record_workspace_read_result(tracker: &OperationTracker, content: &str, truncated: bool) {
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
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(READ_FILE_NAME),
                arguments_text: serde_json::json!({"path": WORKSPACE_SEED_PATH}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
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
                attempt_denied: false,
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
                attempt_denied: false,
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
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_THIRD_EVAL_REQUEST_ID),
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
                attempt_denied: false,
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
    let started = current_filesystem_recorded_time()?;
    fs::write(&answer, WORKSPACE_ANSWER)?;
    fs::set_permissions(
        &answer,
        fs::Permissions::from_mode(WORKSPACE_PRIVATE_CREATION_MODE),
    )?;
    suite.executor.record_filesystem_execution_window(
        WRITE_FILE_NAME,
        FilesystemExecutionTimeWindow {
            started,
            finished: current_filesystem_recorded_time()?,
        },
    );
    let snapshot = successful_workspace_natural_snapshot();

    assert!(suite.natural_state_passed(&snapshot)?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn workspace_existing_mutation_target_rejects_inode_flag_drift() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let target = Path::new(WORKSPACE_SEED_PATH);
    let file = fs::File::open(suite.workspace.path().join(target))?;
    let flags = rustix::fs::ioctl_getflags(&file)?;
    rustix::fs::ioctl_setflags(&file, flags | rustix::fs::IFlags::NOATIME)?;

    assert!(!workspace_inode_flags_match_for_mutation(
        suite.workspace.path(),
        &suite.workspace_seed_inode_flags,
        target,
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn workspace_created_mutation_target_rejects_inode_flag_drift() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let target = Path::new("written.txt");
    fs::write(
        suite.workspace.path().join(target),
        "synthetic created file\n",
    )?;
    let file = fs::File::open(suite.workspace.path().join(target))?;
    let flags = rustix::fs::ioctl_getflags(&file)?;
    rustix::fs::ioctl_setflags(&file, flags | rustix::fs::IFlags::NOATIME)?;

    assert!(!workspace_inode_flags_match_for_mutation(
        suite.workspace.path(),
        &suite.workspace_seed_inode_flags,
        target,
    )?);
    Ok(())
}

#[test]
fn workspace_natural_state_rejects_pre_execution_parent_mtime() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let answer = suite.workspace.path().join(WORKSPACE_ANSWER_PATH);
    let started = current_filesystem_recorded_time()?;
    fs::write(&answer, WORKSPACE_ANSWER)?;
    fs::File::open(suite.workspace.path())?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    suite.executor.record_filesystem_execution_window(
        WRITE_FILE_NAME,
        FilesystemExecutionTimeWindow {
            started,
            finished: current_filesystem_recorded_time()?,
        },
    );
    let snapshot = successful_workspace_natural_snapshot();

    assert!(!suite.natural_state_passed(&snapshot)?);
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
            "offset": 0,
            "bytes_read": WORKSPACE_SEED.len(),
            "next_offset": WORKSPACE_SEED.len(),
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

type PreparedExecWorkspace = (
    TempDir,
    BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    BTreeMap<PathBuf, SystemTime>,
    BTreeMap<PathBuf, FilesystemIdentity>,
);

type PreparedExecNaturalWorkspace = (
    TempDir,
    BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    BTreeMap<PathBuf, SystemTime>,
    BTreeMap<PathBuf, FilesystemIdentity>,
    BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
    BTreeMap<PathBuf, u32>,
    FilesystemExecutionTimeWindow,
);

fn prepared_exec_seed_workspace() -> EvalResult<PreparedExecWorkspace> {
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

fn prepared_exec_natural_workspace() -> EvalResult<PreparedExecNaturalWorkspace> {
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

fn create_cargo_target_directory(root: &Path) -> EvalResult<FilesystemExecutionTimeWindow> {
    let started = current_filesystem_recorded_time()?;
    let target = root.join("target");
    fs::create_dir(&target)?;
    #[cfg(unix)]
    fs::set_permissions(
        target,
        fs::Permissions::from_mode(CARGO_TARGET_DIRECTORY_MODE),
    )?;
    Ok(FilesystemExecutionTimeWindow {
        started,
        finished: current_filesystem_recorded_time()?,
    })
}

#[cfg(unix)]
#[test]
fn exec_natural_created_file_gate_rejects_changed_ownership() -> EvalResult {
    let (
        workspace,
        _entries,
        _times,
        expected_identities,
        _attributes,
        _inode_flags,
        _execution_window,
    ) = prepared_exec_natural_workspace()?;
    let target = Path::new(EXEC_RESULT_PATH);
    let mut actual_identities = workspace_entry_identities(workspace.path())?;
    let changed_group_id = actual_identities[target].group_id.wrapping_add(1);
    actual_identities
        .get_mut(target)
        .expect("the Exec result has a filesystem identity")
        .group_id = changed_group_id;

    assert!(!created_entry_identity_matches_workspace(
        &actual_identities,
        &expected_identities,
        target,
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn exec_natural_created_file_gate_rejects_a_different_device() -> EvalResult {
    let (
        workspace,
        _entries,
        _times,
        expected_identities,
        _attributes,
        _inode_flags,
        _execution_window,
    ) = prepared_exec_natural_workspace()?;
    let target = Path::new(EXEC_RESULT_PATH);
    let mut actual_identities = workspace_entry_identities(workspace.path())?;
    actual_identities
        .get_mut(target)
        .expect("the Exec result has a filesystem identity")
        .device = expected_identities[Path::new("")].device.wrapping_add(1);

    assert!(!created_entry_identity_matches_workspace(
        &actual_identities,
        &expected_identities,
        target,
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn exec_natural_created_file_gate_rejects_collateral_write_permissions() -> EvalResult {
    let (workspace, entries, times, identities, attributes, inode_flags, execution_window) =
        prepared_exec_natural_workspace()?;
    fs::set_permissions(
        workspace.path().join(EXEC_RESULT_PATH),
        fs::Permissions::from_mode(EXEC_PERMISSIVE_CREATION_MODE),
    )?;

    assert!(!exec_natural_entries_match(
        workspace.path(),
        &entries,
        &times,
        &identities,
        &attributes,
        &inode_flags,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
fn replace_exec_seed_file_byte_identically(
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

#[test]
fn forced_direct_exec_workspace_accepts_the_unchanged_seed() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;

    assert!(exec_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &workspace_inode_flags(workspace.path())?,
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn forced_direct_exec_workspace_rejects_inode_flag_drift() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let seed_inode_flags = workspace_inode_flags(workspace.path())?;
    let seed = fs::File::open(workspace.path().join("Cargo.toml"))?;
    let flags = rustix::fs::ioctl_getflags(&seed)?;
    rustix::fs::ioctl_setflags(&seed, flags | rustix::fs::IFlags::NOATIME)?;

    assert!(!exec_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &seed_inode_flags,
    )?);
    Ok(())
}

#[test]
fn forced_direct_exec_workspace_rejects_a_mutated_seed_file() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    fs::write(workspace.path().join("src/lib.rs"), "pub fn drifted() {}\n")?;

    assert!(!exec_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &workspace_inode_flags(workspace.path())?,
    )?);
    Ok(())
}

#[test]
fn forced_direct_exec_workspace_rejects_a_collateral_path() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    fs::write(
        workspace.path().join("collateral.txt"),
        "collateral fixture\n",
    )?;

    assert!(!exec_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &workspace_inode_flags(workspace.path())?,
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_direct_exec_workspace_rejects_byte_identical_seed_replacement() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    replace_exec_seed_file_byte_identically(workspace.path(), &seed_modified_times)?;

    assert_eq!(workspace_entries(workspace.path())?, seed_entries);
    assert_eq!(
        workspace_modified_times(workspace.path())?,
        seed_modified_times
    );
    assert_ne!(
        workspace_entry_identities(workspace.path())?,
        seed_entry_identities
    );
    assert!(!exec_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &workspace_inode_flags(workspace.path())?,
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn forced_direct_exec_workspace_rejects_extended_attribute_drift() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    rustix::fs::setxattr(
        workspace.path().join("Cargo.toml"),
        SYNTHETIC_UNEXPECTED_XATTR_NAME,
        SYNTHETIC_UNEXPECTED_XATTR_VALUE,
        rustix::fs::XattrFlags::CREATE,
    )?;

    assert!(!exec_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &workspace_inode_flags(workspace.path())?,
    )?);
    Ok(())
}

#[test]
fn forced_cargo_diagnostics_workspace_accepts_the_exact_target_directory() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;

    assert!(cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn forced_cargo_diagnostics_workspace_rejects_target_inode_flag_drift() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let seed_inode_flags = workspace_inode_flags(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;
    let target = fs::File::open(workspace.path().join("target"))?;
    let flags = rustix::fs::ioctl_getflags(&target)?;
    rustix::fs::ioctl_setflags(&target, flags | rustix::fs::IFlags::NOATIME)?;

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &seed_inode_flags,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn forced_cargo_diagnostics_workspace_rejects_an_unexpected_target_descendant() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;
    fs::write(
        workspace.path().join("target/unexpected"),
        "synthetic unexpected target descendant\n",
    )?;

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_cargo_diagnostics_workspace_rejects_a_writable_target_directory() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let started = current_filesystem_recorded_time()?;
    fs::create_dir(workspace.path().join("target"))?;
    fs::set_permissions(
        workspace.path().join("target"),
        fs::Permissions::from_mode(WORKSPACE_INSECURE_CREATION_MODE),
    )?;
    let execution_window = FilesystemExecutionTimeWindow {
        started,
        finished: current_filesystem_recorded_time()?,
    };

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_cargo_diagnostics_workspace_rejects_a_hard_linked_target_artifact() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let support = tempfile::tempdir()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let artifact = workspace.path().join("target/artifact");
    let started = current_filesystem_recorded_time()?;
    fs::create_dir(workspace.path().join("target"))?;
    fs::write(&artifact, "synthetic target artifact\n")?;
    fs::hard_link(&artifact, support.path().join("artifact-link"))?;
    let execution_window = FilesystemExecutionTimeWindow {
        started,
        finished: current_filesystem_recorded_time()?,
    };

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_cargo_diagnostics_workspace_rejects_a_symlinked_target_artifact() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;
    symlink(
        workspace.path().join("Cargo.toml"),
        workspace.path().join("target/escape"),
    )?;

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn forced_cargo_diagnostics_workspace_rejects_a_mutated_seed_file() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;
    fs::write(workspace.path().join("src/lib.rs"), "pub fn drifted() {}\n")?;

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn forced_cargo_diagnostics_workspace_rejects_a_deleted_seed_file() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;
    fs::remove_file(workspace.path().join("Cargo.toml"))?;

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_cargo_diagnostics_rejects_byte_identical_seed_replacement() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;
    replace_exec_seed_file_byte_identically(workspace.path(), &seed_modified_times)?;

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn forced_cargo_diagnostics_rejects_root_extended_attribute_drift() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;
    rustix::fs::setxattr(
        workspace.path(),
        SYNTHETIC_UNEXPECTED_XATTR_NAME,
        SYNTHETIC_UNEXPECTED_XATTR_VALUE,
        rustix::fs::XattrFlags::CREATE,
    )?;

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn forced_cargo_diagnostics_rejects_target_extended_attributes() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let started = current_filesystem_recorded_time()?;
    fs::create_dir(workspace.path().join("target"))?;
    rustix::fs::setxattr(
        workspace.path().join("target"),
        SYNTHETIC_UNEXPECTED_XATTR_NAME,
        SYNTHETIC_UNEXPECTED_XATTR_VALUE,
        rustix::fs::XattrFlags::CREATE,
    )?;
    let execution_window = FilesystemExecutionTimeWindow {
        started,
        finished: current_filesystem_recorded_time()?,
    };

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn cargo_target_identity_gate_rejects_changed_identity() -> EvalResult {
    let (workspace, _seed_entries, _seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let _execution_window = create_cargo_target_directory(workspace.path())?;
    let entries = workspace_entries(workspace.path())?;
    let mut identities = workspace_entry_identities(workspace.path())?;
    identities
        .get_mut(Path::new("target"))
        .expect("the Cargo target fixture has a filesystem identity")
        .user_id = seed_entry_identities[Path::new("")].user_id.wrapping_add(1);

    assert!(!cargo_target_identities_match(
        &entries,
        &identities,
        seed_entry_identities.get(Path::new("")),
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn cargo_target_identity_gate_rejects_a_different_device() -> EvalResult {
    let (workspace, _seed_entries, _seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let _execution_window = create_cargo_target_directory(workspace.path())?;
    let entries = workspace_entries(workspace.path())?;
    let mut identities = workspace_entry_identities(workspace.path())?;
    identities
        .get_mut(Path::new("target"))
        .expect("the Cargo target fixture has a filesystem identity")
        .device = seed_entry_identities[Path::new("")].device.wrapping_add(1);

    assert!(!cargo_target_identities_match(
        &entries,
        &identities,
        seed_entry_identities.get(Path::new("")),
    ));
    Ok(())
}

#[test]
fn forced_cargo_diagnostics_rejects_out_of_window_target_times() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;
    fs::File::open(workspace.path().join("target"))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn exec_natural_state_accepts_only_the_requested_output_addition() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;

    assert!(exec_natural_entries_match(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &seed_inode_flags,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn exec_natural_state_rejects_output_inode_flag_drift() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    let output = fs::File::open(workspace.path().join(EXEC_RESULT_PATH))?;
    let flags = rustix::fs::ioctl_getflags(&output)?;
    rustix::fs::ioctl_setflags(&output, flags | rustix::fs::IFlags::NOATIME)?;

    assert!(!exec_natural_entries_match(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &seed_inode_flags,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn exec_natural_state_rejects_seed_inode_flag_drift() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    let seed = fs::File::open(workspace.path().join("Cargo.toml"))?;
    let flags = rustix::fs::ioctl_getflags(&seed)?;
    rustix::fs::ioctl_setflags(&seed, flags | rustix::fs::IFlags::NOATIME)?;

    assert!(!exec_natural_entries_match(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &seed_inode_flags,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn exec_natural_state_rejects_out_of_window_output_times() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    fs::File::open(workspace.path().join(EXEC_RESULT_PATH))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;

    assert!(!exec_natural_entries_match(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &seed_inode_flags,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn exec_natural_state_rejects_out_of_window_parent_times() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    fs::File::open(workspace.path())?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;

    assert!(!exec_natural_entries_match(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &seed_inode_flags,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn exec_natural_state_rejects_a_mutated_seed_file() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"collateral-mutation\"\n",
    )?;

    assert!(!exec_natural_entries_match(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &seed_inode_flags,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn exec_natural_state_rejects_a_collateral_addition() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    fs::write(
        workspace.path().join("collateral.txt"),
        "collateral fixture\n",
    )?;

    assert!(!exec_natural_entries_match(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &seed_inode_flags,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn exec_natural_state_rejects_an_oversized_sparse_collateral_file() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    fs::File::create(workspace.path().join("oversized-collateral.txt"))?
        .set_len((MAX_WORKSPACE_READ_BYTES + 1) as u64)?;

    assert!(!exec_natural_entries_match(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &seed_inode_flags,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn exec_natural_state_rejects_root_extended_attribute_drift() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    let root = Path::new("");
    rustix::fs::setxattr(
        workspace.path(),
        SYNTHETIC_UNEXPECTED_XATTR_NAME,
        SYNTHETIC_UNEXPECTED_XATTR_VALUE,
        rustix::fs::XattrFlags::CREATE,
    )?;

    assert_ne!(
        workspace_extended_attributes(workspace.path())?[root],
        seed_extended_attributes[root]
    );
    assert!(!exec_natural_entries_match(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &seed_inode_flags,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn exec_natural_state_rejects_byte_identical_seed_replacement() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    replace_exec_seed_file_byte_identically(workspace.path(), &seed_modified_times)?;

    assert!(!exec_natural_entries_match(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &seed_inode_flags,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn exec_result_inspection_rejects_a_directory() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    fs::remove_file(workspace.path().join(EXEC_RESULT_PATH))?;
    fs::create_dir(workspace.path().join(EXEC_RESULT_PATH))?;

    assert!(!exec_natural_entries_match(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &seed_inode_flags,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn exec_result_inspection_rejects_a_fifo_without_opening_it() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    let result = workspace.path().join(EXEC_RESULT_PATH);
    fs::remove_file(&result)?;
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &result,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )?;

    assert!(!exec_natural_entries_match(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &seed_inode_flags,
        Some(execution_window),
    )?);
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
                attempt_denied: false,
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
                attempt_denied: false,
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
                attempt_denied: false,
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
                attempt_denied: false,
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
                attempt_denied: false,
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
                attempt_denied: false,
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
                attempt_denied: false,
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
                attempt_denied: false,
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
                attempt_denied: false,
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
                attempt_denied: false,
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
                attempt_denied: false,
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
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_FETCH_NAME),
                arguments_text: serde_json::json!({"url": WEB_URL}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
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
                attempt_denied: false,
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
                attempt_denied: false,
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
                attempt_denied: false,
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

fn log_exec_infrastructure_evidence(outcome: &CaseOutcome) {
    for result in &outcome.tool_results {
        if exec_result_is_infrastructure(result) {
            eprintln!(
                "structured Exec infrastructure evidence: {}",
                result.content
            );
        }
    }
}
