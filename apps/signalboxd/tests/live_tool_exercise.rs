//! Live whole-daemon exercise of every wired tool family with a scripted model.
//!
//! The test is ignored by default and skips when `GITHUB_TOKEN` is absent. It
//! spends no model credentials: a deterministic `ScriptedModel` drives the
//! production tool catalog through the real PostgreSQL tool loop and process
//! protocol. The only approved effects are mutations inside a temporary
//! workspace; every confirm-default tool is separately parked and denied.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone live integration smoke uses explicit fixture assertions"
)]

mod support;

use std::{
    error::Error,
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{Value, json};
use signalbox_application::{
    InProcessAttemptDispatchGate, InProcessEligibilityWorkSource, InProcessToolDispatchGate,
    ModelCallCredentialReference, StartEligibleTurnOutcome, StartEligibleTurnService, ToolCatalog,
    UuidV7StartEligibleTurnIdGenerator,
};
use signalbox_domain::{ModelCallId, SessionId, ToolPermissionDefault, TurnId};
use signalbox_model_provider_runtime::RuntimeModelCallProvider;
use signalbox_model_runtime::{
    AssistantPart, CompletionEvidence, CompletionFinish, CredentialReference, ExchangeFacts,
    MessagePart, ProviderReportedModel, Script, ScriptedModel, TerminalEvidence, TokenUsage,
    ToolCallId, ToolCallProposal, ToolName,
};
use signalbox_persistence::{
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
    model_execution::PostgresModelCallRepository, scheduler::PostgresEligibilitySweep,
    start_eligible_turn::StartEligibleTurnRepository,
};
use signalbox_process_protocol::{
    CanonicalU64, CanonicalUuid, ClientFrame, ClientRequest, CommandId, ModelSelection,
    ModelSettingsOverlay, ProtocolVersion, RequestId, ServerFrame, ServerMessage, SessionPlacement,
    SystemPromptMember, ToolDecision, TurnState, UserInputContent, decode_server_line,
    encode_client_line,
};
use signalbox_tools_basic::SESSION_STATUS_UPDATE_NAME;
use signalbox_tools_code_host::CodeHostNumericBounds;
use signalbox_tools_conversations::{
    LIST_CONVERSATIONS_NAME, READ_CONVERSATION_NAME, READ_IMPORTED_CONVERSATION_NAME,
    READ_OWN_CONVERSATION_NAME,
};
use signalbox_tools_exec::{SANDBOXED_EXEC_NAME, UNSANDBOXED_EXEC_NAME};
use signalbox_tools_plan::{PLAN_READ_NAME, PLAN_WRITE_NAME};
use signalbox_tools_web::{BRAVE_SEARCH_CREDENTIAL_REFERENCE, WEB_FETCH_NAME, WEB_SEARCH_NAME};
use signalboxd::{
    APPLY_PATCH_NAME, ActivatedTurnExecution, CHANGE_REQUEST_COMMENT_NAME,
    CHANGE_REQUEST_RERUN_FAILED_JOBS_NAME, CHANGE_REQUEST_SUMMARY_NAME,
    CHANGE_REQUEST_THREAD_REPLY_NAME, CHANGE_REQUEST_THREAD_RESOLVE_NAME,
    CODE_HOST_CREDENTIAL_REFERENCE, DaemonTools, EDIT_FILE_NAME, FileCredentialAccess,
    GitHubCodeHostTransport, HubModelConfiguration, LIST_DIRECTORY_NAME, LocalProcessListener,
    MappedDaemonCredentialInputs, PULL_REQUEST_METADATA_NAME, PULL_REQUEST_PUBLISH_REVIEW_NAME,
    ProcessRuntime, READ_FILE_NAME, SEARCH_FILES_NAME, SessionTemplateConfiguration,
    SystemCurrentTimeClock, WRITE_FILE_NAME,
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::watch,
    time::timeout,
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_live_tool_exercise";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const FIXTURE_REPOSITORY: &str = "KeenWill/signalbox";
const FIXTURE_PULL_REQUEST: u64 = 94;
const FIXTURE_HEAD_REVISION: &str = "59af8a3634792a30cfb9480bea08cb04acd17bbf";
const SMOKE_ALIAS: u128 = 0x7fde05bcb4c344f78a87748814c80191;
const TRANSCRIPT_MARKER: &str = "live conversation transcript marker";
const SEED_PATH: &str = "seed.txt";
const GIT_ADMINISTRATION_DIRECTORY: &str = ".git";
const SEED_CONTENT: &str = "needle from the real workspace\n";
const SEED_PATTERN: &str = "needle";
const STAGED_PATH: &str = "staged.txt";
const STAGED_WRITE_CONTENT: &str = "alpha\n";
const STAGED_EDIT_OLD: &str = "alpha";
const STAGED_EDIT_NEW: &str = "beta";
const STAGED_PATCH: &str =
    "*** Begin Patch\n*** Update File: staged.txt\n@@\n-beta\n+gamma\n*** End Patch";
const STAGED_FINAL_CONTENT: &str = "gamma\n";
const FIRST_PLAN_TEXT: &str = "exercise every wired family";
const SECOND_PLAN_TEXT: &str = "deny every external gate";
const FIRST_PLAN_STATUS: &str = "in_progress";
const SECOND_PLAN_STATUS: &str = "completed";
const WEB_ORIGIN: &str = "https://example.com";
const WEB_URL: &str = "https://example.com/";
const UNUSED_WEB_SEARCH_CREDENTIAL_FILE: &str = "unused-brave-key";
const DENIED_WEB_SEARCH_QUERY: &str = "synthetic denied search";
const DENIED_UNSANDBOXED_PROGRAM: &str = "/bin/false";
const DENIED_SANDBOXED_PROGRAM: &str = "false";
const DENIED_WRITE_PATH: &str = "denied.txt";
const DENIED_PATCH_PATH: &str = "denied-patch.txt";

type SmokeResult<T = ()> = Result<T, Box<dyn Error>>;

struct PlanEntryFixture {
    entry_id: u64,
    text: &'static str,
    status: &'static str,
}

struct ScriptedToolCall {
    name: String,
    arguments_json: String,
}

struct PlanFixture {
    calls: Vec<ScriptedToolCall>,
    entries: [PlanEntryFixture; 2],
}

impl PlanFixture {
    fn new() -> Self {
        let entries = [
            PlanEntryFixture {
                entry_id: 1,
                text: FIRST_PLAN_TEXT,
                status: FIRST_PLAN_STATUS,
            },
            PlanEntryFixture {
                entry_id: 2,
                text: SECOND_PLAN_TEXT,
                status: SECOND_PLAN_STATUS,
            },
        ];
        let calls = vec![
            call(
                PLAN_WRITE_NAME,
                json!({"kind": "create", "text": entries[0].text}),
            ),
            call(
                PLAN_WRITE_NAME,
                json!({"kind": "create", "text": entries[1].text}),
            ),
            call(
                PLAN_WRITE_NAME,
                json!({"kind": "set_status", "entry_id": entries[0].entry_id, "status": entries[0].status}),
            ),
            call(
                PLAN_WRITE_NAME,
                json!({"kind": "set_status", "entry_id": entries[1].entry_id, "status": entries[1].status}),
            ),
            call(
                PLAN_READ_NAME,
                json!({"after_entry_id": null, "include_history": true}),
            ),
        ];
        Self { calls, entries }
    }

    fn history_count(&self) -> usize {
        self.calls
            .iter()
            .filter(|call| call.name == PLAN_WRITE_NAME)
            .count()
    }
}

#[test]
#[ignore = "uses ephemeral PostgreSQL, a local Unix socket, single-digit public HTTP requests, and GITHUB_TOKEN"]
fn live_daemon_executes_every_tool_family_and_denies_every_gate() -> SmokeResult {
    let outcome = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime
                .block_on(run_live_smoke())
                .map_err(|error| error.to_string())
        })?
        .join()
        .map_err(|_| io::Error::other("the live smoke thread panicked"))?;
    outcome.map_err(io::Error::other)?;
    Ok(())
}

