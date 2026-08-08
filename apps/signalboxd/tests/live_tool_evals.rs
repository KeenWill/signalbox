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

use git2::{BranchType, DiffOptions, IndexAddOption, Oid, Patch, Repository, Signature, Status};
use signalbox_application::{
    ClassifyOperatorFailure, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    CreateSessionOutcome, CreateSessionRequest, CreateSessionService, InProcessAttemptDispatchGate,
    InProcessEligibilityWorkSource, InProcessToolDispatchGate, ModelCallCredentialReference,
    OperatorFailureClass, StartEligibleTurnOutcome, StartEligibleTurnService, SubmitInputOutcome,
    SubmitInputRequest, SubmitInputService, ToolCatalog, ToolCatalogValidationFailure,
    ToolDefinition, ToolExecutionInvocation, ToolExecutor, ToolExecutorEvidence,
    UuidV7SessionIdGenerator, UuidV7StartEligibleTurnIdGenerator, UuidV7SubmitInputIdGenerator,
};
use signalbox_domain::{
    DangerousToolAutoApproval, DeliveryRequest, DirectModelSelection, DurableCommandId,
    ModelCallId, ModelSelectionOverride, ModelSelectionRequest, ModelTargetCatalog,
    ModelTargetDefinition, NormalizedToolArguments, PerInputConfigurationChoices,
    ProviderModelIdentity, ResolvedProviderTarget, SemanticTranscriptEntryId,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionId,
    SubmitInputAppliedResult, SubmitInputResult, ToolAttemptId, ToolName as DomainToolName,
    ToolRequestId, TurnAttemptId, TurnId, UserContent,
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
    SessionCredentialPin, SessionModelCredential, local_test_connection_options, migrate,
    model_execution::PostgresModelCallRepository,
    process_read::{
        ProcessFailedModelCallDisposition, ProcessProviderModelCallFailureCause,
        ProcessReadRepository, ProcessToolExecutionResultDisposition, ProcessTranscriptEntry,
        ProcessTurnState,
    },
    scheduler::PostgresEligibilitySweep,
    start_eligible_turn::StartEligibleTurnRepository,
    submit_input::SubmitInputRepository,
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
const CONTEXT_WINDOW_TOKENS: u32 = 200_000;
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(2 * 60);
/// Four exchanges cover the accepted premature fetch, search, later fetch,
/// and final answer path, with one minute for local persistence and dispatch.
const TURN_TIMEOUT: Duration = Duration::from_secs(4 * 2 * 60 + 60);
const LIVE_EVAL_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;
const GIT_AUTHOR_NAME: &str = "Signalbox Tool Eval";
const GIT_AUTHOR_EMAIL: &str = "signalbox-tool-eval@example.test";
const GIT_SEED_PATH: &str = "seed.txt";
const GIT_STAGE_PATH: &str = "stage-me.txt";
const GIT_COMMIT_PATH: &str = "commit-me.txt";
const GIT_NATURAL_PATH: &str = "eval.txt";
const GIT_NATURAL_MESSAGE: &str = "tool eval commit";
const WORKSPACE_SEED_PATH: &str = "brief.txt";
const WORKSPACE_SEED: &str = "alpha\n";
const WORKSPACE_ANSWER_PATH: &str = "answer.txt";
const WORKSPACE_ANSWER: &str = "model loop observed\n";
const WEB_ORIGIN: &str = "https://example.com";
const WEB_URL: &str = "https://example.com/eval";
const WEB_QUERY: &str = "Signalbox tool evaluation";
const WEB_FETCH_BODY: &str = "Signalbox tool evaluation fixture";
const WEB_SEARCH_SNIPPET: &str = "Synthetic result for model-in-the-loop evaluation.";
const EXPECTED_OPENAI_CREDENTIAL_REFERENCE: &str = "openai-tool-eval";
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
const ARBITRARY_SECOND_EVAL_MODEL_CALL_ID: u128 = 0x9109;
const ARBITRARY_SECOND_EVAL_REQUEST_ID: u128 = 0x910a;
const MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP: i64 = 2;
const SYNTHETIC_EXECUTOR_FAILURE: &str = "synthetic executor failure";
const SYNTHETIC_EVAL_RECEIPT: &str = "01988c5f-89c4-7000-8000-000000000001";
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
        suite.prepare_for(case.name)?;
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
        .map_err(|_| io::Error::other("the daemon tool eval turn exceeded its timeout"))??;
    let snapshot = CaseSnapshot::read(&database.pool, session, turn).await?;
    let execution_completed = match (forced_case, snapshot.requests.as_slice()) {
        (None, _) => true,
        (Some(case), [request]) => match tracker.result_content(request.request_id) {
            Some(content) => suite.forced_case_result_passed(case, &content)?,
            None => false,
        },
        (Some(_), _) => false,
    };
    Ok(CaseOutcome {
        target: forced_tool.map(str::to_owned),
        expected_arguments: forced_case
            .map(|case| normalized_arguments_text(case.expected_arguments))
            .transpose()?,
        execution_completed,
        result_round_trips: tracker.result_round_trips(),
        round_tripped_request_ids: tracker.round_tripped_request_ids(),
        snapshot,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EvalFamily {
    Git,
    Workspace,
    Web,
}

impl EvalFamily {
    fn from_environment() -> EvalResult<Option<Self>> {
        match std::env::var(FAMILY_VARIABLE).as_deref() {
            Ok("git") => Ok(Some(Self::Git)),
            Ok("workspace") => Ok(Some(Self::Workspace)),
            Ok("web") => Ok(Some(Self::Web)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            _ => Err(io::Error::other("the configured tool-eval family is unsupported").into()),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Workspace => "workspace",
            Self::Web => "web",
        }
    }

    const fn model(self) -> &'static str {
        match self {
            Self::Git | Self::Workspace | Self::Web => DEFAULT_MODEL,
        }
    }

    fn build_suite(self) -> EvalResult<FamilySuite> {
        match self {
            Self::Git => FamilySuite::git(),
            Self::Workspace => FamilySuite::workspace(),
            Self::Web => FamilySuite::web(),
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
        fs::write(workspace.path().join(WORKSPACE_SEED_PATH), WORKSPACE_SEED)?;
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

    const fn forced_cases(&self) -> &'static [ForcedCase] {
        match self.family {
            EvalFamily::Git => GIT_CASES,
            EvalFamily::Workspace => WORKSPACE_CASES,
            EvalFamily::Web => WEB_CASES,
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
        }
    }

    fn prepare_for(&self, tool: &str) -> EvalResult {
        if self.family == EvalFamily::Git && tool == GIT_CREATE_COMMIT_NAME {
            stage_path(self.workspace.path(), GIT_COMMIT_PATH)?;
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
                git_forced_case_passed(self.workspace.path(), case.name, &arguments, &result)
            }
            EvalFamily::Workspace => {
                workspace_forced_case_passed(self.workspace.path(), case.name, &arguments, &result)
            }
            EvalFamily::Web => Ok(web_forced_case_passed(case.name, &arguments, &result)),
        }
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

fn git_forced_case_passed(
    root: &Path,
    name: &str,
    arguments: &serde_json::Value,
    result: &serde_json::Value,
) -> EvalResult<bool> {
    let repository = Repository::open(root)?;
    let head = repository.head()?.peel_to_commit()?;
    let passed = match name {
        GIT_BRANCH_CREATE_NAME => {
            let Some(branch_name) = arguments["name"].as_str() else {
                return Ok(false);
            };
            let branch = repository.find_branch(branch_name, BranchType::Local)?;
            let branch_target = branch.get().target().map(|target| target.to_string());
            result["branch"] == branch_name && result["head"].as_str() == branch_target.as_deref()
        }
        GIT_BRANCH_SWITCH_NAME => {
            let Some(branch_name) = arguments["name"].as_str() else {
                return Ok(false);
            };
            result["branch"] == branch_name
                && result["head"] == head.id().to_string()
                && repository.head()?.shorthand().ok() == Some(branch_name)
        }
        GIT_CREATE_COMMIT_NAME => {
            result["commit"] == head.id().to_string()
                && result["state_cleaned"] == true
                && head.message().ok() == arguments["message"].as_str()
        }
        GIT_DIFF_NAME => {
            result["patch"].as_str() == Some(expected_git_worktree_patch(root)?.as_str())
                && result["truncated"] == false
        }
        GIT_LOG_NAME => {
            result["commits"]
                .as_array()
                .and_then(|commits| commits.first())
                .is_some_and(|commit| commit["commit"] == head.id().to_string())
                && result["truncated"] == false
        }
        GIT_STAGE_NAME => {
            let Some(paths) = arguments["paths"].as_array() else {
                return Ok(false);
            };
            result["staged_paths"] == paths.len()
                && paths.iter().all(|path| {
                    path.as_str().is_some_and(|path| {
                        repository.status_file(Path::new(path)).ok() == Some(Status::INDEX_NEW)
                    })
                })
        }
        GIT_STATUS_NAME => {
            let entries = result["entries"].as_array();
            let paths = entries
                .into_iter()
                .flatten()
                .filter_map(|entry| entry["path"].as_str())
                .collect::<BTreeSet<_>>();
            result["head"] == head.id().to_string()
                && entries.is_some_and(|entries| entries.len() == 3)
                && paths == BTreeSet::from([GIT_STAGE_PATH, GIT_COMMIT_PATH, GIT_NATURAL_PATH])
                && result["truncated"] == false
        }
        _ => false,
    };
    Ok(passed)
}

fn expected_git_worktree_patch(root: &Path) -> EvalResult<String> {
    let mut expected = Vec::new();
    for path in [GIT_COMMIT_PATH, GIT_NATURAL_PATH, GIT_STAGE_PATH] {
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

fn workspace_forced_case_passed(
    root: &Path,
    name: &str,
    arguments: &serde_json::Value,
    result: &serde_json::Value,
) -> EvalResult<bool> {
    let passed = match name {
        APPLY_PATCH_NAME => {
            result["operations_applied"] == 1
                && fs::read_to_string(root.join("patched.txt"))? == "patched by eval\n"
        }
        EDIT_FILE_NAME => {
            let Some(path) = arguments["path"].as_str() else {
                return Ok(false);
            };
            let expected = WORKSPACE_SEED.replace(
                arguments["old_string"].as_str().unwrap_or_default(),
                arguments["new_string"].as_str().unwrap_or_default(),
            );
            result["path"] == path
                && result["replacements"] == 1
                && result["bytes_written"] == expected.len()
                && fs::read_to_string(root.join(path))? == expected
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
        }
        READ_FILE_NAME => {
            let Some(path) = arguments["path"].as_str() else {
                return Ok(false);
            };
            let expected = fs::read_to_string(root.join(path))?;
            result["path"] == path
                && result["content"] == expected
                && result["bytes_read"] == expected.len()
                && result["total_bytes"] == expected.len()
                && result["truncated"] == false
        }
        LIST_DIRECTORY_NAME => {
            result_paths_equal(&result["entries"], [WORKSPACE_SEED_PATH])
                && result["truncated"] == false
        }
        GLOB_FILES_NAME => {
            result_paths_equal(&result["matches"], [WORKSPACE_SEED_PATH])
                && result["truncated"] == false
        }
        SEARCH_FILES_NAME => {
            result["matches"].as_array().is_some_and(Vec::is_empty) && result["truncated"] == false
        }
        _ => false,
    };
    Ok(passed)
}

fn result_paths_equal<const N: usize>(value: &serde_json::Value, expected: [&str; N]) -> bool {
    value.as_array().is_some_and(|entries| {
        entries.len() == N
            && entries
                .iter()
                .filter_map(|entry| entry["path"].as_str())
                .eq(expected)
    })
}

fn web_forced_case_passed(
    name: &str,
    arguments: &serde_json::Value,
    result: &serde_json::Value,
) -> bool {
    match name {
        WEB_FETCH_NAME => {
            result["url"] == arguments["url"]
                && result["status"] == 200
                && result["content_type"] == "text/plain"
                && result["body"] == WEB_FETCH_BODY
                && result["truncated"] == false
        }
        WEB_SEARCH_NAME => {
            result["results"]
                .as_array()
                .and_then(|results| results.first())
                .is_some_and(|first| {
                    first["title"] == arguments["query"]
                        && first["url"] == WEB_URL
                        && first["snippet"] == WEB_SEARCH_SNIPPET
                })
                && result["truncated"] == false
        }
        _ => false,
    }
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
    let index = repository.index()?;
    let index_contains_only_expected_paths = index.len() == 2
        && index.get_path(Path::new(GIT_SEED_PATH), 0).is_some()
        && index.get_path(Path::new(GIT_NATURAL_PATH), 0).is_some();
    Ok(message_matches
        && exactly_one_descendant_commit
        && commit_changes_only_natural_path
        && natural_path_is_clean
        && index_contains_only_expected_paths)
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
        if request.query() != WEB_QUERY {
            return WebSearchTransportOutcome::failed(
                WebSearchTransportFailure::RequestFailed,
                credential,
            );
        }
        let result = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(WEB_QUERY),
            url: String::from(WEB_URL),
            snippet: String::from(WEB_SEARCH_SNIPPET),
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
    result_round_trips: usize,
    round_tripped_request_ids: BTreeSet<Uuid>,
    pending_result_receipts: BTreeMap<Uuid, String>,
    result_contents: BTreeMap<Uuid, String>,
}

impl OperationTracker {
    fn observe(&self, operation: &ModelOperation<ModelCallId>) {
        for result in operation
            .messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                MessagePart::ToolResult(result) => Some(result),
                MessagePart::Text(_)
                | MessagePart::ToolCall(_)
                | MessagePart::Thinking { .. }
                | MessagePart::RedactedThinking { .. } => None,
            })
        {
            let Some(request_id) = Uuid::parse_str(result.tool_call_id.as_str()).ok() else {
                continue;
            };
            self.observe_result(request_id, &result.content);
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
        }
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

struct RequestSnapshot {
    request_id: Uuid,
    producing_model_call_id: Uuid,
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
        let turn_disposition = SnapshotTurnDisposition::from_process_state(turn_state);
        let successful_requests = successful_tool_requests(transcript.entries());
        let requests = transcript
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                ProcessTranscriptEntry::AssistantToolUse {
                    turn: request_turn,
                    model_call,
                    request,
                    name,
                    arguments,
                    ..
                } if *request_turn == turn => Some(RequestSnapshot {
                    request_id: request.into_uuid(),
                    producing_model_call_id: model_call.into_uuid(),
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
            }))
    }

    fn web_natural_requests_passed(&self) -> EvalResult<bool> {
        let expected_query =
            normalized_arguments_text(&serde_json::json!({"query": WEB_QUERY}).to_string())?;
        let expected_url =
            normalized_arguments_text(&serde_json::json!({"url": WEB_URL}).to_string())?;
        Ok(self
            .requests
            .iter()
            .enumerate()
            .any(|(search_index, search)| {
                search.name == WEB_SEARCH_NAME
                    && search.arguments_text == expected_query
                    && self.requests.iter().skip(search_index + 1).any(|fetch| {
                        fetch.name == WEB_FETCH_NAME
                            && fetch.arguments_text == expected_url
                            && search.producing_model_call_id != fetch.producing_model_call_id
                    })
            }))
    }

    fn exact_natural_request_failed(&self, family: EvalFamily) -> bool {
        self.requests.iter().enumerate().any(|(index, request)| {
            !request.attempt_succeeded
                && match family {
                    EvalFamily::Git => self.exact_git_natural_request_failed(index, request),
                    EvalFamily::Workspace => {
                        (request.name == READ_FILE_NAME
                            && request.arguments().is_some_and(|arguments| {
                                arguments == serde_json::json!({"path": WORKSPACE_SEED_PATH})
                            })
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
    let full_seed_bytes =
        u64::try_from(WORKSPACE_SEED.len()).expect("the workspace seed fixture length fits in u64");
    arguments["path"] == WORKSPACE_SEED_PATH
        && arguments
            .get("max_bytes")
            .and_then(serde_json::Value::as_u64)
            .is_none_or(|max_bytes| max_bytes >= full_seed_bytes)
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotTurnDisposition {
    Completed,
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
            Self::ProviderFailure(_) | Self::Infrastructure | Self::Refused => false,
        }
    }

    const fn is_infrastructure(self) -> bool {
        match self {
            Self::ProviderFailure(_) | Self::Infrastructure => true,
            Self::Completed | Self::Refused => false,
        }
    }

    /// Renders the turn cell, naming the closed provider cause when the daemon
    /// retained one so a paid run reports why the exchange never happened.
    fn label(self) -> String {
        match self {
            Self::Completed => String::from("completed"),
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
    result_round_trips: usize,
    round_tripped_request_ids: BTreeSet<Uuid>,
    snapshot: CaseSnapshot,
}

impl CaseOutcome {
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
                && self.result_round_trips >= 1
                && self
                    .round_tripped_request_ids
                    .contains(&self.snapshot.requests[0].request_id)
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
        let required_names: &[&str] = match family {
            EvalFamily::Git => &[GIT_STAGE_NAME, GIT_CREATE_COMMIT_NAME],
            EvalFamily::Workspace => &[READ_FILE_NAME, WRITE_FILE_NAME],
            EvalFamily::Web => &[WEB_SEARCH_NAME, WEB_FETCH_NAME],
        };
        EvalDisposition::from_passed(
            self.execution_completed
                && self.snapshot.turn_disposition.is_completed()
                && self.result_round_trips >= 1
                && self
                    .snapshot
                    .requests
                    .iter()
                    .all(|request| self.round_tripped_request_ids.contains(&request.request_id))
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
                    .all(|request| request.attempt_succeeded),
        )
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
        result_round_trips: 0,
        round_tripped_request_ids: BTreeSet::new(),
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
        result_round_trips: 1,
        round_tripped_request_ids: BTreeSet::from([Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)]),
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
        result_round_trips: 0,
        round_tripped_request_ids: BTreeSet::new(),
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
fn forced_tier_reports_infrastructure_for_an_exact_known_failed_attempt() {
    let target = GIT_STATUS_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from("{}")),
        execution_completed: true,
        result_round_trips: 1,
        round_tripped_request_ids: BTreeSet::from([Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)]),
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

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_web_tier_reports_infrastructure_for_an_exact_known_failed_attempt() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        result_round_trips: 1,
        round_tripped_request_ids: BTreeSet::from([Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)]),
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_SEARCH_NAME),
                arguments_text: serde_json::json!({"query": WEB_QUERY}).to_string(),
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
        result_round_trips: 1,
        round_tripped_request_ids: BTreeSet::from([Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)]),
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
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
        result_round_trips: 1,
        round_tripped_request_ids: BTreeSet::from([Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)]),
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
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
        result_round_trips: 1,
        round_tripped_request_ids: BTreeSet::from([Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)]),
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(GIT_STAGE_NAME),
                    arguments_text: serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
                    attempt_succeeded: true,
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                    name: String::from(GIT_CREATE_COMMIT_NAME),
                    arguments_text: serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
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
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_ATTEMPT_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
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
        result_round_trips: 1,
        round_tripped_request_ids: BTreeSet::from([Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)]),
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
        result_round_trips: 1,
        round_tripped_request_ids: BTreeSet::from([Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)]),
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(APPLY_PATCH_NAME),
                    arguments_text: String::from("{}"),
                    attempt_succeeded: true,
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                    name: String::from(READ_FILE_NAME),
                    arguments_text: serde_json::json!({"path": WORKSPACE_SEED_PATH}).to_string(),
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

fn failed_request_snapshot(name: &str, arguments: serde_json::Value) -> CaseSnapshot {
    CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![RequestSnapshot {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
            name: name.to_owned(),
            arguments_text: arguments.to_string(),
            attempt_succeeded: false,
        }],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    }
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
        result_round_trips: 1,
        round_tripped_request_ids: BTreeSet::from([Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)]),
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(READ_FILE_NAME),
                    arguments_text: serde_json::json!({"path": WORKSPACE_SEED_PATH}).to_string(),
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
        result_round_trips: 1,
        round_tripped_request_ids: BTreeSet::from([Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)]),
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
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
fn git_natural_state_rejects_extra_staging_after_the_target_commit() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let seed = seed_git_repository(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    stage_path(workspace.path(), GIT_STAGE_PATH)?;

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
fn forced_case_inventory_matches_every_family_catalog() -> EvalResult {
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
fn forced_git_diff_verifier_accepts_the_seeded_worktree_patch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_DIFF_NAME)
        .expect("the Git diff fixture exists");
    let result = serde_json::json!({
        "patch": expected_git_worktree_patch(suite.workspace.path())?,
        "truncated": false,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_diff_verifier_rejects_an_empty_patch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_DIFF_NAME)
        .expect("the Git diff fixture exists");
    let result = serde_json::json!({
        "patch": "",
        "truncated": false,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_tier_reports_a_miss_for_drifted_arguments() {
    let target = GIT_STATUS_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from("{}")),
        execution_completed: true,
        result_round_trips: 1,
        round_tripped_request_ids: BTreeSet::from([Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)]),
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
    let sequence = ForcedToolSequence::new(Some(GIT_STATUS_NAME));

    assert_eq!(
        sequence.next(),
        ForcedToolOperation::Force(RuntimeToolName::new(GIT_STATUS_NAME))
    );
    assert_eq!(sequence.next(), ForcedToolOperation::Continuation);
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
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(READ_FILE_NAME),
                arguments_text: serde_json::json!({"path": WORKSPACE_SEED_PATH}).to_string(),
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
                attempt_succeeded: true,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

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
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
                )?,
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                )?,
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
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                )?,
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"paths": [GIT_STAGE_PATH]}).to_string(),
                )?,
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
                    &serde_json::json!({"query": WEB_QUERY}).to_string(),
                )?,
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_FETCH_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"url": WEB_URL}).to_string(),
                )?,
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
                arguments_text: serde_json::json!({"url": WEB_URL}).to_string(),
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
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_SEARCH_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"query": WEB_QUERY}).to_string(),
                )?,
                attempt_succeeded: true,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_FETCH_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"url": WEB_URL}).to_string(),
                )?,
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
            outcome.result_round_trips,
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
        report.natural.result_round_trips,
        report.natural_state.label(),
        report.natural.snapshot.turn_disposition.label(),
    ));
    fs::write(summary_path, &markdown)?;
    print!("{markdown}");
    Ok(())
}
