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
    local_test_connection_options, migrate, model_execution::PostgresModelCallRepository,
    scheduler::PostgresEligibilitySweep, start_eligible_turn::StartEligibleTurnRepository,
};
use signalbox_process_protocol::{
    CanonicalU64, CanonicalUuid, ClientFrame, ClientRequest, CommandId, InputContent,
    ModelSelection, ProtocolVersion, RequestId, ServerFrame, ServerMessage, SystemPromptMember,
    ToolDecision, TurnState, decode_server_line, encode_client_line,
};
use signalbox_tools_basic::{SESSION_STATUS_UPDATE_NAME, WEB_FETCH_NAME};
use signalbox_tools_conversations::{
    LIST_CONVERSATIONS_NAME, READ_CONVERSATION_NAME, READ_IMPORTED_CONVERSATION_NAME,
    READ_OWN_CONVERSATION_NAME,
};
use signalbox_tools_plan::{PLAN_READ_NAME, PLAN_WRITE_NAME};
use signalboxd::{
    APPLY_PATCH_NAME, ActivatedTurnExecution, CHANGE_REQUEST_COMMENT_NAME,
    CHANGE_REQUEST_RERUN_FAILED_JOBS_NAME, CHANGE_REQUEST_SUMMARY_NAME,
    CHANGE_REQUEST_THREAD_REPLY_NAME, CHANGE_REQUEST_THREAD_RESOLVE_NAME,
    CODE_HOST_CREDENTIAL_REFERENCE, DaemonTools, EDIT_FILE_NAME, FileCredentialAccess,
    GitHubCodeHostTransport, HubModelConfiguration, LIST_DIRECTORY_NAME, LocalProcessListener,
    PULL_REQUEST_METADATA_NAME, PULL_REQUEST_PUBLISH_REVIEW_NAME, ProcessRuntime, READ_FILE_NAME,
    SEARCH_FILES_NAME, SessionTemplateConfiguration, SystemCurrentTimeClock, WRITE_FILE_NAME,
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
const FIXTURE_TITLE: &str = "Record owner-ratified interrupt deferral";
const FIXTURE_HEAD_REVISION: &str = "59af8a3634792a30cfb9480bea08cb04acd17bbf";
const SMOKE_ALIAS: u128 = 0x7fde05bcb4c344f78a87748814c80191;
const TRANSCRIPT_MARKER: &str = "live conversation transcript marker";

type SmokeResult<T = ()> = Result<T, Box<dyn Error>>;

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
    fs::write(
        workspace.path().join("seed.txt"),
        "needle from the real workspace\n",
    )?;
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
    let web_fetch_egress_policy = model_configuration.web_fetch_egress_policy();

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
    let tools = DaemonTools::try_new_production(
        SystemCurrentTimeClock,
        pool.clone(),
        credentials,
        GitHubCodeHostTransport::try_new()?,
        github_egress_policy,
        &configured_workspace,
        web_fetch_egress_policy,
    )?;
    let (tool_catalog, tool_executor) = tools.into_parts();
    assert_confirm_inventory(&tool_catalog);

    let scripts = smoke_scripts(session);
    let scripted = ScriptedModel::<ModelCallId>::following(scripts);
    let probe = scripted.clone();
    let provider = RuntimeModelCallProvider::new(scripted, runtime_models);
    let execution = signalboxd::PostgresProviderModelExecution::new(
        PostgresModelCallRepository::new(
            pool.clone(),
            model_targets,
            ModelCallCredentialReference::new("scripted-live-tool-exercise"),
        ),
        InProcessAttemptDispatchGate::default(),
        provider,
    )
    .with_tool_loop(tool_gate, tool_catalog, tool_executor);

    run_automatic_turn(
        &pool,
        &mut connection,
        session,
        "exercise workspace read list search",
        &execution,
    )
    .await?;
    assert_workspace_read_results(&latest_tool_results(&probe, 3)?)?;

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
    assert_eq!(fs::read(workspace.path().join("staged.txt"))?, b"gamma\n");
    assert_eq!(tool_attempt_count(&pool, mutation_turn).await?, 3);

    run_automatic_turn(
        &pool,
        &mut connection,
        session,
        "read back workspace bytes",
        &execution,
    )
    .await?;
    assert_workspace_readback(&latest_tool_results(&probe, 1)?)?;

    run_automatic_turn(
        &pool,
        &mut connection,
        session,
        "exercise code host live read",
        &execution,
    )
    .await?;
    assert_change_request_summary(&latest_tool_results(&probe, 1)?)?;

    run_automatic_turn(
        &pool,
        &mut connection,
        session,
        "exercise github live read",
        &execution,
    )
    .await?;
    assert_github_metadata(&latest_tool_results(&probe, 1)?)?;

    run_automatic_turn(
        &pool,
        &mut connection,
        session,
        TRANSCRIPT_MARKER,
        &execution,
    )
    .await?;
    assert_own_transcript(&latest_tool_results(&probe, 1)?)?;

    run_automatic_turn(
        &pool,
        &mut connection,
        session,
        "exercise durable plan fold",
        &execution,
    )
    .await?;
    assert_plan_fold(&latest_tool_results(&probe, 5)?)?;

    run_automatic_turn(
        &pool,
        &mut connection,
        session,
        "exercise allowed web fetch",
        &execution,
    )
    .await?;
    assert_web_fetch(&latest_tool_results(&probe, 1)?)?;

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
        &confirm_tool_names(),
        DecisionPosture::Deny,
        &execution,
    )
    .await?;
    assert_eq!(tool_attempt_count(&pool, gate_turn).await?, 0);
    assert!(!workspace.path().join("denied.txt").exists());
    assert!(!workspace.path().join("denied-patch.txt").exists());

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