async fn run_live_smoke() -> SmokeResult {
    let Some(token) = std::env::var_os("GITHUB_TOKEN")
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };

    let workspace = tempfile::tempdir()?;
    git2::Repository::init(workspace.path())?;
    fs::write(workspace.path().join(SEED_PATH), SEED_CONTENT)?;
    let credential_directory = tempfile::tempdir()?;
    let credential_file = credential_directory.path().join("github-token");
    fs::write(&credential_file, token)?;
    fs::set_permissions(&credential_file, fs::Permissions::from_mode(0o600))?;

    let (container, pool) = migrated_postgres().await?;
    let socket_directory = tempfile::tempdir()?;
    fs::set_permissions(socket_directory.path(), fs::Permissions::from_mode(0o700))?;
    let socket = socket_directory.path().join("signalboxd.sock");
    let model_configuration = smoke_configuration(workspace.path())?;
    let runtime_models = model_configuration.runtime_model_catalog();
    let model_targets = model_configuration.target_catalog();
    let template_configuration = session_template_configuration(&model_configuration)?;
    let daemon_configuration = model_configuration
        .daemon_tools()
        .expect("the smoke configuration wires every deployment-owned family");
    let github_egress_policy = daemon_configuration.github_egress_policy();
    let configured_workspace = daemon_configuration.workspace_root().to_path_buf();
    let git_identity = daemon_configuration.git_identity().clone();
    let exec_supervisor_executable = daemon_configuration
        .exec_supervisor_executable()
        .to_path_buf();
    let web_fetch_egress_policy = model_configuration.web_fetch_egress_policy();
    let numeric_bounds = model_configuration.numeric_bounds();
    let configured_usize = |field| {
        numeric_bounds
            .integer(field)
            .flatten()
            .map(usize::try_from)
            .transpose()
    };
    let code_host_numeric_bounds = CodeHostNumericBounds::new(
        numeric_bounds
            .duration("code_host_request_timeout")
            .flatten(),
        configured_usize("max_job_log_bytes")?,
        configured_usize("max_stack_comparisons_in_flight")?,
        configured_usize("max_code_host_result_text_bytes")?,
        configured_usize("max_code_host_result_items")?,
        configured_usize("max_repository_file_content_bytes")?,
    );

    let listener = LocalProcessListener::bind(&socket)?;
    let (eligibility_nudge, _work_source) =
        InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(pool.clone()));
    let tool_gate = InProcessToolDispatchGate::default();
    let runtime = ProcessRuntime::new_with_templates(
        listener,
        pool.clone(),
        eligibility_nudge,
        tool_gate.clone(),
        model_configuration,
        template_configuration,
    );
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let runtime_task = tokio::spawn(runtime.run(shutdown_receiver));
    let mut connection = Connection::connect(&socket).await?;
    let session = create_session(&mut connection).await?;

    let credentials = FileCredentialAccess::new(
        credential_file,
        CredentialReference::new(CODE_HOST_CREDENTIAL_REFERENCE),
    );
    let web_search_credentials = FileCredentialAccess::new(
        credential_directory
            .path()
            .join(UNUSED_WEB_SEARCH_CREDENTIAL_FILE),
        CredentialReference::new(BRAVE_SEARCH_CREDENTIAL_REFERENCE),
    );
    let tools = DaemonTools::try_new_production(
        SystemCurrentTimeClock,
        pool.clone(),
        MappedDaemonCredentialInputs {
            web_search: web_search_credentials,
            code_host: credentials.clone(),
            github: credentials,
        },
        GitHubCodeHostTransport::try_new(code_host_numeric_bounds)?,
        github_egress_policy,
        &configured_workspace,
        git_identity,
        &exec_supervisor_executable,
        None,
        web_fetch_egress_policy,
    )?;
    let (tool_catalog, tool_executor) = tools.into_parts();
    let confirm_names = confirm_tool_names(&tool_catalog);
    let confirm_name_refs = confirm_names.iter().map(String::as_str).collect::<Vec<_>>();
    let gate_calls = confirm_calls(session);
    assert_confirm_calls(&gate_calls, &confirm_name_refs);
    let plan_fixture = PlanFixture::new();

    let scripts = smoke_scripts(&plan_fixture, &gate_calls);
    let expected_model_operations = scripts.len();
    let scripted = ScriptedModel::<ModelCallId>::following(scripts);
    let probe = scripted.clone();
    let provider = RuntimeModelCallProvider::new(scripted, runtime_models, None);
    let execution = signalboxd::PostgresProviderModelExecution::new(
        PostgresModelCallRepository::new(
            pool.clone(),
            model_targets,
            ModelCallCredentialReference::new("scripted-live-tool-exercise"),
        ),
        InProcessAttemptDispatchGate::default(),
        provider,
        None,
    )
    .with_tool_loop(tool_gate, tool_catalog, tool_executor)
    .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
        pool.clone(),
        None,
        Vec::new(),
    ));

    run_automatic_turn(
        &pool,
        &mut connection,
        session,
        "exercise workspace read list search",
        &execution,
    )
    .await?;
    assert_workspace_read_results(&current_tool_results(&probe)?)?;

    let mutation_turn = run_parking_turn(
        &pool,
        &mut connection,
        session,
        "exercise workspace write edit patch",
        &execution,
    )
    .await?;
    decide_requests(
        &pool,
        &mut connection,
        session,
        mutation_turn,
        &[WRITE_FILE_NAME, EDIT_FILE_NAME, APPLY_PATCH_NAME],
        DecisionPosture::Approve,
        &execution,
    )
    .await?;
    assert_eq!(
        fs::read(workspace.path().join(STAGED_PATH))?,
        STAGED_FINAL_CONTENT.as_bytes()
    );
    assert_eq!(tool_attempt_count(&pool, mutation_turn).await?, 3);

    run_automatic_turn(
        &pool,
        &mut connection,
        session,
        "read back workspace bytes",
        &execution,
    )
    .await?;
    assert_workspace_readback(&current_tool_results(&probe)?)?;

    run_automatic_turn(
        &pool,
        &mut connection,
        session,
        "exercise code host live read",
        &execution,
    )
    .await?;
    assert_change_request_summary(&current_tool_results(&probe)?)?;

    run_automatic_turn(
        &pool,
        &mut connection,
        session,
        "exercise github live read",
        &execution,
    )
    .await?;
    assert_github_metadata(&current_tool_results(&probe)?)?;

    run_automatic_turn(
        &pool,
        &mut connection,
        session,
        TRANSCRIPT_MARKER,
        &execution,
    )
    .await?;
    assert_own_transcript(&current_tool_results(&probe)?)?;

    run_automatic_turn(
        &pool,
        &mut connection,
        session,
        "exercise durable plan fold",
        &execution,
    )
    .await?;
    assert_plan_fold(&current_tool_results(&probe)?, &plan_fixture)?;

    let web_fetch_turn = run_parking_turn(
        &pool,
        &mut connection,
        session,
        "exercise allowed web fetch",
        &execution,
    )
    .await?;
    decide_requests(
        &pool,
        &mut connection,
        session,
        web_fetch_turn,
        &[WEB_FETCH_NAME],
        DecisionPosture::Approve,
        &execution,
    )
    .await?;
    assert_web_fetch(&current_tool_results(&probe)?)?;

    let gate_turn = run_parking_turn(
        &pool,
        &mut connection,
        session,
        "park and deny every confirm default",
        &execution,
    )
    .await?;
    decide_requests(
        &pool,
        &mut connection,
        session,
        gate_turn,
        &confirm_name_refs,
        DecisionPosture::Deny,
        &execution,
    )
    .await?;
    assert_eq!(probe.received_operations().len(), expected_model_operations);
    assert_eq!(tool_attempt_count(&pool, gate_turn).await?, 0);
    assert!(!workspace.path().join(DENIED_WRITE_PATH).exists());
    assert!(!workspace.path().join(DENIED_PATCH_PATH).exists());

    shutdown.send(true)?;
    timeout(Duration::from_secs(10), runtime_task).await???;
    pool.close().await;
    drop(container);
    drop(credential_directory);
    cleanup_socket_lock(&socket);
    drop(socket_directory);
    Ok(())
}

