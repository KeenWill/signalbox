//! Per-request proposal admission through the provider and tool loop.

use super::*;
use signalbox_application::{
    CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence, ToolDefinition,
    ToolExecutionInvocation, ToolExecutor, ToolExecutorEvidence, ToolInputSchema,
    ToolProposalLimits,
};
use signalbox_domain::{ToolEffectClass, ToolExecutionErrorDetail, ToolPermissionDefault};

#[derive(Debug)]
enum ExecutorCannotFail {}

impl std::fmt::Display for ExecutorCannotFail {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {}
    }
}
impl Error for ExecutorCannotFail {}
impl ClassifyOperatorFailure for ExecutorCannotFail {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match *self {}
    }
}

#[derive(Clone, Default)]
struct RecordingExecutor(Arc<Mutex<Vec<String>>>);

impl ToolExecutor for RecordingExecutor {
    type Error = ExecutorCannotFail;
    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        self.0
            .lock()
            .expect("executor recording lock")
            .push(invocation.request().name().as_str().to_owned());
        Ok(invocation.bind(ToolExecutorEvidence::CompletedText("ok".to_owned())))
    }
}

#[derive(sqlx::FromRow)]
struct StoredProposal {
    resolution_kind: Option<String>,
    inadmissible_reason: Option<String>,
    arguments_text: String,
}

struct ProposalOutcome {
    executed: Vec<String>,
    results: Vec<signalbox_model_runtime::ToolResultRecord>,
    requests: Vec<StoredProposal>,
    turn_disposition: String,
}

fn proposal(name: &str, arguments: String) -> AssistantPart {
    AssistantPart::ToolCall(signalbox_model_runtime::ToolCallProposal {
        id: signalbox_model_runtime::ToolCallId::new(Uuid::now_v7().to_string()),
        name: signalbox_model_runtime::ToolName::new(name),
        arguments_json: arguments,
    })
}

