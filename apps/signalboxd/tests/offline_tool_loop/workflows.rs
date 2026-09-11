//! Workflow tools exercised through the ordinary provider/tool loop.
use super::*;
use signalbox_domain::program_registration::{ProgramGrants, ProgramRegistrationRequest};
use signalbox_domain::{
    ProgramRegistrationId, ProgramRunId, ToolRequest, ToolRequestOrdinal,
    ToolRequestReconstitutionInput,
};
use signalbox_tools_workflows::{
    RegisterArguments, StartArguments, WorkflowExecutor, WorkflowPort, WorkflowRequest,
};
use signalboxd::configuration_reload::ConfigurationReload;
use signalboxd::workflows::{WorkflowRuntime, WorkflowService};
use signalboxd::{DaemonWorkflowPort, SessionTemplateConfiguration, WorkflowToolPolicy};
use sqlx::Row;

async fn workflow_fixture(
    policy: &str,
) -> Result<(ToolLoopFixture, WorkflowToolPolicy), Box<dyn Error>> {
    let models = approval_judge_model_configuration();
    let directory = tempdir()?;
    let template_path = directory.path().join("templates.toml");
    fs::write(
        &template_path,
        format!(
            r#"
version = 1
[[templates]]
name = "workflow-test"
version = 1
model = "{}"
system_prompt = "Run the requested build workflow."
dangerous_tool_auto_approval = false
{policy}
"#,
            Uuid::from_u128(FIXTURE_ID_SEED + 1)
        ),
    )?;
    let templates = SessionTemplateConfiguration::read(&template_path, || None, &models)?;
    let name = signalbox_domain::SessionTemplateName::try_new("workflow-test".into())?;
    let fixture = ToolLoopFixture::with_template(
        DangerousToolAutoApproval::Disabled,
        templates.resolve(&name),
    )
    .await?;
    let reload = ConfigurationReload::new(
        fixture.pool.clone(),
        models,
        templates,
        "/tmp/unused-workflow-models.toml".into(),
        "/tmp/unused-workflow-templates.toml".into(),
        None,
    )
    .map_err(|error| format!("reload: {error:?}"))?;
    let policy = WorkflowToolPolicy::new(fixture.pool.clone(), reload);
    Ok((fixture, policy))
}

async fn register_build(
    service: &WorkflowService,
) -> Result<ProgramRegistrationId, Box<dyn Error>> {
    let id = ProgramRegistrationId::from_uuid(Uuid::now_v7());
    service
        .register_javascript(
            id,
            ProgramRegistrationRequest {
                name: "build".into(),
                revision: "1".into(),
                source: b"export default input => input;".to_vec(),
                artifact: "export default input => input;".into(),
                grants: ProgramGrants::new([]),
            },
        )
        .await?;
    Ok(id)
}