fn smoke_configuration(workspace: &Path) -> SmokeResult<HubModelConfiguration> {
    let executable = std::env::current_exe()?;
    let configuration = format!(
        r#"version = 1

[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient"

[[credential_pools]]
name = "codex-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{{ profile = "codex-subscription-primary", priority = 1 }}]


[[adapter_mappings]]
model_family = "codex"
adapter = "codex_cli"
credential_pool = "codex-main"

[codex_cli]
executable = "{}"
working_directory = "{}"

[compaction]
prompt = "Summarize the prior conversation faithfully for continuation."

[web_fetch]
allowed_origins = ["{WEB_ORIGIN}"]

[[tool_mappings]]
family = "code_host"
adapter = "github"
credential_profile = "github-primary"
egress_policy = "github_api_only"

[[tool_mappings]]
family = "github"
adapter = "github"
credential_profile = "github-primary"
egress_policy = "github_api_only"

[[tool_mappings]]
family = "workspace"
adapter = "local"
workspace_root = "{}"

[[tool_mappings]]
family = "conversations"
adapter = "application"

[daemon_tools]
exec_supervisor_executable = "{}"

[git_identity]
author_name = "Signalbox Live Smoke"
author_email = "signalbox-live@example.test"

[[models]]
selection_id = "00000000-0000-0000-0000-000000000001"
target_id = "00000000-0000-0000-0000-000000000003"
model_family = "codex"
provider_model = "scripted-live-tool-exercise"
max_output_tokens = 256
context_window_tokens = 200000

[[aliases]]
alias_id = "7fde05bc-b4c3-44f7-8a87-748814c80191"
selection_id = "00000000-0000-0000-0000-000000000001"

[[aliases]]
alias_id = "540ce009-c2ec-4a04-b823-c411ea189778"
selection_id = "00000000-0000-0000-0000-000000000001"
"#,
        executable.display(),
        workspace.display(),
        workspace.display(),
        executable.display(),
    );
    Ok(support::parse_model_configuration(&configuration)?)
}

fn session_template_configuration(
    models: &HubModelConfiguration,
) -> SmokeResult<SessionTemplateConfiguration> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/session-templates.example.toml");
    Ok(SessionTemplateConfiguration::read(&path, || None, models)?)
}

