#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

use std::{error::Error, process::Command, time::Duration};

use signalbox_application::{
    CreateSessionOutcome, CreateSessionRequest, CreateSessionService, InProcessAttemptDispatchGate,
    InProcessEligibilityWorkSource, SchedulerLoop, SchedulerLoopExit, StartEligibleTurnService,
    SubmitInputOutcome, SubmitInputRequest, SubmitInputService, UuidV7SessionIdGenerator,
    UuidV7StartEligibleTurnIdGenerator, UuidV7SubmitInputIdGenerator,
};
use signalbox_domain::{
    DeliveryRequest, DirectModelSelection, DurableCommandId, ModelSelectionOverride,
    ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition, PerInputConfigurationChoices,
    ProviderModelIdentity, ResolvedProviderTarget, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionId, SubmitInputAppliedResult, SubmitInputResult,
    TurnId, UserContent,
};
use signalbox_hubd::{ActivatedTurnPass, FatalExecutionSupervisor, PostgresProviderModelExecution};
use signalbox_model_provider_runtime::{
    RuntimeModelCallProvider, RuntimeModelCatalog, RuntimeModelDefinition,
};
use signalbox_model_runtime::{
    AssistantPart, CompletionEvidence, CompletionFinish, CredentialReference, ExchangeFacts,
    ProviderReportedModel, Script, ScriptedModel, TerminalEvidence, TokenUsage,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository, local_test_connection_options, migrate,
    model_execution::PostgresModelCallRepository, scheduler::PostgresEligibilitySweep,
    start_eligible_turn::StartEligibleTurnRepository, submit_input::SubmitInputRepository,
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use tokio::time::timeout;

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_hubd_e2e";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool, String), Box<dyn Error>> {
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
    Ok((container, pool, database_url))
}

