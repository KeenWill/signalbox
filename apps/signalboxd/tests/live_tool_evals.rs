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

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use git2::{IndexAddOption, Oid, Repository, Signature, Status};
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
    DangerousToolAutoApproval, DecideToolRequest, DecideToolRequestResult, DeliveryRequest,
    DirectModelSelection, DurableCommandId, ModelCallId, ModelSelectionOverride,
    ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition, NormalizedToolArguments,
    PerInputConfigurationChoices, ProviderModelIdentity, ResolvedProviderTarget,
    SemanticTranscriptEntryId, SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
    SessionId, SubmitInputAppliedResult, SubmitInputResult, ToolApprovalDecision, ToolAttemptId,
    ToolName as DomainToolName, ToolRequestId, TurnAttemptId, TurnId, UserContent,
};
use signalbox_model_provider_runtime::{
    RuntimeModelCallProvider, RuntimeModelCatalog, RuntimeModelDefinition,
};
use signalbox_model_runtime::{
    CancellationSignal, CredentialAccess, CredentialAccessError, CredentialAccessFailure,
    CredentialReference, CredentialValue, MessagePart, ModelOperation, ModelRuntime,
    ObservationSink, PreparationOutcome, TerminalReport, ToolChoice, ToolName as RuntimeToolName,
};
use signalbox_model_runtime_openai::{OpenAiConfig, OpenAiPreparedRequest, OpenAiRuntime};
use signalbox_persistence::{
    SessionCredentialPin, SessionModelCredential, local_test_connection_options, migrate,
    model_execution::PostgresModelCallRepository,
    process_read::{
        ProcessReadRepository, ProcessToolExecutionResultDisposition, ProcessTranscriptEntry,
        ProcessTurnState,
    },
    scheduler::PostgresEligibilitySweep,
    start_eligible_turn::StartEligibleTurnRepository,
    submit_input::SubmitInputRepository,
    tool_loop::PostgresToolLoopRepository,
};
use signalbox_tools_exec::{
    CARGO_DIAGNOSTICS_NAME, CargoDiagnosticsExecutor, CargoDiagnosticsTool, ExecExecutor,
    SANDBOXED_EXEC_NAME, SandboxedCommandRunner, SandboxedExecTool, TokioProcessRunner,
    UNSANDBOXED_EXEC_NAME, UnsandboxedCommandRunner, UnsandboxedExecTool,
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
    WebSearchTool, WebSearchTransport, WebSearchTransportOutcome,
};
use signalbox_tools_workspace::{
    APPLY_PATCH_NAME, EDIT_FILE_NAME, GLOB_FILES_NAME, LIST_DIRECTORY_NAME,
    LocalWorkspaceFileSystem, READ_FILE_NAME, SEARCH_FILES_NAME, WRITE_FILE_NAME,
    WorkspaceMutationExecutor, WorkspaceMutationTools, WorkspaceReadExecutor, WorkspaceReadTools,
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
const GIT_MODEL: &str = "gpt-5-mini";
const MAX_OUTPUT_TOKENS: u32 = 1_024;
const CONTEXT_WINDOW_TOKENS: u32 = 200_000;
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const TURN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const LIVE_EVAL_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;
const GIT_AUTHOR_NAME: &str = "Signalbox Tool Eval";
const GIT_AUTHOR_EMAIL: &str = "signalbox-tool-eval@example.test";
const GIT_SEED_PATH: &str = "seed.txt";
const GIT_STAGE_PATH: &str = "stage-me.txt";
const GIT_COMMIT_PATH: &str = "commit-me.txt";
const GIT_NATURAL_PATH: &str = "eval.txt";
const GIT_NATURAL_MESSAGE: &str = "tool eval commit";
const WORKSPACE_SEED_PATH: &str = "brief.txt";
const WORKSPACE_ANSWER_PATH: &str = "answer.txt";
const WORKSPACE_ANSWER: &str = "model loop observed\n";
const EXEC_SUPERVISOR_VARIABLE: &str = "SIGNALBOX_EXEC_SUPERVISOR";
const EXEC_RESULT_PATH: &str = "exec-result.txt";
const EXEC_RESULT: &str = "model loop observed\n";
const EXEC_FORCED_SANDBOXED_ARGUMENTS: &str = r#"{"program":"printf","arguments":["forced sandboxed eval\n"],"working_directory":".","timeout_seconds":30}"#;
const EXEC_FORCED_SANDBOXED_OUTPUT: &str = "forced sandboxed eval\n";
const EXEC_FORCED_READ_ONLY_PROGRAM: &str = "/usr/bin/printf";
const EXEC_FORCED_READ_ONLY_OUTPUT: &str = "forced unsandboxed eval\n";
const EXEC_NATURAL_PROGRAM: &str = "/bin/sh";
const EXEC_NATURAL_SCRIPT: &str = "printf 'model loop observed\\n' > exec-result.txt";
const WEB_ORIGIN: &str = "https://example.com";
const WEB_URL: &str = "https://example.com/eval";
const SYNTHETIC_WEB_CREDENTIAL: &[u8] = b"synthetic-web-eval-key";
const ARBITRARY_EVAL_SELECTION_ID: u128 = 0x9101;
const ARBITRARY_EVAL_PROVIDER_ID: u128 = 0x9102;
const ARBITRARY_EVAL_REQUEST_ID: u128 = 0x9103;
const ARBITRARY_EVAL_ATTEMPT_ID: u128 = 0x9104;
const ARBITRARY_EVAL_ENTRY_ID: u128 = 0x9105;
const ARBITRARY_EVAL_TURN_ATTEMPT_ID: u128 = 0x9106;
const ARBITRARY_EVAL_SESSION_ID: u128 = 0x9107;
const ARBITRARY_EVAL_MODEL_CALL_ID: u128 = 0x9108;
const ARBITRARY_EVAL_APPROVAL_COMMAND_ID: u128 = 0x9109;
const ARBITRARY_SECOND_EVAL_REQUEST_ID: u128 = 0x910a;
const ARBITRARY_SECOND_EVAL_MODEL_CALL_ID: u128 = 0x910b;
const MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP: i64 = 2;

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
                .map_err(|error| error.to_string())?;
            runtime
                .block_on(run_selected_family_if_enabled())
                .map_err(|error| error.to_string())
        })?
        .join()
        .map_err(|_| io::Error::other("the live tool eval thread panicked"))?;
    outcome.map_err(|error| io::Error::other(error).into())
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
    let (session, turn, activated) = database.start_turn(prompt).await?;
    let tracker = OperationTracker::default();
    let runtime = EvalOpenAiRuntime::new(forced_tool, tracker.clone())?;
    let provider = RuntimeModelCallProvider::new(runtime, database.runtime_models.clone());
    let execution = PostgresProviderModelExecution::new(
        PostgresModelCallRepository::new(
            database.pool.clone(),
            database.targets.clone(),
            ModelCallCredentialReference::new("openai-tool-eval"),
        ),
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
        .map_err(|_| io::Error::other("the daemon tool eval turn exceeded its timeout"))?
        .map_err(|_| io::Error::other("the daemon tool eval turn execution failed"))?;
    if forced_tool == Some(UNSANDBOXED_EXEC_NAME)
        && database
            .approve_exact_read_only_unsandboxed_request(session, turn)
            .await?
    {
        timeout(TURN_TIMEOUT, execution.resume_active(session))
            .await
            .map_err(|_| io::Error::other("the daemon tool eval resume exceeded its timeout"))?
            .map_err(|_| io::Error::other("the daemon tool eval turn resume failed"))?;
    }
    let snapshot = CaseSnapshot::read(&database.pool, session, turn).await?;
    Ok(CaseOutcome {
        target: forced_tool.map(str::to_owned),
        expected_arguments: forced_case
            .map(|case| normalized_arguments_text(case.expected_arguments))
            .transpose()?,
        execution_completed: true,
        tool_results: tracker.tool_results(),
        snapshot,
    })
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
            Self::Git => GIT_MODEL,
            Self::Workspace | Self::Web | Self::Exec => DEFAULT_MODEL,
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
        expected_arguments: r#"{"name":"created-by-eval","start":"HEAD"}"#,
        prompt: "Call git_branch_create with exactly {\"name\":\"created-by-eval\",\"start\":\"HEAD\"}. After its result, answer done without another tool call.",
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
        expected_arguments: r#"{"revision":"HEAD","max_entries":5}"#,
        prompt: "Call git_log with exactly {\"revision\":\"HEAD\",\"max_entries\":5}. After its result, answer done without another tool call.",
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
        expected_arguments: r#"{"path":"brief.txt","old_string":"alpha","new_string":"beta","replace_all":false}"#,
        prompt: "Call edit_file with exactly {\"path\":\"brief.txt\",\"old_string\":\"alpha\",\"new_string\":\"beta\",\"replace_all\":false}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: WRITE_FILE_NAME,
        expected_arguments: r#"{"path":"written.txt","content":"written by eval\n"}"#,
        prompt: "Call write_file with exactly {\"path\":\"written.txt\",\"content\":\"written by eval\\n\"}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: READ_FILE_NAME,
        expected_arguments: r#"{"path":"brief.txt","max_bytes":1024}"#,
        prompt: "Call read_file with exactly {\"path\":\"brief.txt\",\"max_bytes\":1024}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: LIST_DIRECTORY_NAME,
        expected_arguments: r#"{"path":".","max_results":20}"#,
        prompt: "Call list_directory with exactly {\"path\":\".\",\"max_results\":20}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: GLOB_FILES_NAME,
        expected_arguments: r#"{"path":".","pattern":"*.txt","max_results":20}"#,
        prompt: "Call glob_files with exactly {\"path\":\".\",\"pattern\":\"*.txt\",\"max_results\":20}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: SEARCH_FILES_NAME,
        expected_arguments: r#"{"path":".","pattern":"beta","max_results":20}"#,
        prompt: "Call search_files with exactly {\"path\":\".\",\"pattern\":\"beta\",\"max_results\":20}. After its result, answer done without another tool call.",
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

const EXEC_CASES: &[ForcedCase] = &[
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
    catalog: MergedCatalog,
    executor: SharedFamilyExecutor,
}

impl FamilySuite {
    fn git() -> EvalResult<Self> {
        let workspace = tempfile::tempdir()?;
        let git_seed = seed_git_repository(workspace.path())?;
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
            catalog: MergedCatalog::try_new([catalog])?,
            executor: SharedFamilyExecutor::new(FamilyExecutor::Git(executor)),
        })
    }

    fn workspace() -> EvalResult<Self> {
        let workspace = tempfile::tempdir()?;
        fs::write(workspace.path().join(WORKSPACE_SEED_PATH), "alpha\n")?;
        let reads = WorkspaceReadTools::try_new(LocalWorkspaceFileSystem, workspace.path())?;
        let mutations =
            WorkspaceMutationTools::try_new(LocalWorkspaceFileSystem, workspace.path())?;
        let (read_catalog, read_executor) = reads.into_parts();
        let (mutation_catalog, mutation_executor) = mutations.into_parts();
        Ok(Self {
            family: EvalFamily::Workspace,
            workspace,
            git_seed: None,
            catalog: MergedCatalog::try_new([read_catalog, mutation_catalog])?,
            executor: SharedFamilyExecutor::new(FamilyExecutor::Workspace {
                read: read_executor,
                mutation: mutation_executor,
            }),
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
            catalog: MergedCatalog::try_new([fetch_catalog, search_catalog])?,
            executor: SharedFamilyExecutor::new(FamilyExecutor::Web {
                fetch: fetch_executor,
                search: search_executor,
            }),
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
        if catalog_names != case_names || case_names.len() != cases.len() {
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
        if self.family == EvalFamily::Git && tool == GIT_CREATE_COMMIT_NAME {
            stage_path(self.workspace.path(), GIT_COMMIT_PATH)?;
        }
        if self.family == EvalFamily::Exec {
            self.executor.prepare_exec_case(tool).await?;
        }
        Ok(())
    }

    fn natural_state_passed(&self, snapshot: &CaseSnapshot) -> EvalResult<bool> {
        match self.family {
            EvalFamily::Git => {
                let seed = self.git_seed.ok_or_else(|| {
                    io::Error::other("the Git eval suite has no captured seed identity")
                })?;
                Ok(git_natural_state_passed(self.workspace.path(), seed)?
                    && snapshot.git_natural_requests_passed()?)
            }
            EvalFamily::Workspace => {
                let bytes_match = self.workspace_answer_matches()?;
                Ok(bytes_match && snapshot.workspace_natural_requests_passed())
            }
            EvalFamily::Web => snapshot.web_natural_requests_passed(),
            EvalFamily::Exec => Ok(fs::read(self.workspace.path().join(EXEC_RESULT_PATH))
                .ok()
                .as_deref()
                == Some(EXEC_RESULT.as_bytes())),
        }
    }

    fn workspace_answer_matches(&self) -> EvalResult<bool> {
        match fs::read(self.workspace.path().join(WORKSPACE_ANSWER_PATH)) {
            Ok(bytes) => Ok(bytes == WORKSPACE_ANSWER.as_bytes()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
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

fn normalized_arguments_text(arguments: &str) -> EvalResult<String> {
    NormalizedToolArguments::try_from_provider_text(arguments.to_owned())
        .map(|arguments| arguments.as_str().to_owned())
        .map_err(|_| io::Error::other("the eval fixture arguments do not normalize").into())
}

fn seed_git_repository(root: &Path) -> EvalResult<Oid> {
    let repository = Repository::init(root)?;
    fs::write(root.join(GIT_SEED_PATH), "seed\n")?;
    fs::write(root.join(GIT_STAGE_PATH), "stage me\n")?;
    fs::write(root.join(GIT_COMMIT_PATH), "commit me\n")?;
    fs::write(root.join(GIT_NATURAL_PATH), "natural eval\n")?;
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
    repository.branch("switch-target", &commit, false)?;
    Ok(commit.id())
}

fn stage_path(root: &Path, path: &str) -> EvalResult {
    let repository = Repository::open(root)?;
    let mut index = repository.index()?;
    index.add_all([path], IndexAddOption::DEFAULT, None)?;
    index.write()?;
    Ok(())
}

fn commit_staged_paths(root: &Path, message: &str) -> EvalResult {
    let repository = Repository::open(root)?;
    let mut index = repository.index()?;
    let tree_id = index.write_tree()?;
    let tree = repository.find_tree(tree_id)?;
    let parent = repository.head()?.peel_to_commit()?;
    let signature = Signature::now(GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL)?;
    repository.commit(
        Some("HEAD"),
        &signature,
        &signature,
        message,
        &tree,
        &[&parent],
    )?;
    Ok(())
}

fn git_natural_state_passed(root: &Path, seed: Oid) -> EvalResult<bool> {
    let repository = Repository::open(root)?;
    let head = repository.head()?.peel_to_commit()?;
    let message_matches = head.message()? == GIT_NATURAL_MESSAGE;
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
    Ok(message_matches
        && exactly_one_descendant_commit
        && commit_changes_only_natural_path
        && natural_path_is_clean)
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

impl ExecEvalCase {
    fn for_forced_tool(tool: &str) -> EvalResult<Self> {
        match tool {
            SANDBOXED_EXEC_NAME => Ok(Self::ForcedSandboxed),
            UNSANDBOXED_EXEC_NAME => Ok(Self::ForcedUnsandboxed),
            CARGO_DIAGNOSTICS_NAME => Ok(Self::ForcedDiagnostics),
            _ => Err(io::Error::other("the forced exec eval tool is unsupported").into()),
        }
    }

    fn admits(self, name: &str, arguments: &NormalizedToolArguments) -> bool {
        let (expected_name, expected_arguments) = match self {
            Self::Natural => (
                SANDBOXED_EXEC_NAME,
                serde_json::json!({
                    "program": EXEC_NATURAL_PROGRAM,
                    "arguments": ["-c", EXEC_NATURAL_SCRIPT],
                    "working_directory": ".",
                    "timeout_seconds": 30,
                }),
            ),
            Self::ForcedSandboxed => (
                SANDBOXED_EXEC_NAME,
                serde_json::json!({
                    "program": "printf",
                    "arguments": [EXEC_FORCED_SANDBOXED_OUTPUT],
                    "working_directory": ".",
                    "timeout_seconds": 30,
                }),
            ),
            Self::ForcedUnsandboxed => (
                UNSANDBOXED_EXEC_NAME,
                serde_json::json!({
                    "program": EXEC_FORCED_READ_ONLY_PROGRAM,
                    "arguments": [EXEC_FORCED_READ_ONLY_OUTPUT],
                    "working_directory": ".",
                    "timeout_seconds": 30,
                }),
            ),
            Self::ForcedDiagnostics => (
                CARGO_DIAGNOSTICS_NAME,
                serde_json::json!({
                    "command": "check",
                    "timeout_seconds": 120,
                }),
            ),
        };
        let expected =
            NormalizedToolArguments::try_from_provider_text(expected_arguments.to_string())
                .expect("the static exec eval arguments normalize");
        name == expected_name && arguments == &expected
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

#[derive(Clone, Copy, Debug)]
struct FamilyExecutorError;

impl fmt::Display for FamilyExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the selected eval tool executor failed")
    }
}

impl Error for FamilyExecutorError {}

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
        let mut inner = self.inner.lock().await;
        match &mut *inner {
            FamilyExecutor::Git(executor) => executor
                .execute(invocation)
                .await
                .map_err(|_| FamilyExecutorError),
            FamilyExecutor::Workspace { read, .. }
                if matches!(
                    name.as_str(),
                    READ_FILE_NAME | LIST_DIRECTORY_NAME | GLOB_FILES_NAME | SEARCH_FILES_NAME
                ) =>
            {
                read.execute(invocation)
                    .await
                    .map_err(|_| FamilyExecutorError)
            }
            FamilyExecutor::Workspace { mutation, .. } => mutation
                .execute(invocation)
                .await
                .map_err(|_| FamilyExecutorError),
            FamilyExecutor::Web { fetch, .. } if name == WEB_FETCH_NAME => fetch
                .execute(invocation)
                .await
                .map_err(|_| FamilyExecutorError),
            FamilyExecutor::Web { search, .. } => search
                .execute(invocation)
                .await
                .map_err(|_| FamilyExecutorError),
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
                        .map_err(|_| FamilyExecutorError),
                    UNSANDBOXED_EXEC_NAME => unsandboxed
                        .execute(invocation)
                        .await
                        .map_err(|_| FamilyExecutorError),
                    _ => diagnostics
                        .execute(invocation)
                        .await
                        .map_err(|_| FamilyExecutorError),
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct FixtureWebFetchTransport;

impl WebFetchTransport for FixtureWebFetchTransport {
    async fn fetch(
        &mut self,
        _request: WebFetchRequest,
    ) -> Result<WebFetchResponse, WebFetchTransportFailure> {
        WebFetchResponse::new(
            200,
            Some(String::from("text/plain")),
            b"Signalbox tool evaluation fixture".to_vec(),
            WebFetchBodyCompleteness::Complete,
        )
        .ok_or(WebFetchTransportFailure::DispatchUnknown)
    }
}

#[derive(Clone, Copy, Debug)]
struct FixtureWebCredential;

impl CredentialAccess for FixtureWebCredential {
    async fn resolve(
        &self,
        _reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        Ok(CredentialValue::new(SYNTHETIC_WEB_CREDENTIAL.to_vec()))
    }
}

#[derive(Clone, Copy, Debug)]
struct FixtureWebSearchTransport;

impl WebSearchTransport for FixtureWebSearchTransport {
    async fn search(
        &mut self,
        _request: WebSearchRequest,
        credential: &CredentialValue,
    ) -> WebSearchTransportOutcome {
        let result = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from("Signalbox tool evaluation"),
            url: String::from(WEB_URL),
            snippet: String::from("Synthetic result for model-in-the-loop evaluation."),
        })
        .expect("the synthetic web result is valid");
        let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
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
}

impl ForcedToolSequence {
    fn new(forced_tool: Option<&str>) -> Self {
        Self {
            pending: forced_tool.map(|tool| StdMutex::new(Some(RuntimeToolName::new(tool)))),
        }
    }

    fn next(&self) -> ForcedToolOperation {
        let Some(pending) = &self.pending else {
            return ForcedToolOperation::Natural;
        };
        pending
            .lock()
            .expect("forced-tool lock is available")
            .take()
            .map_or(
                ForcedToolOperation::Continuation,
                ForcedToolOperation::Force,
            )
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
        self.inner.execute(prepared, sink, cancellation).await
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TrackedToolResult {
    content: String,
    is_error: bool,
}

impl OperationTracker {
    fn observe(&self, operation: &ModelOperation<ModelCallId>) {
        let tool_results = operation.messages.iter().flat_map(|message| {
            message.parts.iter().filter_map(|part| match part {
                MessagePart::ToolResult(result) => Some((
                    String::from(result.tool_call_id.as_str()),
                    TrackedToolResult {
                        content: result.content.clone(),
                        is_error: result.is_error,
                    },
                )),
                _ => None,
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
                state.tool_results.push(result);
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
}

struct EvalDatabase {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
    selection: DirectModelSelection,
    targets: ModelTargetCatalog,
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

    async fn approve_exact_read_only_unsandboxed_request(
        &self,
        session: SessionId,
        turn: TurnId,
    ) -> EvalResult<bool> {
        let pending = sqlx::query_as::<_, PendingApproval>(
            "SELECT request.request_id, request.tool_name, request.arguments_text
               FROM turn_lifecycle AS lifecycle
               JOIN tool_request AS request
                 ON request.request_id = lifecycle.approval_tool_request_id
                AND request.session_id = lifecycle.session_id
                AND request.turn_id = lifecycle.turn_id
              WHERE lifecycle.session_id = $1
                AND lifecycle.turn_id = $2
                AND lifecycle.active_phase_kind = 'awaiting_tool_approval'",
        )
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        let Some(pending) = pending else {
            return Ok(false);
        };
        if !pending.is_exact_read_only_unsandboxed_request()? {
            return Ok(false);
        }
        let mut service = DecideToolRequestService::new(
            UuidV7ToolLoopIdGenerator,
            PostgresToolLoopRepository::new(self.pool.clone()),
        );
        let prepared = service
            .execute(
                DecideToolRequest::try_new(
                    DurableCommandId::from_uuid(Uuid::from_u128(
                        ARBITRARY_EVAL_APPROVAL_COMMAND_ID,
                    )),
                    ToolRequestId::from_uuid(pending.request_id),
                    ToolApprovalDecision::Approve,
                )
                .map_err(|_| io::Error::other("the exact read-only exec approval is invalid"))?,
            )
            .await?;
        if !matches!(prepared.result(), DecideToolRequestResult::Applied(_)) {
            return Err(io::Error::other("the exact read-only exec approval was rejected").into());
        }
        Ok(true)
    }
}

#[derive(sqlx::FromRow)]
struct PendingApproval {
    request_id: Uuid,
    tool_name: String,
    arguments_text: String,
}

impl PendingApproval {
    fn is_exact_read_only_unsandboxed_request(&self) -> EvalResult<bool> {
        let arguments =
            NormalizedToolArguments::try_from_provider_text(self.arguments_text.clone())
                .map_err(|_| io::Error::other("the pending exec arguments do not normalize"))?;
        Ok(ExecEvalCase::ForcedUnsandboxed.admits(&self.tool_name, &arguments))
    }
}

fn eval_session_credential_pin() -> SessionCredentialPin {
    SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        "openai",
        "openai-primary",
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

#[derive(sqlx::FromRow)]
struct RequestSnapshot {
    request_id: Uuid,
    producing_model_call_id: Uuid,
    name: String,
    arguments_text: String,
    #[sqlx(skip)]
    attempt_succeeded: bool,
}

impl RequestSnapshot {
    fn arguments(&self) -> Option<serde_json::Value> {
        serde_json::from_str(&self.arguments_text).ok()
    }
}

impl CaseSnapshot {
    async fn read(pool: &PgPool, session: SessionId, turn: TurnId) -> EvalResult<Self> {
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
        let turn_disposition = SnapshotTurnDisposition::from_process_state(turn_state)?;
        let successful_requests = successful_tool_requests(transcript.entries());
        let requests = sqlx::query_as::<_, RequestSnapshot>(
            "SELECT request.request_id,
                    request.producing_model_call_id,
                    request.tool_name AS name,
                    request.arguments_text
               FROM tool_request AS request
              WHERE request.session_id = $1 AND request.turn_id = $2
              ORDER BY request.producing_model_call_id, request.request_ordinal",
        )
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .fetch_all(pool)
        .await?;
        let requests = requests
            .into_iter()
            .map(|mut request| {
                request.attempt_succeeded = successful_requests.contains(&request.request_id);
                request
            })
            .collect();
        let model_calls = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM model_call WHERE session_id = $1 AND turn_id = $2",
        )
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .fetch_one(pool)
        .await?;
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
        let read = self.requests.iter().position(|request| {
            request.name == READ_FILE_NAME
                && request
                    .arguments()
                    .is_some_and(|arguments| arguments["path"] == WORKSPACE_SEED_PATH)
        });
        let write = self.requests.iter().position(|request| {
            request.name == WRITE_FILE_NAME
                && request.arguments().is_some_and(|arguments| {
                    arguments["path"] == WORKSPACE_ANSWER_PATH
                        && arguments["content"] == WORKSPACE_ANSWER
                })
        });
        read.zip(write).is_some_and(|(read, write)| {
            read < write
                && self.requests[read].producing_model_call_id
                    != self.requests[write].producing_model_call_id
        })
    }

    fn git_natural_requests_passed(&self) -> EvalResult<bool> {
        let expected_stage = normalized_arguments_text(r#"{"paths":["eval.txt"]}"#)?;
        let expected_commit = normalized_arguments_text(r#"{"message":"tool eval commit"}"#)?;
        let stage = self.requests.iter().position(|request| {
            request.name == GIT_STAGE_NAME && request.arguments_text == expected_stage
        });
        let commit = self.requests.iter().position(|request| {
            request.name == GIT_CREATE_COMMIT_NAME && request.arguments_text == expected_commit
        });
        Ok(stage.zip(commit).is_some_and(|(stage, commit)| {
            stage < commit
                && self.requests[stage].producing_model_call_id
                    != self.requests[commit].producing_model_call_id
        }))
    }

    fn web_natural_requests_passed(&self) -> EvalResult<bool> {
        let expected_query = normalized_arguments_text(r#"{"query":"Signalbox tool evaluation"}"#)?;
        let expected_url = normalized_arguments_text(r#"{"url":"https://example.com/eval"}"#)?;
        let search = self.requests.iter().position(|request| {
            request.name == WEB_SEARCH_NAME && request.arguments_text == expected_query
        });
        let fetch = self.requests.iter().position(|request| {
            request.name == WEB_FETCH_NAME && request.arguments_text == expected_url
        });
        Ok(search.zip(fetch).is_some_and(|(search, fetch)| {
            search < fetch
                && self.requests[search].producing_model_call_id
                    != self.requests[fetch].producing_model_call_id
        }))
    }
}

fn successful_tool_requests(entries: &[ProcessTranscriptEntry]) -> BTreeSet<Uuid> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            ProcessTranscriptEntry::ToolExecutionResult {
                request,
                disposition: ProcessToolExecutionResultDisposition::Completed,
                ..
            } => Some(request.into_uuid()),
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

#[derive(Clone, Copy)]
enum SnapshotTurnDisposition {
    Completed,
    Other,
}

impl SnapshotTurnDisposition {
    fn from_process_state(state: &ProcessTurnState) -> EvalResult<Self> {
        match state {
            ProcessTurnState::Completed { .. } => Ok(Self::Completed),
            ProcessTurnState::Refused { .. }
            | ProcessTurnState::ActiveAwaitingToolApproval { .. } => Ok(Self::Other),
            ProcessTurnState::Queued { .. }
            | ProcessTurnState::QueuedDelegated { .. }
            | ProcessTurnState::QueuedDelegationWake { .. }
            | ProcessTurnState::DelegationTerminated { .. }
            | ProcessTurnState::ActiveRunning { .. }
            | ProcessTurnState::ActiveAwaitingModelCallRecovery { .. }
            | ProcessTurnState::ActiveAwaitingChild { .. }
            | ProcessTurnState::ActiveAwaitingToolRecovery { .. }
            | ProcessTurnState::Failed { .. }
            | ProcessTurnState::Cancelled { .. }
            | ProcessTurnState::ReconciliationRequired { .. } => Err(io::Error::other(
                "the eval turn did not reach a reportable model outcome",
            )
            .into()),
        }
    }

    const fn is_completed(self) -> bool {
        match self {
            Self::Completed => true,
            Self::Other => false,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Other => "not completed",
        }
    }
}

struct CaseOutcome {
    target: Option<String>,
    expected_arguments: Option<String>,
    execution_completed: bool,
    tool_results: Vec<TrackedToolResult>,
    snapshot: CaseSnapshot,
}

impl CaseOutcome {
    fn forced_disposition(&self) -> EvalDisposition {
        let Some(target) = self.target.as_deref() else {
            return EvalDisposition::Miss;
        };
        let Some(expected_arguments) = self.expected_arguments.as_deref() else {
            return EvalDisposition::Miss;
        };
        EvalDisposition::from_passed(
            self.execution_completed
                && self.snapshot.turn_disposition.is_completed()
                && self.snapshot.model_calls >= MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP
                && self.forced_result_passed(target)
                && self.snapshot.requests.len() == 1
                && self.snapshot.requests[0].name == target
                && self.snapshot.requests[0].arguments_text == expected_arguments
                && self.snapshot.requests[0].attempt_succeeded,
        )
    }

    fn natural_loop_disposition(&self, family: EvalFamily) -> EvalDisposition {
        let required_names: &[&str] = match family {
            EvalFamily::Git => &[GIT_STAGE_NAME, GIT_CREATE_COMMIT_NAME],
            EvalFamily::Workspace => &[READ_FILE_NAME, WRITE_FILE_NAME],
            EvalFamily::Web => &[WEB_SEARCH_NAME, WEB_FETCH_NAME],
            EvalFamily::Exec => &[SANDBOXED_EXEC_NAME],
        };
        EvalDisposition::from_passed(
            self.execution_completed
                && self.snapshot.turn_disposition.is_completed()
                && !self.tool_results.is_empty()
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
                        && self.snapshot.requests[0].name == SANDBOXED_EXEC_NAME)),
        )
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
        let execution = if target == CARGO_DIAGNOSTICS_NAME {
            &result["execution"]
        } else {
            &result
        };
        execution["outcome"]["kind"] == "exited"
            && execution["outcome"]["code"] == 0
            && (target != CARGO_DIAGNOSTICS_NAME
                || (execution["preparation_failure"].is_null()
                    && execution["cargo_failure"].is_null()))
    }
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
fn turn_snapshot_rejects_ambiguous_model_recovery() {
    let state = ProcessTurnState::ActiveAwaitingModelCallRecovery {
        ended_attempt: TurnAttemptId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_TURN_ATTEMPT_ID)),
        recovery_call: ModelCallId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID)),
    };

    assert!(SnapshotTurnDisposition::from_process_state(&state).is_err());
}

#[test]
fn forced_tier_passes_one_completed_target_with_a_result_round_trip() {
    let target = GIT_STATUS_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from("{}")),
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            content: String::from("fixture result"),
            is_error: false,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(target),
                arguments_text: String::from("{}"),
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
                attempt_succeeded: true,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_tier_reports_a_miss_for_a_known_failed_attempt() {
    let target = GIT_STATUS_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from("{}")),
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            content: String::from("fixture result"),
            is_error: false,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(target),
                arguments_text: String::from("{}"),
                attempt_succeeded: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn unforced_git_tier_requires_both_task_tools() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            content: String::from("fixture result"),
            is_error: false,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: String::from(r#"{"paths":["eval.txt"]}"#),
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
    let seed = seed_git_repository(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    stage_path(workspace.path(), GIT_STAGE_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;

    assert!(!git_natural_state_passed(workspace.path(), seed)?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_an_unrelated_earlier_commit() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let seed = seed_git_repository(workspace.path())?;
    stage_path(workspace.path(), GIT_STAGE_PATH)?;
    commit_staged_paths(workspace.path(), "unrelated eval commit")?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;

    assert!(!git_natural_state_passed(workspace.path(), seed)?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_parentless_seed_commit() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let seed = seed_git_repository(workspace.path())?;

    assert!(!git_natural_state_passed(workspace.path(), seed)?);
    Ok(())
}

#[test]
fn git_natural_state_keeps_the_captured_seed_after_a_branch_switch() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let seed = seed_git_repository(workspace.path())?;
    let repository = Repository::open(workspace.path())?;
    repository.set_head("refs/heads/switch-target")?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;

    assert!(git_natural_state_passed(workspace.path(), seed)?);
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
fn forced_tier_reports_a_miss_for_drifted_arguments() {
    let target = GIT_STATUS_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from("{}")),
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            content: String::from("fixture result"),
            is_error: false,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(target),
                arguments_text: String::from(r#"{"unexpected":true}"#),
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
fn exec_eval_rejects_model_argument_drift_before_dispatch() {
    let drifted = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({
            "program": "curl",
            "arguments": ["https://example.com"],
            "working_directory": ".",
            "timeout_seconds": 30,
        })
        .to_string(),
    )
    .expect("drifted fixture arguments normalize");

    assert!(!ExecEvalCase::ForcedSandboxed.admits(SANDBOXED_EXEC_NAME, &drifted));
}

#[test]
fn operation_tracker_records_each_cumulative_tool_result_once() {
    let tracker = OperationTracker::default();
    let tool_call_id = String::from("synthetic-tool-call");
    let result = TrackedToolResult {
        content: String::from("synthetic result"),
        is_error: false,
    };
    tracker.record_new_results([(tool_call_id.clone(), result.clone())]);
    tracker.record_new_results([(tool_call_id, result.clone())]);

    assert_eq!(tracker.tool_results(), vec![result]);
}

#[test]
fn forced_exec_tier_rejects_a_nonzero_process_result() {
    let target = SANDBOXED_EXEC_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from(EXEC_FORCED_SANDBOXED_ARGUMENTS)),
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            content: serde_json::json!({
                "outcome": {"kind": "exited", "code": 1},
            })
            .to_string(),
            is_error: false,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(target),
                arguments_text: String::from(EXEC_FORCED_SANDBOXED_ARGUMENTS),
                attempt_succeeded: true,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn unforced_exec_tier_rejects_an_additional_tool_call() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        tool_results: vec![TrackedToolResult {
            content: String::from("fixture result"),
            is_error: false,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(SANDBOXED_EXEC_NAME),
                    arguments_text: String::from("{}"),
                    attempt_succeeded: true,
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(CARGO_DIAGNOSTICS_NAME),
                    arguments_text: String::from("{}"),
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

#[test]
fn workspace_natural_state_requires_the_read_before_the_write() {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WRITE_FILE_NAME),
                arguments_text: String::from(
                    r#"{"content":"model loop observed\n","path":"answer.txt"}"#,
                ),
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(READ_FILE_NAME),
                arguments_text: String::from(r#"{"path":"brief.txt"}"#),
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
                arguments_text: String::from(r#"{"path":"brief.txt"}"#),
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WRITE_FILE_NAME),
                arguments_text: String::from(
                    r#"{"content":"model loop observed\n","path":"answer.txt"}"#,
                ),
                attempt_succeeded: true,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.workspace_natural_requests_passed());
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

#[test]
fn git_natural_state_requires_a_later_model_call_for_the_commit() -> EvalResult {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: normalized_arguments_text(r#"{"paths":["eval.txt"]}"#)?,
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: normalized_arguments_text(r#"{"message":"tool eval commit"}"#)?,
                attempt_succeeded: true,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.git_natural_requests_passed()?);
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
                    r#"{"query":"Signalbox tool evaluation"}"#,
                )?,
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_FETCH_NAME),
                arguments_text: normalized_arguments_text(r#"{"url":"https://example.com/eval"}"#)?,
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
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_FETCH_NAME),
                arguments_text: String::from(r#"{"url":"https://example.com/eval"}"#),
                attempt_succeeded: true,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.web_natural_requests_passed()?);
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EvalDisposition {
    Pass,
    Miss,
}

impl EvalDisposition {
    const fn from_passed(passed: bool) -> Self {
        if passed { Self::Pass } else { Self::Miss }
    }

    const fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Pass, Self::Pass) => Self::Pass,
            (Self::Pass, Self::Miss) | (Self::Miss, Self::Pass | Self::Miss) => Self::Miss,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Miss => "MISS",
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
            outcome.tool_results.len(),
        ));
    }
    let natural = report
        .natural
        .natural_loop_disposition(report.family)
        .and(report.natural_state);
    markdown.push_str(&format!(
        "\n### Unforced tier\n\n| Result | Calls observed | Tool result round-trips | Task state |\n| --- | --- | ---: | --- |\n| {} | {} | {} | {} |\n\nAll outcomes are report-only; a model miss does not fail this workflow.\n",
        natural.label(),
        report.natural.snapshot.called_names(),
        report.natural.tool_results.len(),
        report.natural_state.label(),
    ));
    fs::write(summary_path, &markdown)?;
    print!("{markdown}");
    Ok(())
}