fn smoke_scripts(plan_fixture: &PlanFixture, gate_calls: &[ScriptedToolCall]) -> Vec<Script> {
    let workspace_reads = vec![
        call(
            READ_FILE_NAME,
            json!({"path": SEED_PATH, "max_bytes": 1024}),
        ),
        call(LIST_DIRECTORY_NAME, json!({"path": ".", "max_results": 10})),
        call(
            SEARCH_FILES_NAME,
            json!({"path": ".", "pattern": SEED_PATTERN, "max_results": 10}),
        ),
    ];
    let workspace_mutations = vec![
        call(
            WRITE_FILE_NAME,
            json!({"path": STAGED_PATH, "content": STAGED_WRITE_CONTENT}),
        ),
        call(
            EDIT_FILE_NAME,
            json!({
                "path": STAGED_PATH,
                "old_string": STAGED_EDIT_OLD,
                "new_string": STAGED_EDIT_NEW,
                "replace_all": false,
            }),
        ),
        call(
            APPLY_PATCH_NAME,
            json!({
                "patch": STAGED_PATCH
            }),
        ),
    ];
    let workspace_readback = vec![call(
        READ_FILE_NAME,
        json!({"path": STAGED_PATH, "max_bytes": 1024}),
    )];
    let code_host = vec![call(
        CHANGE_REQUEST_SUMMARY_NAME,
        json!({"repository": FIXTURE_REPOSITORY, "number": FIXTURE_PULL_REQUEST}),
    )];
    let github = vec![call(
        PULL_REQUEST_METADATA_NAME,
        json!({"repository": FIXTURE_REPOSITORY, "number": FIXTURE_PULL_REQUEST}),
    )];
    let conversations = vec![call(
        READ_OWN_CONVERSATION_NAME,
        json!({"after_position": null, "max_entries": 100, "max_bytes": 131072}),
    )];
    let web = vec![call(WEB_FETCH_NAME, json!({"url": WEB_URL}))];

    vec![
        tool_use_script(&workspace_reads),
        completion_script("workspace reads observed"),
        tool_use_script(&workspace_mutations),
        completion_script("workspace mutations observed"),
        tool_use_script(&workspace_readback),
        completion_script("workspace bytes read back"),
        tool_use_script(&code_host),
        completion_script("code host read observed"),
        tool_use_script(&github),
        completion_script("github read observed"),
        tool_use_script(&conversations),
        completion_script("own transcript observed"),
        tool_use_script(&plan_fixture.calls),
        completion_script("plan fold observed"),
        tool_use_script(&web),
        completion_script("web fetch observed"),
        tool_use_script(gate_calls),
        completion_script("all gated requests denied"),
    ]
}