async fn wait_for_terminal(pool: &PgPool, session: SessionId, turn: TurnId) {
    loop {
        let terminal: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1
                  FROM turn_lifecycle
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND state_kind = 'terminal'
            )",
        )
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .fetch_one(pool)
        .await
        .unwrap_or(false);
        if terminal {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// S01 / S02 / INV-014 / INV-015: the complete offline
/// chain creates a session, submits input, lets the scheduler activate it,
/// invokes the application provider port, and atomically persists the exact
/// selection, resolved target, consumed frontier, Prepared-to-InFlight
/// checkpoint sequence, assistant reply, and terminal lifecycle facts.
/// INV-026: the bridge receives a one-action runtime script, so any repeated
/// physical interaction exhausts the script and fails the test.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_s02_inv014_inv015_runtime_bridge_persists_scripted_assistant_reply()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x2001));
    let mut create = CreateSessionService::new(
        UuidV7SessionIdGenerator,
        CreateSessionRepository::new(pool.clone()),
    );
    let CreateSessionOutcome::Applied(created) = create
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x2002)),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
        )?)
        .await?
    else {
        panic!("the unique fixture command must create its session")
    };
    let session = created.session();

    let sweep = PostgresEligibilitySweep::new(pool.clone());
    let (nudge, work_source) = InProcessEligibilityWorkSource::new(sweep);
    let mut submit = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        SubmitInputRepository::new(pool.clone()),
        nudge,
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(origin),
    )) = submit
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x2003)),
            session,
            UserContent::try_text(String::from("offline user request"))
                .expect("fixture user content is admitted"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )?)
        .await?
    else {
        panic!("the unique fixture input must create queued origin work")
    };
    let turn = origin.turn();

    let provider_identity = ProviderModelIdentity::from_uuid(Uuid::from_u128(0x2004));
    let target = ResolvedProviderTarget::naming(provider_identity);
    let targets =
        ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(selection, target)])
            .expect("one fixture target definition is unique");
    let runtime_models =
        RuntimeModelCatalog::try_from_definitions([RuntimeModelDefinition::try_new(
            target,
            String::from("scripted-exact"),
            64,
        )
        .expect("fixture runtime definition is valid")])
        .expect("one fixture runtime target is unique");
    let runtime = ScriptedModel::single(Script::delivering(TerminalEvidence::Completed(
        CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("scripted-exact")),
            finish: CompletionFinish::EndTurn,
            content: vec![AssistantPart::Text(String::from("offline assistant reply"))],
            usage: TokenUsage::unreported(),
        },
    )));
    let provider = RuntimeModelCallProvider::new(
        runtime,
        runtime_models,
        CredentialReference::new("scripted-test"),
    );
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(PostgresProviderModelExecution::new(
            PostgresModelCallRepository::new(pool.clone(), targets),
            InProcessAttemptDispatchGate::default(),
            provider,
        ));
    let pass = ActivatedTurnPass::new(
        StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(pool.clone()),
        ),
        execution,
    );
    let mut scheduler = SchedulerLoop::new(work_source, pass);
    let observation_pool = pool.clone();
    let fatal_shutdown = fatal_execution.clone();
    let shutdown = async move {
        tokio::select! {
            () = wait_for_terminal(&observation_pool, session, turn) => {}
            () = fatal_shutdown.wait() => {}
        }
    };
    assert_eq!(
        timeout(Duration::from_secs(10), scheduler.run_until(shutdown)).await?,
        SchedulerLoopExit::Shutdown
    );
    assert!(
        !fatal_execution.is_triggered(),
        "post-activation execution failure must stop this isolated scheduler"
    );

    let transcript = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT entry.payload_kind,
                accepted.content_text,
                entry.assistant_text_value
           FROM turn_lifecycle AS lifecycle
           JOIN context_frontier_member AS member
             ON member.owning_session_id = lifecycle.session_id
            AND member.context_frontier_id = lifecycle.terminal_frontier_id
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
           LEFT JOIN accepted_input AS accepted
             ON accepted.session_id = entry.source_session_id
            AND accepted.accepted_input_id = entry.origin_accepted_input_id
          WHERE lifecycle.session_id = $1
            AND lifecycle.turn_id = $2
          ORDER BY member.member_position",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        transcript,
        vec![
            (
                String::from("origin_accepted_input"),
                Some(String::from("offline user request")),
                None,
            ),
            (
                String::from("assistant_text"),
                None,
                Some(String::from("offline assistant reply")),
            ),
            (String::from("turn_completed"), None, None),
        ]
    );

    let terminal_shape: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM turn_lifecycle
              WHERE session_id = $1
                AND turn_id = $2
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed'),
            (SELECT count(*) FROM turn_attempt
              WHERE session_id = $1
                AND turn_id = $2
                AND state_kind = 'ended'
                AND end_disposition = 'turn_completed'),
            (SELECT count(*) FROM model_call
              WHERE session_id = $1
                AND turn_id = $2
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed')",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(terminal_shape, (1, 1, 1));

    let call_provenance: (Uuid, String, Option<Uuid>, Uuid, Uuid, Uuid) = sqlx::query_as(
        "SELECT call.model_call_id,
                call.selection_kind,
                call.direct_model_selection_id,
                call.resolved_provider_model_identity_id,
                call.context_frontier_id,
                turn.starting_frontier_id
           FROM model_call AS call
           JOIN turn_lifecycle AS turn
             ON turn.session_id = call.session_id
            AND turn.turn_id = call.turn_id
          WHERE call.session_id = $1
            AND call.turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(call_provenance.1, "direct");
    assert_eq!(call_provenance.2, Some(selection.into_uuid()));
    assert_eq!(call_provenance.3, provider_identity.into_uuid());
    assert_eq!(call_provenance.4, call_provenance.5);

    let transition_sequence = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT transition.call_state_kind,
                transition.terminal_disposition_kind
           FROM model_call_transition_outbox_event AS transition
          WHERE transition.session_id = $1
            AND transition.turn_id = $2
            AND transition.model_call_id = $3
          ORDER BY transition.event_sequence",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(call_provenance.0)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        transition_sequence,
        vec![
            (String::from("prepared"), None),
            (String::from("in_flight"), None),
            (String::from("terminal"), Some(String::from("completed"))),
        ]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The thin debug harness drives the same scheduler path and prints only the
/// terminal semantic transcript requested by its caller.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn debug_driver_prints_the_scripted_terminal_transcript() -> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let output = Command::new(env!("CARGO_BIN_EXE_signalbox-debug"))
        .env("SIGNALBOX_DEBUG_DATABASE_URL", database_url)
        .args(["driver user request", "driver assistant reply"])
        .output()?;
    assert!(
        output.status.success(),
        "debug driver must exit successfully"
    );
    assert_eq!(
        String::from_utf8(output.stdout)?,
        "user: \"driver user request\"\nassistant: \"driver assistant reply\"\nevent: turn_completed\n"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Invalid scripted output is rejected before the debug harness writes any
/// session or queued work to its database.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn debug_driver_rejects_invalid_reply_before_durable_writes() -> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let output = Command::new(env!("CARGO_BIN_EXE_signalbox-debug"))
        .env("SIGNALBOX_DEBUG_DATABASE_URL", database_url)
        .args(["valid user input", ""])
        .output()?;

    assert!(!output.status.success());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM session")
            .fetch_one(&pool)
            .await?,
        0
    );

    pool.close().await;
    drop(container);
    Ok(())
}