[[adapter_mappings]]
model_family = "codex"
adapter = "codex_cli"
credential_profile = "codex-subscription-primary"

[codex_cli]
executable = "{}"
working_directory = "{}"

[compaction]
prompt = "Summarize the prior conversation faithfully for continuation."

[web_fetch]
allowed_origins = ["https://example.com"]

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
"#,
        executable.display(),
        workspace.display(),
        workspace.display(),
    );
    Ok(HubModelConfiguration::parse(&configuration)?)
}

fn session_template_configuration(
    models: &HubModelConfiguration,
) -> SmokeResult<SessionTemplateConfiguration> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/session-templates.example.toml");
    Ok(SessionTemplateConfiguration::read(&path, || None, models)?)
}

fn smoke_scripts(session: CanonicalUuid) -> Vec<Script> {
    let workspace_reads = vec![
        call(
            READ_FILE_NAME,
            json!({"path": "seed.txt", "max_bytes": 1024}),
        ),
        call(LIST_DIRECTORY_NAME, json!({"path": ".", "max_results": 10})),
        call(
            SEARCH_FILES_NAME,
            json!({"path": ".", "pattern": "needle", "max_results": 10}),
        ),
    ];
    let workspace_mutations = vec![
        call(
            WRITE_FILE_NAME,
            json!({"path": "staged.txt", "content": "alpha\n"}),
        ),
        call(
            EDIT_FILE_NAME,
            json!({
                "path": "staged.txt",
                "old_string": "alpha",
                "new_string": "beta",
                "replace_all": false,
            }),
        ),
        call(
            APPLY_PATCH_NAME,
            json!({
                "patch": "*** Begin Patch\n*** Update File: staged.txt\n@@\n-beta\n+gamma\n*** End Patch"
            }),
        ),
    ];
    let workspace_readback = vec![call(
        READ_FILE_NAME,
        json!({"path": "staged.txt", "max_bytes": 1024}),
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
    let plan = vec![
        call(
            PLAN_WRITE_NAME,
            json!({"kind": "create", "text": "exercise every wired family"}),
        ),
        call(
            PLAN_WRITE_NAME,
            json!({"kind": "create", "text": "deny every external gate"}),
        ),
        call(
            PLAN_WRITE_NAME,
            json!({"kind": "set_status", "entry_id": 1, "status": "in_progress"}),
        ),
        call(
            PLAN_WRITE_NAME,
            json!({"kind": "set_status", "entry_id": 2, "status": "completed"}),
        ),
        call(
            PLAN_READ_NAME,
            json!({"after_entry_id": null, "include_history": true}),
        ),
    ];
    let web = vec![call(WEB_FETCH_NAME, json!({"url": "https://example.com/"}))];
    let gates = confirm_calls(session);

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
        tool_use_script(&plan),
        completion_script("plan fold observed"),
        tool_use_script(&web),
        completion_script("web fetch observed"),
        tool_use_script(&gates),
        completion_script("all gated requests denied"),
    ]
}

fn call(name: &str, arguments: Value) -> (String, String) {
    (name.to_owned(), arguments.to_string())
}

fn tool_use_script(calls: &[(String, String)]) -> Script {
    let content = calls
        .iter()
        .enumerate()
        .map(|(ordinal, (name, arguments))| {
            AssistantPart::ToolCall(ToolCallProposal {
                id: ToolCallId::new(format!("live-call-{ordinal}")),
                name: ToolName::new(name),
                arguments_json: arguments.clone(),
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

fn confirm_tool_names() -> Vec<&'static str> {
    vec![
        APPLY_PATCH_NAME,
        CHANGE_REQUEST_COMMENT_NAME,
        CHANGE_REQUEST_RERUN_FAILED_JOBS_NAME,
        CHANGE_REQUEST_THREAD_REPLY_NAME,
        CHANGE_REQUEST_THREAD_RESOLVE_NAME,
        EDIT_FILE_NAME,
        PULL_REQUEST_PUBLISH_REVIEW_NAME,
        LIST_CONVERSATIONS_NAME,
        READ_CONVERSATION_NAME,
        READ_IMPORTED_CONVERSATION_NAME,
        SESSION_STATUS_UPDATE_NAME,
        WRITE_FILE_NAME,
    ]
}

fn confirm_calls(session: CanonicalUuid) -> Vec<(String, String)> {
    vec![
        call(
            APPLY_PATCH_NAME,
            json!({"patch": "*** Begin Patch\n*** Add File: denied-patch.txt\n+denied\n*** End Patch"}),
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
            json!({"thread_id": "PRRT_kwDOTWhy-86SHVJQ", "body": "must remain denied"}),
        ),
        call(
            CHANGE_REQUEST_THREAD_RESOLVE_NAME,
            json!({"thread_id": "PRRT_kwDOTWhy-86SHVJQ"}),
        ),
        call(
            EDIT_FILE_NAME,
            json!({"path": "seed.txt", "old_string": "needle", "new_string": "forbidden", "replace_all": false}),
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
            SESSION_STATUS_UPDATE_NAME,
            json!({"title": "denied", "tags": [], "attributes": {}, "archived": false}),
        ),
        call(
            WRITE_FILE_NAME,
            json!({"path": "denied.txt", "content": "denied\n"}),
        ),
    ]
}

fn assert_confirm_inventory(catalog: &impl ToolCatalog) {
    let actual = catalog
        .definitions()
        .iter()
        .filter(|definition| definition.permission_default() == ToolPermissionDefault::Confirm)
        .map(|definition| definition.name().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(actual, confirm_tool_names());
}

fn latest_tool_results(
    model: &ScriptedModel<ModelCallId>,
    expected_count: usize,
) -> SmokeResult<Vec<Value>> {
    let operations = model.received_operations();
    let continuation = operations
        .last()
        .ok_or_else(|| io::Error::other("the continuation model operation was not received"))?;
    let mut results = continuation
        .messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            MessagePart::ToolResult(result) => Some(serde_json::from_str(&result.content)),
            MessagePart::Text(_)
            | MessagePart::ToolCall(_)
            | MessagePart::Thinking { .. }
            | MessagePart::RedactedThinking { .. } => None,
        })
        .collect::<Result<Vec<_>, _>>()?;
    if results.len() < expected_count {
        return Err(io::Error::other("the continuation has too few tool results").into());
    }
    Ok(results.split_off(results.len() - expected_count))
}

fn assert_workspace_read_results(results: &[Value]) -> SmokeResult {
    let [read, list, search] = results else {
        return Err(
            io::Error::other("workspace read round returned the wrong result count").into(),
        );
    };
    assert_eq!(read["content"], "needle from the real workspace\n");
    assert_eq!(read["truncated"], false);
    assert_eq!(list["entries"][0]["path"], "seed.txt");
    assert_eq!(search["matches"][0]["path"], "seed.txt");
    assert_eq!(search["matches"][0]["line"], 1);
    Ok(())
}

fn assert_workspace_readback(results: &[Value]) -> SmokeResult {
    let [read] = results else {
        return Err(io::Error::other("workspace readback returned the wrong result count").into());
    };
    assert_eq!(read["path"], "staged.txt");
    assert_eq!(read["content"], "gamma\n");
    assert_eq!(read["bytes_read"], 6);
    assert_eq!(read["total_bytes"], 6);
    Ok(())
}

fn assert_change_request_summary(results: &[Value]) -> SmokeResult {
    let [summary] = results else {
        return Err(io::Error::other("code-host round returned the wrong result count").into());
    };
    assert_eq!(summary["number"], FIXTURE_PULL_REQUEST);
    assert_eq!(summary["title"], FIXTURE_TITLE);
    assert_eq!(summary["state"], "closed");
    assert_eq!(summary["head_revision"], FIXTURE_HEAD_REVISION);
    Ok(())
}

fn assert_github_metadata(results: &[Value]) -> SmokeResult {
    let [metadata] = results else {
        return Err(io::Error::other("GitHub round returned the wrong result count").into());
    };
    assert_eq!(metadata["number"], FIXTURE_PULL_REQUEST);
    assert_eq!(metadata["title"], FIXTURE_TITLE);
    assert_eq!(metadata["state"], "closed");
    assert_eq!(metadata["head_revision"], FIXTURE_HEAD_REVISION);
    Ok(())
}

fn assert_own_transcript(results: &[Value]) -> SmokeResult {
    let [transcript] = results else {
        return Err(io::Error::other("conversation round returned the wrong result count").into());
    };
    let visible = transcript["entries"].as_array().is_some_and(|entries| {
        entries
            .iter()
            .any(|entry| entry["content"] == TRANSCRIPT_MARKER)
    });
    assert!(
        visible,
        "the own-conversation tool must return the smoke turn's real input"
    );
    Ok(())
}

fn assert_plan_fold(results: &[Value]) -> SmokeResult {
    let [_, _, _, _, plan] = results else {
        return Err(io::Error::other("plan round returned the wrong result count").into());
    };
    assert_eq!(plan["entries"][0]["entry_id"], 1);
    assert_eq!(plan["entries"][0]["text"], "exercise every wired family");
    assert_eq!(plan["entries"][0]["status"], "in_progress");
    assert_eq!(plan["entries"][1]["entry_id"], 2);
    assert_eq!(plan["entries"][1]["text"], "deny every external gate");
    assert_eq!(plan["entries"][1]["status"], "completed");
    assert_eq!(plan["history"].as_array().map(Vec::len), Some(4));
    Ok(())
}

fn assert_web_fetch(results: &[Value]) -> SmokeResult {
    let [fetch] = results else {
        return Err(io::Error::other("web-fetch round returned the wrong result count").into());
    };
    assert_eq!(fetch["url"], "https://example.com/");
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
) -> SmokeResult<Box<signalbox_domain::ActivatedAcceptedInputTurn>> {
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
            system_prompt: SystemPromptMember::present(None),
        })
        .await?;
    let response = connection.response_within().await?;
    match response.message() {
        ServerMessage::SessionCreated { session_id } => Ok(*session_id),
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
            content: InputContent::new(content.to_owned()),
            expected_defaults_version: Some(CanonicalU64::new(1)),
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
        .with_fsync_enabled()
        .with_tag(POSTGRES_IMAGE_TAG)
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