fn call(name: &str, arguments: Value) -> ScriptedToolCall {
    ScriptedToolCall {
        name: name.to_owned(),
        arguments_json: arguments.to_string(),
    }
}

fn tool_use_script(calls: &[ScriptedToolCall]) -> Script {
    let content = calls
        .iter()
        .enumerate()
        .map(|(ordinal, call)| {
            AssistantPart::ToolCall(ToolCallProposal {
                id: ToolCallId::new(format!("live-call-{ordinal}")),
                name: ToolName::new(&call.name),
                arguments_json: call.arguments_json.clone(),
            })
        })
        .collect();
    Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
        exchange: ExchangeFacts::default(),
        message_id: None,
        reported_model: Some(ProviderReportedModel::new("scripted-live-tool-exercise")),
        finish: CompletionFinish::ToolUse,
        content,
        usage: TokenUsage::unreported(),
    }))
}

fn completion_script(text: &str) -> Script {
    Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
        exchange: ExchangeFacts::default(),
        message_id: None,
        reported_model: Some(ProviderReportedModel::new("scripted-live-tool-exercise")),
        finish: CompletionFinish::EndTurn,
        content: vec![AssistantPart::Text(text.to_owned())],
        usage: TokenUsage::unreported(),
    }))
}

/// Names every composed declaration whose permission default parks for a user.
///
/// Both confirming defaults belong here. `AlwaysConfirm` is the stricter of the
/// two — human-only even under the dangerous session blanket — so an exact
/// `Confirm` comparison omits precisely the declarations that most need the
/// gate, and the denied-gate fixture would then be expected to skip them.
/// Whether a permission default puts a tool behind a confirm gate.
///
/// Exhaustive rather than `matches!`: a new `ToolPermissionDefault` must fail
/// to compile here instead of being silently classified as ungated, which is
/// the coverage hole this smoke exists to close.
///
/// Split out from `confirm_tool_names` so ordinary test runs exercise it. That
/// function is reachable only from the ignored, token-gated smoke below, so
/// mapping `AlwaysConfirm` to `false` would otherwise leave the whole suite
/// green while the smoke's expected gated set quietly lost a tool.
fn is_confirm_gated(permission_default: ToolPermissionDefault) -> bool {
    match permission_default {
        ToolPermissionDefault::Confirm | ToolPermissionDefault::AlwaysConfirm => true,
        ToolPermissionDefault::Auto => false,
    }
}

#[test]
fn a_confirm_default_is_confirm_gated() {
    assert!(is_confirm_gated(ToolPermissionDefault::Confirm));
}

#[test]
fn an_always_confirm_default_is_confirm_gated() {
    assert!(is_confirm_gated(ToolPermissionDefault::AlwaysConfirm));
}

#[test]
fn an_auto_default_is_not_confirm_gated() {
    assert!(!is_confirm_gated(ToolPermissionDefault::Auto));
}

fn confirm_tool_names(catalog: &impl ToolCatalog) -> Vec<String> {
    catalog
        .definitions()
        .iter()
        .filter(|definition| is_confirm_gated(definition.permission_default()))
        .map(|definition| definition.name().as_str().to_owned())
        .collect()
}

fn confirm_calls(session: CanonicalUuid) -> Vec<ScriptedToolCall> {
    vec![
        call(
            APPLY_PATCH_NAME,
            json!({"patch": format!("*** Begin Patch\n*** Add File: {DENIED_PATCH_PATH}\n+denied\n*** End Patch")}),
        ),
        call(
            CHANGE_REQUEST_COMMENT_NAME,
            json!({"repository": FIXTURE_REPOSITORY, "number": FIXTURE_PULL_REQUEST, "body": "must remain denied"}),
        ),
        call(
            CHANGE_REQUEST_RERUN_FAILED_JOBS_NAME,
            json!({"repository": FIXTURE_REPOSITORY, "run_id": 1}),
        ),
        call(
            CHANGE_REQUEST_THREAD_REPLY_NAME,
            json!({
                "repository": FIXTURE_REPOSITORY,
                "number": FIXTURE_PULL_REQUEST,
                "thread_id": "PRRT_kwDOTWhy-86SHVJQ",
                "body": "must remain denied",
            }),
        ),
        call(
            CHANGE_REQUEST_THREAD_RESOLVE_NAME,
            json!({
                "repository": FIXTURE_REPOSITORY,
                "number": FIXTURE_PULL_REQUEST,
                "thread_id": "PRRT_kwDOTWhy-86SHVJQ",
            }),
        ),
        call(
            EDIT_FILE_NAME,
            json!({"path": SEED_PATH, "old_string": SEED_PATTERN, "new_string": "forbidden", "replace_all": false}),
        ),
        call(
            PULL_REQUEST_PUBLISH_REVIEW_NAME,
            json!({
                "repository": FIXTURE_REPOSITORY,
                "number": FIXTURE_PULL_REQUEST,
                "commit_id": FIXTURE_HEAD_REVISION,
                "event": "approve",
                "comments": [],
            }),
        ),
        call(
            LIST_CONVERSATIONS_NAME,
            json!({"after": null, "max_results": 10}),
        ),
        call(
            READ_CONVERSATION_NAME,
            json!({
                "session_id": session.into_uuid().to_string(),
                "after_position": null,
                "max_entries": 10,
                "max_bytes": 4096,
            }),
        ),
        call(
            READ_IMPORTED_CONVERSATION_NAME,
            json!({
                "imported_conversation_id": "00000000-0000-0000-0000-000000000099",
                "after_position": null,
                "max_entries": 10,
                "max_bytes": 4096,
            }),
        ),
        call(
            SANDBOXED_EXEC_NAME,
            json!({"program": DENIED_SANDBOXED_PROGRAM}),
        ),
        call(
            SESSION_STATUS_UPDATE_NAME,
            json!({"title": "denied", "tags": [], "attributes": {}, "archived": false}),
        ),
        call(
            UNSANDBOXED_EXEC_NAME,
            json!({"program": DENIED_UNSANDBOXED_PROGRAM}),
        ),
        call(WEB_FETCH_NAME, json!({"url": WEB_URL})),
        call(WEB_SEARCH_NAME, json!({"query": DENIED_WEB_SEARCH_QUERY})),
        call(
            WRITE_FILE_NAME,
            json!({"path": DENIED_WRITE_PATH, "content": "denied\n"}),
        ),
    ]
}