async fn retained_request(
    fixture: &ToolLoopFixture,
    id: ToolRequestId,
) -> Result<ToolRequest, Box<dyn Error>> {
    let row = sqlx::query("SELECT producing_model_call_id, request_ordinal::bigint AS ordinal, tool_name, arguments_text FROM tool_request WHERE request_id = $1")
        .bind(id.into_uuid()).fetch_one(&fixture.pool).await?;
    Ok(ToolRequestReconstitutionInput::new(
        id,
        fixture.session,
        fixture.turn,
        ModelCallId::from_uuid(row.try_get("producing_model_call_id")?),
        ToolRequestOrdinal::from_u32(u32::try_from(row.try_get::<i64, _>("ordinal")?)?),
        ToolName::try_new(row.try_get::<String, _>("tool_name")?).expect("retained tool name"),
        NormalizedToolArguments::try_from_provider_text(
            row.try_get::<String, _>("arguments_text")?,
        )
        .expect("retained arguments"),
    )
    .into_request())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn workflow_start_uses_the_judge_and_replays_the_same_run_identity()
-> Result<(), Box<dyn Error>> {
    let (fixture, policy) =
        workflow_fixture("[templates.workflow_tools.start]\nnames = [\"build\"]").await?;
    let (service, _runner) = WorkflowRuntime::new(fixture.pool.clone())?;
    let registration = register_build(&service).await?;
    let mut port = DaemonWorkflowPort::new(policy.clone(), service, None);
    let arguments = r#"{"name":"build","revision":"1","input":[0,255,10]}"#;
    let (execution, runtime, judge) = fixture.execution_with_judge(
        [
            tool_use_script(&[("workflow_start", arguments)]),
            completion_script("started"),
        ],
        approval_judge_script("approve", "The build registration is granted."),
        signalbox_tools_workflows::catalog()?,
        WorkflowExecutor(port.clone()),
    );
    execution
        .with_workflow_tool_policy(policy)
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request_id = fixture.request_ids().await?[0];
    let result = continuation_result_json(&runtime)?;
    assert_eq!(
        result,
        serde_json::json!({"run_id":request_id.into_uuid().to_string(),"registration_id":registration.into_uuid().to_string()})
    );
    assert_eq!(judge.received_operations().len(), 1);
    let request = retained_request(&fixture, request_id).await?;
    let retry = port
        .execute(
            &request,
            WorkflowRequest::Start(StartArguments {
                name: "build".into(),
                revision: "1".into(),
                input: vec![0, 255, 10],
            }),
        )
        .await?;
    assert_eq!(
        retry,
        ToolExecutorEvidence::CompletedText(result.to_string())
    );
    let input: Vec<u8> =
        sqlx::query_scalar("SELECT input FROM program_run_registration WHERE run_id = $1")
            .bind(request_id.into_uuid())
            .fetch_one(&fixture.pool)
            .await?;
    assert_eq!(input, [0, 255, 10]);
    let conflict = port
        .execute(
            &request,
            WorkflowRequest::Start(StartArguments {
                name: "build".into(),
                revision: "1".into(),
                input: vec![1],
            }),
        )
        .await?;
    assert!(
        matches!(conflict, ToolExecutorEvidence::KnownFailed { detail: Some(detail) } if detail.as_str().contains("conflicting_reuse"))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn workflow_start_outside_the_grant_is_retained_as_a_tool_failure()
-> Result<(), Box<dyn Error>> {
    let (fixture, policy) = workflow_fixture(
        "[templates.workflow_tools.start]\nnames = [\"build\"]\nposture = \"auto\"",
    )
    .await?;
    let (service, _runner) = WorkflowRuntime::new(fixture.pool.clone())?;
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[(
                "workflow_start",
                r#"{"name":"release","revision":"1","input":[]}"#,
            )]),
            completion_script("refusal observed"),
        ],
        signalbox_tools_workflows::catalog()?,
        WorkflowExecutor(DaemonWorkflowPort::new(policy.clone(), service, None)),
    );
    execution
        .with_workflow_tool_policy(policy)
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    assert_eq!(
        continuation_result_json(&runtime)?,
        serde_json::json!({"error":{"kind":"execution_failed","detail":"workflow_grant_denied"}})
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM program_run_registration")
        .fetch_one(&fixture.pool)
        .await?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn workflow_replay_copies_complete_input_and_list_marks_the_callers_run()
-> Result<(), Box<dyn Error>> {
    let (fixture, policy) = workflow_fixture("[templates.workflow_tools.replay]\nnames = [\"build\"]\nposture = \"auto\"\n[templates.workflow_tools.list]\nenabled = true\n[templates.workflow_tools.read]\nenabled = true").await?;
    let (service, _runner) = WorkflowRuntime::new(fixture.pool.clone())?;
    let registration = register_build(&service).await?;
    let original = ProgramRunId::from_uuid(Uuid::now_v7());
    // This exact input exceeds the tool-result bound as JSON decimal bytes.
    let input = vec![255; 1_048_576];
    service.start(original, registration, &input).await?;
    let replay_arguments =
        serde_json::json!({"run_id":original.into_uuid().to_string()}).to_string();
    let mut port = DaemonWorkflowPort::new(policy.clone(), service, None);
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[("workflow_replay", &replay_arguments)]),
            completion_script("replayed"),
        ],
        signalbox_tools_workflows::catalog()?,
        WorkflowExecutor(port.clone()),
    );
    execution
        .with_workflow_tool_policy(policy)
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let id = fixture.request_ids().await?[0];
    assert_eq!(
        continuation_result_json(&runtime)?["run_id"],
        id.into_uuid().to_string()
    );
    let replayed: Vec<u8> =
        sqlx::query_scalar("SELECT input FROM program_run_registration WHERE run_id = $1")
            .bind(id.into_uuid())
            .fetch_one(&fixture.pool)
            .await?;
    assert_eq!(replayed, input);
    let request = retained_request(&fixture, id).await?;
    let ToolExecutorEvidence::CompletedText(list) = port
        .execute(&request, WorkflowRequest::List { after: None })
        .await?
    else {
        panic!("list")
    };
    let list: serde_json::Value = serde_json::from_str(&list)?;
    assert_eq!(list["runs"].as_array().expect("runs").len(), 2);
    let own = list["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .find(|run| run["own_run"] == true)
        .expect("own run");
    assert_eq!(own["run_id"], id.into_uuid().to_string());
    assert_eq!(own["state"], "running");
    assert!(own["started_at"].is_string());
    let ToolExecutorEvidence::CompletedText(read) = port
        .execute(
            &request,
            WorkflowRequest::Read {
                run: original.into_uuid(),
            },
        )
        .await?
    else {
        panic!("read")
    };
    let read: serde_json::Value = serde_json::from_str(&read)?;
    assert_eq!(
        read["run"]["input_extent"],
        serde_json::json!({"kind":"truncated","total_bytes":1_048_576})
    );
    assert_eq!(read["journal_length"], "0");
    assert!(signalbox_domain::ToolResultText::try_new(read.to_string()).is_ok());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn workflow_stop_other_run_preserves_the_cancellation_receipt_on_retry()
-> Result<(), Box<dyn Error>> {
    let (fixture, policy) =
        workflow_fixture("[templates.workflow_tools.stop]\nenabled = true\nposture = \"auto\"")
            .await?;
    let (service, _runner) = WorkflowRuntime::new(fixture.pool.clone())?;
    let registration = register_build(&service).await?;
    let run = ProgramRunId::from_uuid(Uuid::now_v7());
    service.start(run, registration, &[]).await?;
    let mut port = DaemonWorkflowPort::new(policy.clone(), service, None);
    let args = serde_json::json!({"run_id":run.into_uuid().to_string()}).to_string();
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[("workflow_stop", &args)]),
            completion_script("stopped"),
        ],
        signalbox_tools_workflows::catalog()?,
        WorkflowExecutor(port.clone()),
    );
    execution
        .with_workflow_tool_policy(policy)
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let result = continuation_result_json(&runtime)?;
    assert_eq!(
        result["outcome"],
        serde_json::json!({"kind":"applied","terminal_state":"cancelled","result":null})
    );
    let request = retained_request(&fixture, fixture.request_ids().await?[0]).await?;
    assert_eq!(
        port.execute(
            &request,
            WorkflowRequest::Stop {
                run: run.into_uuid()
            }
        )
        .await?,
        ToolExecutorEvidence::CompletedText(result.to_string())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn workflow_register_retry_uses_retained_workspace_bytes() -> Result<(), Box<dyn Error>> {
    let (fixture, policy) = workflow_fixture(
        "[templates.workflow_tools.register]\nnames = [\"build\"]\nposture = \"auto\"",
    )
    .await?;
    let workspace = tempdir()?;
    let source = b"export default input => input;";
    fs::write(workspace.path().join("source.js"), source)?;
    fs::write(workspace.path().join("artifact.js"), source)?;
    let tools = commissioned_daemon_tools(
        &fixture.pool,
        UnusedCodeHostTransport,
        UnusedGitHubTransport,
        workspace.path(),
    )?;
    let roots = tools.workspace_instruction_root_resolver();
    let (service, _runner) = WorkflowRuntime::new(fixture.pool.clone())?;
    let mut port = DaemonWorkflowPort::new(policy.clone(), service, roots);
    let (catalog, executor) = tools.into_parts();
    let catalog = catalog.with_compiled_catalog(signalbox_tools_workflows::catalog()?)?;
    let executor = executor.with_workflows(port.clone());
    let args = r#"{"name":"build","revision":"1","source_path":"source.js","artifact_path":"artifact.js","grants":[]}"#;
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[("workflow_register", args)]),
            completion_script("registered"),
        ],
        catalog,
        executor,
    );
    execution
        .with_workflow_tool_policy(policy)
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let id = fixture.request_ids().await?[0];
    assert_eq!(
        continuation_result_json(&runtime)?,
        serde_json::json!({"registration_id":id.into_uuid().to_string()})
    );
    fs::write(workspace.path().join("source.js"), "changed source")?;
    fs::remove_file(workspace.path().join("artifact.js"))?;
    let request = retained_request(&fixture, id).await?;
    let retry = port
        .execute(
            &request,
            WorkflowRequest::Register(RegisterArguments {
                name: "build".into(),
                revision: "1".into(),
                source_path: "source.js".into(),
                artifact_path: "artifact.js".into(),
                grants: vec![],
            }),
        )
        .await?;
    assert_eq!(
        retry,
        ToolExecutorEvidence::CompletedText(
            serde_json::json!({"registration_id":id.into_uuid().to_string()}).to_string()
        )
    );
    let artifact: String =
        sqlx::query_scalar("SELECT artifact FROM program_registration WHERE registration_id = $1")
            .bind(id.into_uuid())
            .fetch_one(&fixture.pool)
            .await?;
    assert_eq!(artifact.as_bytes(), source);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn workflow_human_posture_parks_before_start_admission() -> Result<(), Box<dyn Error>> {
    let (fixture, policy) = workflow_fixture(
        "[templates.workflow_tools.start]\nnames = [\"build\"]\nposture = \"human\"",
    )
    .await?;
    let (service, _runner) = WorkflowRuntime::new(fixture.pool.clone())?;
    register_build(&service).await?;
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[(
                "workflow_start",
                r#"{"name":"build","revision":"1","input":[]}"#,
            )]),
            completion_script("started after approval"),
        ],
        signalbox_tools_workflows::catalog()?,
        WorkflowExecutor(DaemonWorkflowPort::new(policy.clone(), service, None)),
    );
    let execution = execution.with_workflow_tool_policy(policy);
    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request = fixture.request_ids().await?[0];
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM program_run_registration")
        .fetch_one(&fixture.pool)
        .await?;
    assert_eq!(count, 0);
    let posture: String =
        sqlx::query_scalar("SELECT approval_posture FROM tool_request WHERE request_id = $1")
            .bind(request.into_uuid())
            .fetch_one(&fixture.pool)
            .await?;
    assert_eq!(posture, "human");
    fixture
        .decide(request, ToolApprovalDecision::Approve)
        .await?;
    execution.resume_active(fixture.session).await?;
    assert_eq!(
        continuation_result_json(&runtime)?["run_id"],
        request.into_uuid().to_string()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn workflow_register_refuses_a_source_symlink_outside_the_workspace()
-> Result<(), Box<dyn Error>> {
    let (fixture, policy) = workflow_fixture(
        "[templates.workflow_tools.register]\nnames = [\"build\"]\nposture = \"auto\"",
    )
    .await?;
    let workspace = tempdir()?;
    let outside = tempdir()?;
    fs::write(
        outside.path().join("source.js"),
        "export default input => input;",
    )?;
    std::os::unix::fs::symlink(
        outside.path().join("source.js"),
        workspace.path().join("source.js"),
    )?;
    fs::write(
        workspace.path().join("artifact.js"),
        "export default input => input;",
    )?;
    let tools = commissioned_daemon_tools(
        &fixture.pool,
        UnusedCodeHostTransport,
        UnusedGitHubTransport,
        workspace.path(),
    )?;
    let roots = tools.workspace_instruction_root_resolver();
    let (service, _runner) = WorkflowRuntime::new(fixture.pool.clone())?;
    let (catalog, executor) = tools.into_parts();
    let catalog = catalog.with_compiled_catalog(signalbox_tools_workflows::catalog()?)?;
    let executor = executor.with_workflows(DaemonWorkflowPort::new(policy.clone(), service, roots));
    let args = r#"{"name":"build","revision":"1","source_path":"source.js","artifact_path":"artifact.js","grants":[]}"#;
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[("workflow_register", args)]),
            completion_script("refused"),
        ],
        catalog,
        executor,
    );
    execution
        .with_workflow_tool_policy(policy)
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    assert_eq!(
        continuation_result_json(&runtime)?,
        serde_json::json!({"error":{"kind":"execution_failed","detail":"workflow_workspace_source_unavailable"}})
    );
    let registrations: i64 = sqlx::query_scalar("SELECT count(*) FROM program_registration")
        .fetch_one(&fixture.pool)
        .await?;
    assert_eq!(registrations, 0);
    Ok(())
}