async fn run_proposals(
    content: Vec<AssistantPart>,
    limits: ToolProposalLimits,
) -> Result<ProposalOutcome, Box<dyn Error>> {
    let text = MODEL_CONFIGURATION.replace("adapter = \"anthropic\"", "adapter = \"openai\"");
    let runtime = RunningRuntime::start_with_model_configuration(&text).await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, turn_id) = submit_first_input(
        &mut connection,
        session_id,
        "Execute the admitted proposals and report their results.".to_owned(),
    )
    .await?;
    let session = SessionId::from_uuid(session_id.into_uuid());
    let configuration = support::parse_model_configuration(&text)?;
    let models = configuration.runtime_model_catalog();
    let ordinary = compaction::RecordingCountedScriptedModel::following(
        [
            Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
                exchange: ExchangeFacts::default(),
                message_id: None,
                reported_model: Some(ProviderReportedModel::new("fixture-model")),
                finish: CompletionFinish::ToolUse,
                content,
                usage: TokenUsage::unreported(),
            })),
            completed_script("fixture-model", "Task complete.", TokenUsage::unreported()),
        ],
        [100, 200],
    );
    let probe = ordinary.clone();
    let provider = RuntimeModelCallProvider::new(ordinary, models.clone(), None)
        .with_tool_proposal_limits(limits);
    let calls = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("proposal-fixture"),
    )
    .with_session_credentials(configuration.credential_family_catalog());
    let catalog = CompiledToolCatalog::try_new(["read_file", "write_file"].map(|name| {
        CompiledTool::new(
            ToolDefinition::new(
                ToolName::try_new(name.to_owned()).expect("fixture tool name"),
                "Records a fixture invocation.".to_owned(),
                ToolInputSchema::try_new(r#"{"type":"object"}"#.to_owned())
                    .expect("fixture schema"),
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            |_: &NormalizedToolArguments| -> Result<(), ToolExecutionErrorDetail> { Ok(()) },
        )
    }))
    .expect("distinct fixture tools");
    let executor = RecordingExecutor::default();
    let executions = executor.0.clone();
    let instructions =
        signalboxd::WorkspaceInstructionRuntime::new(runtime.pool.clone(), None, vec![]);
    let execution = signalboxd::WorkspaceInstructionPreparedExecution::new(
        PostgresProviderModelExecution::new(
            calls.clone(),
            InProcessAttemptDispatchGate::default(),
            provider.clone(),
            None,
        )
        .with_tool_loop(
            InProcessToolDispatchGate::default(),
            catalog.clone(),
            executor,
        ),
        instructions.clone(),
    );
    let mut pass = ContextGuardedTurnPass::new(
        StartEligibleTurnRepository::new(runtime.pool.clone()),
        calls,
        provider,
        catalog,
        models.clone(),
        configuration,
        Arc::new(RuntimeContextCompactionModel::new(
            ScriptedModel::following([]),
            models,
        )),
        HeapAllocatedExecution(execution),
    )
    .with_workspace_instructions(instructions);
    pass.run(session).await?;
    let operations = probe.prepared_operations();
    assert_eq!(
        operations.len(),
        2,
        "the model receives its per-request results"
    );
    let results = operations[1]
        .messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            MessagePart::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect();
    let requests = sqlx::query_as(
        "SELECT resolution_kind, inadmissible_reason, arguments_text FROM tool_request
          WHERE session_id = $1 AND turn_id = $2 ORDER BY request_ordinal",
    )
    .bind(session.into_uuid())
    .bind(turn_id.into_uuid())
    .fetch_all(&runtime.pool)
    .await?;
    let turn_disposition = sqlx::query_scalar(
        "SELECT terminal_disposition_kind FROM turn_lifecycle WHERE turn_id = $1",
    )
    .bind(turn_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    let executed = executions.lock().expect("executor recording lock").clone();
    drop(connection);
    runtime.stop().await?;
    Ok(ProposalOutcome {
        executed,
        results,
        requests,
        turn_disposition,
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn forty_proposals_execute_32_and_return_eight_cap_errors() -> Result<(), Box<dyn Error>> {
    let content = (0..32)
        .map(|_| proposal("read_file", "{}".to_owned()))
        .chain((0..8).map(|_| proposal("write_file", "{}".to_owned())))
        .collect();
    let outcome = run_proposals(
        content,
        ToolProposalLimits {
            max_requests: Some(32),
            max_argument_bytes: Some(1_048_576),
        },
    )
    .await?;
    assert_eq!(outcome.executed, vec!["read_file"; 32]);
    assert_eq!(outcome.results.len(), 40);
    assert_eq!(
        outcome
            .results
            .iter()
            .filter(|result| result.is_error)
            .count(),
        8
    );
    assert!(
        outcome.requests[..32]
            .iter()
            .all(|request| request.inadmissible_reason.is_none())
    );
    assert!(
        outcome.requests[32..].iter().all(
            |request| request.inadmissible_reason.as_deref() == Some("proposal_limit_exceeded")
        )
    );
    for result in outcome.results.iter().filter(|result| result.is_error) {
        let error: serde_json::Value = serde_json::from_str(&result.content)?;
        assert_eq!(error["error"]["kind"], "execution_failed");
        assert!(result.content.contains("32"));
    }
    assert_eq!(outcome.turn_disposition, "completed");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn oversized_write_arguments_error_only_that_request() -> Result<(), Box<dyn Error>> {
    let arguments =
        serde_json::json!({"path": "output", "content": "x".repeat(2 * 1024 * 1024)}).to_string();
    let outcome = run_proposals(
        vec![
            proposal("write_file", arguments),
            proposal("read_file", "{}".to_owned()),
        ],
        ToolProposalLimits {
            max_requests: Some(32),
            max_argument_bytes: Some(1_048_576),
        },
    )
    .await?;
    assert_eq!(outcome.executed, ["read_file"]);
    assert_eq!(outcome.results.len(), 2);
    assert_eq!(
        outcome
            .results
            .iter()
            .filter(|result| result.is_error)
            .count(),
        1
    );
    let rejected = outcome
        .results
        .iter()
        .find(|result| result.is_error)
        .expect("one rejected write");
    let error: serde_json::Value = serde_json::from_str(&rejected.content)?;
    assert_eq!(error["error"]["kind"], "invalid_arguments");
    assert_eq!(
        outcome.requests[0].resolution_kind.as_deref(),
        Some("closed_inadmissible")
    );
    assert_eq!(
        outcome.requests[0].inadmissible_reason.as_deref(),
        Some("argument_bytes_exceeded")
    );
    let preview: serde_json::Value = serde_json::from_str(&outcome.requests[0].arguments_text)?;
    assert_eq!(preview["retained_bytes"], 1024);
    assert!(
        preview["dropped_bytes"]
            .as_u64()
            .expect("dropped byte count")
            > 1_048_576
    );
    assert_eq!(outcome.turn_disposition, "completed");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn unbounded_proposals_execute_every_request() -> Result<(), Box<dyn Error>> {
    let outcome = run_proposals(
        (0..40)
            .map(|_| proposal("read_file", "{}".to_owned()))
            .collect(),
        ToolProposalLimits {
            max_requests: None,
            max_argument_bytes: None,
        },
    )
    .await?;
    assert_eq!(outcome.executed, vec!["read_file"; 40]);
    assert_eq!(outcome.results.len(), 40);
    assert!(outcome.results.iter().all(|result| !result.is_error));
    assert_eq!(outcome.turn_disposition, "completed");
    Ok(())
}

// The composed execution future is large in an unoptimized integration binary.
#[derive(Clone)]
struct HeapAllocatedExecution<E>(E);

impl<E: signalboxd::ActivatedTurnExecution> signalboxd::ActivatedTurnExecution
    for HeapAllocatedExecution<E>
{
    type Error = E::Error;

    fn execute(
        &self,
        activated: Box<signalbox_domain::ActivatedTurn>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        Box::pin(self.0.execute(activated))
    }

    fn resume_active_with_observer(
        &self,
        session: SessionId,
        observe: Arc<dyn Fn(TurnId) + Send + Sync>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        Box::pin(self.0.resume_active_with_observer(session, observe))
    }
}