fn assert_confirm_calls(calls: &[ScriptedToolCall], expected_names: &[&str]) {
    let actual_names = calls
        .iter()
        .map(|call| call.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(actual_names, expected_names);
}

fn current_tool_results(model: &ScriptedModel<ModelCallId>) -> SmokeResult<Vec<Value>> {
    let operations = model.received_operations();
    let before = operations
        .iter()
        .rev()
        .nth(1)
        .ok_or_else(|| io::Error::other("the tool-use model operation was not received"))?;
    let continuation = operations
        .last()
        .ok_or_else(|| io::Error::other("the continuation model operation was not received"))?;
    let previous_results = operation_tool_results(before)?;
    let continuation_results = operation_tool_results(continuation)?;
    current_tool_result_delta(previous_results, continuation_results)
}

fn current_tool_result_delta(
    previous_results: Vec<Value>,
    mut continuation_results: Vec<Value>,
) -> SmokeResult<Vec<Value>> {
    if !continuation_results.starts_with(&previous_results) {
        return Err(
            io::Error::other("the continuation did not preserve prior tool results").into(),
        );
    }
    Ok(continuation_results.split_off(previous_results.len()))
}

#[test]
fn current_tool_result_delta_extracts_only_the_current_round() -> SmokeResult {
    let previous = json!({"round": "previous"});
    let current_first = json!({"round": "current", "ordinal": 1});
    let current_second = json!({"round": "current", "ordinal": 2});

    let extracted = current_tool_result_delta(
        vec![previous.clone()],
        vec![previous, current_first.clone(), current_second.clone()],
    )?;

    assert_eq!(extracted, vec![current_first, current_second]);
    Ok(())
}

#[test]
fn current_tool_result_delta_rejects_changed_prior_evidence() {
    let previous = json!({"round": "previous"});
    let changed_previous = json!({"round": "changed"});
    let current = json!({"round": "current"});

    let _error = current_tool_result_delta(vec![previous], vec![changed_previous, current])
        .expect_err("changed prior evidence must be rejected");
}

fn operation_tool_results(
    operation: &signalbox_model_runtime::ModelOperation<ModelCallId>,
) -> SmokeResult<Vec<Value>> {
    Ok(operation
        .messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            MessagePart::ToolResult(result) => Some(serde_json::from_str(&result.content)),
            MessagePart::Text(_)
            | MessagePart::ToolCall(_)
            | MessagePart::Thinking { .. }
            | MessagePart::RedactedThinking { .. }
            | MessagePart::ProviderCompaction { .. } => None,
        })
        .collect::<Result<Vec<_>, _>>()?)
}

fn assert_workspace_read_results(results: &[Value]) -> SmokeResult {
    let [read, list, search] = results else {
        return Err(
            io::Error::other("workspace read round returned the wrong result count").into(),
        );
    };
    assert_eq!(read["content"], SEED_CONTENT);
    assert_eq!(read["truncated"], false);
    assert_eq!(list["entries"][0]["path"], GIT_ADMINISTRATION_DIRECTORY);
    assert_eq!(list["entries"][1]["path"], SEED_PATH);
    assert_eq!(search["matches"][0]["path"], SEED_PATH);
    assert_eq!(search["matches"][0]["line"], 1);
    Ok(())
}

fn assert_workspace_readback(results: &[Value]) -> SmokeResult {
    let [read] = results else {
        return Err(io::Error::other("workspace readback returned the wrong result count").into());
    };
    assert_eq!(read["path"], STAGED_PATH);
    assert_eq!(read["content"], STAGED_FINAL_CONTENT);
    assert_eq!(read["bytes_read"], STAGED_FINAL_CONTENT.len());
    assert_eq!(read["total_bytes"], STAGED_FINAL_CONTENT.len());
    Ok(())
}

fn assert_change_request_summary(results: &[Value]) -> SmokeResult {
    let [summary] = results else {
        return Err(io::Error::other("code-host round returned the wrong result count").into());
    };
    assert_eq!(summary["number"], FIXTURE_PULL_REQUEST);
    assert_eq!(summary["state"], "closed");
    assert_eq!(summary["head_revision"], FIXTURE_HEAD_REVISION);
    Ok(())
}

