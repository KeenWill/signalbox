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
    ExecExecutor, ExecResult, ExecutionConfinement, OutputCapture, OutputEncoding, ProcessOutcome,
    ProcessSpawnFailure, ProcessSupervisionFailure, SANDBOXED_EXEC_NAME, SandboxedCommandRunner,
    SandboxedExecTool, TokioProcessRunner, UNSANDBOXED_EXEC_NAME, UnsandboxedCommandRunner,
    UnsandboxedExecTool,
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
use sqlx::{PgPool, types::Uuid};
use tempfile::TempDir;
use tokio::{sync::Mutex, time::timeout};

mod family;
mod fixtures;
mod report;

use family::cargo::*;
use family::exec::*;
use family::git::*;
use family::web::*;
use family::workspace::*;
use fixtures::*;
use report::*;

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