fn assert_github_metadata(results: &[Value]) -> SmokeResult {
    let [metadata] = results else {
        return Err(io::Error::other("GitHub round returned the wrong result count").into());
    };
    assert_eq!(metadata["number"], FIXTURE_PULL_REQUEST);
    assert_eq!(metadata["state"], "closed");
    assert_eq!(metadata["head_revision"], FIXTURE_HEAD_REVISION);
    Ok(())
}

fn assert_own_transcript(results: &[Value]) -> SmokeResult {
    let [transcript] = results else {
        return Err(io::Error::other("conversation round returned the wrong result count").into());
    };
    let visible = transcript["entries"].as_array().is_some_and(|entries| {
        entries.iter().any(|entry| {
            entry["content"]
                .as_str()
                .and_then(|content| serde_json::from_str::<Value>(content).ok())
                .is_some_and(|content| {
                    content
                        == serde_json::json!([{
                            "type": "text",
                            "text": TRANSCRIPT_MARKER,
                        }])
                })
        })
    });
    assert!(
        visible,
        "the own-conversation tool must return the smoke turn's real input"
    );
    Ok(())
}

fn assert_plan_fold(results: &[Value], fixture: &PlanFixture) -> SmokeResult {
    assert_eq!(results.len(), fixture.calls.len());
    let plan = results
        .last()
        .ok_or_else(|| io::Error::other("plan round returned no read result"))?;
    assert_eq!(plan["entries"][0]["entry_id"], fixture.entries[0].entry_id);
    assert_eq!(plan["entries"][0]["text"], fixture.entries[0].text);
    assert_eq!(plan["entries"][0]["status"], fixture.entries[0].status);
    assert_eq!(plan["entries"][1]["entry_id"], fixture.entries[1].entry_id);
    assert_eq!(plan["entries"][1]["text"], fixture.entries[1].text);
    assert_eq!(plan["entries"][1]["status"], fixture.entries[1].status);
    assert_eq!(
        plan["history"].as_array().map(Vec::len),
        Some(fixture.history_count())
    );
    Ok(())
}

fn assert_web_fetch(results: &[Value]) -> SmokeResult {
    let [fetch] = results else {
        return Err(io::Error::other("web-fetch round returned the wrong result count").into());
    };
    assert_eq!(fetch["url"], WEB_URL);
    assert_eq!(fetch["status"], 200);
    assert_eq!(fetch["truncated"], false);
    assert!(
        fetch["body"]
            .as_str()
            .is_some_and(|body| body.contains("Example Domain"))
    );
    Ok(())
}

async fn run_automatic_turn<Execution>(
    pool: &PgPool,
    connection: &mut Connection,
    session: CanonicalUuid,
    content: &str,
    execution: &Execution,
) -> SmokeResult<TurnId>
where
    Execution: ActivatedTurnExecution,
    Execution::Error: Error + 'static,
{
    let turn = submit_turn(connection, session, content).await?;
    let activated = activate_turn(pool, session, turn).await?;
    execution.execute(activated).await?;
    Ok(turn)
}

async fn run_parking_turn<Execution>(
    pool: &PgPool,
    connection: &mut Connection,
    session: CanonicalUuid,
    content: &str,
    execution: &Execution,
) -> SmokeResult<TurnId>
where
    Execution: ActivatedTurnExecution,
    Execution::Error: Error + 'static,
{
    run_automatic_turn(pool, connection, session, content, execution).await
}

async fn activate_turn(
    pool: &PgPool,
    session: CanonicalUuid,
    expected_turn: TurnId,
) -> SmokeResult<Box<signalbox_domain::ActivatedTurn>> {
    let mut service = StartEligibleTurnService::new(
        UuidV7StartEligibleTurnIdGenerator,
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) = service
        .execute(SessionId::from_uuid(session.into_uuid()))
        .await?
    else {
        return Err(io::Error::other("the submitted smoke turn did not activate").into());
    };
    assert_eq!(activated.turn(), expected_turn);
    Ok(activated)
}

#[derive(Clone, Copy)]
enum DecisionPosture {
    Approve,
    Deny,
}

async fn decide_requests<Execution>(
    pool: &PgPool,
    connection: &mut Connection,
    session: CanonicalUuid,
    turn: TurnId,
    expected_names: &[&str],
    posture: DecisionPosture,
    execution: &Execution,
) -> SmokeResult
where
    Execution: ActivatedTurnExecution,
    Execution::Error: Error + 'static,
{
    let requests = tool_requests(pool, turn).await?;
    assert_eq!(
        requests
            .iter()
            .map(|(_, name)| name.as_str())
            .collect::<Vec<_>>(),
        expected_names
    );
    for (request, _) in requests {
        assert_pending_approval(connection, session, turn, request).await?;
        let decision = match posture {
            DecisionPosture::Approve => ToolDecision::Approve {},
            DecisionPosture::Deny => ToolDecision::Deny {
                reason: String::from("live smoke denies external execution"),
            },
        };
        decide_tool_request(connection, session, request, decision.clone()).await?;
        execution
            .resume_active(SessionId::from_uuid(session.into_uuid()))
            .await?;
    }
    Ok(())
}

async fn tool_requests(pool: &PgPool, turn: TurnId) -> SmokeResult<Vec<(CanonicalUuid, String)>> {
    let rows = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT request_id, tool_name
           FROM tool_request
          WHERE turn_id = $1
          ORDER BY request_ordinal",
    )
    .bind(turn.into_uuid())
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(request, name)| (CanonicalUuid::from_uuid(request), name))
        .collect())
}

async fn tool_attempt_count(pool: &PgPool, turn: TurnId) -> SmokeResult<i64> {
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM tool_attempt WHERE turn_id = $1")
            .bind(turn.into_uuid())
            .fetch_one(pool)
            .await?,
    )
}

async fn assert_pending_approval(
    connection: &mut Connection,
    session: CanonicalUuid,
    turn: TurnId,
    request: CanonicalUuid,
) -> SmokeResult {
    connection
        .send(ClientRequest::ReadTranscript {
            session_id: session,
        })
        .await?;
    let start = connection.response_within().await?;
    assert!(matches!(
        start.message(),
        ServerMessage::TranscriptSnapshotStart { session_id, .. } if *session_id == session
    ));
    let mut pending = false;
    loop {
        let frame = connection.response_within().await?;
        match frame.message() {
            ServerMessage::TranscriptTurn {
                turn_id,
                state: TurnState::ActiveAwaitingToolApproval { tool_request_id },
                ..
            } if *turn_id == CanonicalUuid::from_uuid(turn.into_uuid())
                && *tool_request_id == request =>
            {
                pending = true;
            }
            ServerMessage::TranscriptSnapshotEnd { session_id, .. } if *session_id == session => {
                assert!(
                    pending,
                    "the process protocol must expose the pending approval"
                );
                return Ok(());
            }
            _ => {}
        }
    }
}

async fn decide_tool_request(
    connection: &mut Connection,
    session: CanonicalUuid,
    request: CanonicalUuid,
    decision: ToolDecision,
) -> SmokeResult {
    connection
        .send(ClientRequest::DecideToolRequest {
            command_id: command()?,
            session_id: session,
            tool_request_id: request,
            decision: decision.clone(),
        })
        .await?;
    let receipt = connection.response_within().await?;
    assert!(matches!(
        receipt.message(),
        ServerMessage::ToolRequestDecided {
            tool_request_id,
            decision: recorded,
            ..
        } if *tool_request_id == request && *recorded == decision
    ));
    Ok(())
}

async fn create_session(connection: &mut Connection) -> SmokeResult<CanonicalUuid> {
    connection
        .send(ClientRequest::CreateSession {
            command_id: command()?,
            initial_model_selection: ModelSelection::Alias {
                alias_id: CanonicalUuid::from_uuid(Uuid::from_u128(SMOKE_ALIAS)),
            },
            model_settings: ModelSettingsOverlay::inherit_all(),
            system_prompt: SystemPromptMember::present(None),
            placement: SessionPlacement::Pathless {},
            lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
        })
        .await?;
    let response = connection.response_within().await?;
    match response.message() {
        ServerMessage::SessionCreated { session_id, .. } => Ok(*session_id),
        message => {
            Err(io::Error::other(format!("unexpected create-session response: {message:?}")).into())
        }
    }
}

async fn submit_turn(
    connection: &mut Connection,
    session: CanonicalUuid,
    content: &str,
) -> SmokeResult<TurnId> {
    connection
        .send(ClientRequest::SubmitInput {
            command_id: command()?,
            session_id: session,
            content: UserInputContent::text(content.to_owned()),
            expected_defaults_version: Some(CanonicalU64::new(1)),
            model_settings: ModelSettingsOverlay::inherit_all(),
            delivery: None,
        })
        .await?;
    let response = connection.response_within().await?;
    match response.message() {
        ServerMessage::InputSubmitted {
            session_id,
            turn_id,
            ..
        } if *session_id == session => Ok(TurnId::from_uuid(turn_id.into_uuid())),
        message => {
            Err(io::Error::other(format!("unexpected submit-input response: {message:?}")).into())
        }
    }
}

struct Connection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_request: u64,
}

impl Connection {
    async fn connect(path: &Path) -> SmokeResult<Self> {
        let stream = UnixStream::connect(path).await?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
            next_request: 1,
        })
    }

    async fn send(&mut self, request: ClientRequest) -> SmokeResult {
        let request_id = RequestId::try_new(self.next_request)?;
        self.next_request = self.next_request.saturating_add(1);
        let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request)?;
        self.writer.write_all(&encode_client_line(&frame)?).await?;
        Ok(())
    }

    async fn response_within(&mut self) -> SmokeResult<ServerFrame> {
        timeout(Duration::from_secs(10), self.response()).await?
    }

    async fn response(&mut self) -> SmokeResult<ServerFrame> {
        let mut line = Vec::new();
        if self.reader.read_until(b'\n', &mut line).await? == 0 {
            return Err(
                io::Error::other("the process protocol closed before its next frame").into(),
            );
        }
        Ok(decode_server_line(&line)?)
    }
}

fn command() -> SmokeResult<CommandId> {
    Ok(CommandId::try_from_uuid(Uuid::now_v7())?)
}

async fn migrated_postgres() -> SmokeResult<(ContainerAsync<Postgres>, PgPool)> {
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
    let port = container.get_host_port_ipv4(5432).await?;
    let database_url =
        format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

fn cleanup_socket_lock(socket: &Path) {
    let mut lock = PathBuf::from(socket).into_os_string();
    lock.push(".lock");
    let _ = fs::remove_file(PathBuf::from(lock));
}
