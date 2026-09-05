#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

mod support;

use std::error::Error;

use support::{blocked_backends_reached, record_empty_instruction_manifest};

use expect_test::expect;
use signalbox_expect_table::table;

use signalbox_application::{
    AuthorizeModelCallOutcome, AuthorizeModelCallTransaction,
    CommitModelCallObservationTransaction, ModelCallCredentialReference,
    ModelCallTerminalIdentityCandidates, StartEligibleTurnOutcome, StartupScanIdGenerator,
    StartupScanSessionOutcome,
};
use signalbox_domain::{
    AcceptedInputId, AcceptedInputTurnActivationIdentities, AcceptedInputTurnFailureIdentities,
    AssistantText, CancelledModelCallTurnIdentities, CommandPrincipal,
    CompletedModelCallIdentities, ContextCompactionId, ContextFrontierId, CreateSession,
    DeliveryRequest, DescendantTerminationScope, DirectModelSelection, DurableCommandId,
    FailedModelCallTurnIdentities, FinishCheckVerdict, FrozenAliasDefinition, Goal,
    GoalCommandRejection, GoalCommandResult, GoalEvent, GoalGuidance, GoalModelBlockedReasonKind,
    GoalModelProvenance, GoalNeed, GoalReport, GoalSchedulerProvenance, GoalState, GoalStatement,
    GoalUserAction, GoalUserCommand, GoalUserProvenance, LifecycleActor, ModelAlias, ModelCallId,
    ModelCallTerminalIdentities, ModelCallTerminalObservation, ModelSelectionOverride,
    ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition, ParentTerminationKind,
    PerInputConfigurationChoices, PreparedCreateSession, ProviderModelIdentity,
    ReplaceSessionDefaults, ResolvedProviderTarget, SemanticTranscriptEntryId,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionCreationCause,
    SessionCreationProvenance, SessionId, SessionInputPosition, SessionLifecycleApplication,
    SessionLifecycleCommand, SessionLifecycleCommandResult, SessionLifecycleOperation,
    SessionLifecycleState, SessionTerminalOutcome, StopStickiness, SubmitInput,
    SubmitInputAppliedResult, SubmitInputResult, ToolRequestId, TranscriptAncestry, TurnAttemptId,
    TurnId, TurnModelSettingsResolved, TurnTerminalCause, UserContent,
};
use signalbox_persistence::{
    SessionCredentialPin, SessionModelCredential,
    context_compaction::{
        ContextCompactionRepository, PrepareContextCompactionOutcome,
        PrepareContextCompactionRequest,
    },
    create_session::CreateSessionRepository,
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels,
    goal::{
        GoalCommandHandlingOutcome, GoalExecutionFailureRecoveryCause, GoalRepository,
        GoalRepositoryError, GoalTransitionOutcome,
    },
    goal_turn::{GoalTurnCandidates, GoalTurnContinuationOutcome},
    local_test_connection_options, migrate,
    model_execution::{PostgresModelCallRepository, PrepareInitialModelCallOutcome},
    outbox::{
        DispatchedDelegationOutcome, DispatchedDelegationProvenance, DispatchedDelegationReason,
        DispatchedOutboxEvent, DispatchedOutboxEventKind, DispatchedTurnTerminalDisposition,
        OutboxDeliveryDecision, OutboxDispatchOutcome, OutboxDispatcher,
    },
    process_read::{ProcessReadRepository, ProcessTurnState},
    replace_session_defaults::{
        ReplaceSessionDefaultsHandlingOutcome, ReplaceSessionDefaultsRepository,
    },
    scheduler::PostgresEligibilitySweep,
    session_lifecycle::SessionLifecycleRepository,
    session_lifecycle_command::{
        SessionLifecycleCommandHandlingOutcome, SessionLifecycleCommandRepository,
    },
    start_eligible_turn::{CommitCompactionFailurePreviewOutcome, StartEligibleTurnRepository},
    startup::PostgresStartupScanRepository,
    submit_input::SubmitInputRepository,
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_goal";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const SESSION: u128 = 0x701;
const CREATE_COMMAND: u128 = 0x801;
const ATTACH_COMMAND: u128 = 0x901;
const SUPERSEDE_COMMAND: u128 = 0x902;
const STOP_COMMAND: u128 = 0x903;
const REATTACH_COMMAND: u128 = 0x904;
const STEER_COMMAND: u128 = 0x905;
const RESUME_COMMAND: u128 = 0x906;

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
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

fn session(value: u128) -> SessionId {
    SessionId::from_uuid(Uuid::from_u128(value))
}

fn command(value: u128) -> DurableCommandId {
    DurableCommandId::from_uuid(Uuid::from_u128(value))
}

fn tool_request(value: u128) -> ToolRequestId {
    ToolRequestId::from_uuid(Uuid::from_u128(value))
}

/// Delivers every committed event in order.
async fn drain_dispatched(pool: &PgPool) -> Result<Vec<DispatchedOutboxEvent>, Box<dyn Error>> {
    let dispatcher = OutboxDispatcher::new(pool.clone());
    let mut dispatched = Vec::new();
    loop {
        let mut offered = None;
        let outcome = dispatcher
            .dispatch_next(|event| {
                offered = Some(event.clone());
                OutboxDeliveryDecision::Delivered
            })
            .await?;
        match (outcome, offered) {
            (OutboxDispatchOutcome::Delivered { .. }, Some(event)) => dispatched.push(event),
            (OutboxDispatchOutcome::Idle, None) => return Ok(dispatched),
            (outcome, _) => return Err(format!("unexpected dispatch outcome {outcome:?}").into()),
        }
    }
}

/// The stored kind of each delivered event, in delivery order.
fn dispatched_kind_names(dispatched: &[DispatchedOutboxEvent]) -> Vec<&'static str> {
    dispatched
        .iter()
        .map(|event| match event.kind() {
            DispatchedOutboxEventKind::SessionCreated(_) => "session_created",
            DispatchedOutboxEventKind::SessionStateChanged(_) => "session_state_changed",
            DispatchedOutboxEventKind::SessionTerminal(_) => "session_terminal",
            DispatchedOutboxEventKind::TurnTerminal { .. } => "turn_terminal",
            DispatchedOutboxEventKind::GoalChanged(_) => "goal_changed",
            DispatchedOutboxEventKind::CommandSettled { .. } => "command_settled",
            DispatchedOutboxEventKind::InjectionSettled { .. } => "injection_settled",
            DispatchedOutboxEventKind::SessionOwnershipChanged(_) => "session_ownership_changed",
            DispatchedOutboxEventKind::SessionModelSettingsChanged(_) => {
                "session_model_settings_changed"
            }
            DispatchedOutboxEventKind::TurnModelSettingsResolved(_) => {
                "turn_model_settings_resolved"
            }
            DispatchedOutboxEventKind::InputAccepted { .. } => "input_accepted",
            DispatchedOutboxEventKind::TurnActivated { .. } => "turn_activated",
            DispatchedOutboxEventKind::ModelCallTransition { .. } => "model_call_transition",
            DispatchedOutboxEventKind::ToolBatchTransition { .. } => "tool_batch_transition",
            DispatchedOutboxEventKind::ToolApprovalDecided { .. } => "tool_approval_decided",
            DispatchedOutboxEventKind::ContextCompacted { .. } => "context_compacted",
            DispatchedOutboxEventKind::RunnerStateTransition { .. } => "runner_state_transition",
            DispatchedOutboxEventKind::DelegationUpdate(_) => "delegation_update",
            DispatchedOutboxEventKind::DelegationWake(_) => "delegation_wake",
        })
        .collect()
}

/// Where `turn`'s acceptance event sits in the delivered order.
fn acceptance_position(dispatched: &[DispatchedOutboxEvent], turn: TurnId) -> Option<usize> {
    dispatched.iter().position(|event| {
        matches!(
            event.kind(),
            DispatchedOutboxEventKind::InputAccepted { turn: accepted, .. } if *accepted == turn
        )
    })
}

#[track_caller]
fn turn_model_settings_event(event: &DispatchedOutboxEvent) -> &TurnModelSettingsResolved {
    match event.kind() {
        DispatchedOutboxEventKind::TurnModelSettingsResolved(settings) => settings,
        _ => panic!("the event has a turn-settings payload"),
    }
}

fn credential_pin() -> SessionCredentialPin {
    SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        "test-model-family",
        "test-model-primary",
    )])
    .expect("test credential pin is valid")
}

fn creation() -> PreparedCreateSession {
    creation_with_model(ModelSelectionRequest::Direct(
        DirectModelSelection::from_uuid(Uuid::from_u128(0xa01)),
    ))
}

fn creation_with_model(model: ModelSelectionRequest) -> PreparedCreateSession {
    CreateSession::new(
        command(CREATE_COMMAND),
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        SessionConfigurationDefaults::new(model),
    )
    .prepare(session(SESSION))
    .expect("user-initiated creation without ancestry is preparable")
}

fn creation_fixture(command_id: u128, session_id: u128, selection: u128) -> PreparedCreateSession {
    CreateSession::new(
        command(command_id),
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(Uuid::from_u128(selection)),
        )),
    )
    .prepare(session(session_id))
    .expect("user-initiated creation without ancestry is preparable")
}

struct DelegationFixture {
    spawning_request: u128,
    parent_session: u128,
    parent_turn: u128,
    child_session: u128,
    child_turn: u128,
    task_entry: u128,
    selection: u128,
    policy_kind: &'static str,
    on_parent_stopped: Option<&'static str>,
    on_parent_cancelled: Option<&'static str>,
}

async fn insert_queued_delegation_fixture(
    pool: &PgPool,
    fixture: DelegationFixture,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_initial_task DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event DISABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             provenance_kind, provenance_session_id, provenance_turn_id,
             provenance_tool_request_id)
         VALUES ($1, 1, 'spawned', 'tool_request', $2, $3, $1)",
    )
    .bind(Uuid::from_u128(fixture.spawning_request))
    .bind(Uuid::from_u128(fixture.parent_session))
    .bind(Uuid::from_u128(fixture.parent_turn))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation
            (spawning_tool_request_id, parent_session_id, parent_turn_id,
             child_session_id, policy_kind,
             on_parent_stopped, on_parent_cancelled)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(Uuid::from_u128(fixture.spawning_request))
    .bind(Uuid::from_u128(fixture.parent_session))
    .bind(Uuid::from_u128(fixture.parent_turn))
    .bind(Uuid::from_u128(fixture.child_session))
    .bind(fixture.policy_kind)
    .bind(fixture.on_parent_stopped)
    .bind(fixture.on_parent_cancelled)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_kind, origin_accepted_input_id,
             acceptance_position, state_kind)
         VALUES ($1, $2, 'delegation', NULL, 1, 'queued')",
    )
    .bind(Uuid::from_u128(fixture.child_turn))
    .bind(Uuid::from_u128(fixture.child_session))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_initial_task
            (spawning_tool_request_id, child_session_id, turn_id,
             semantic_entry_id, admission_position, defaults_version,
             requested_model_kind, requested_direct_model_selection_id,
             frozen_model_kind, frozen_direct_model_selection_id, task_content)
         VALUES ($1, $2, $3, $4, 1, 1, 'direct', $5, 'direct', $5, $6)",
    )
    .bind(Uuid::from_u128(fixture.spawning_request))
    .bind(Uuid::from_u128(fixture.child_session))
    .bind(Uuid::from_u128(fixture.child_turn))
    .bind(Uuid::from_u128(fixture.task_entry))
    .bind(Uuid::from_u128(fixture.selection))
    .bind("queued cascade fixture task")
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             delegated_task_spawning_tool_request_id)
         VALUES ($1, $2, 'delegated_task', $3)",
    )
    .bind(Uuid::from_u128(fixture.child_session))
    .bind(Uuid::from_u128(fixture.task_entry))
    .bind(Uuid::from_u128(fixture.spawning_request))
    .execute(&mut *transaction)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_initial_task ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event ENABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

fn statement(value: &str) -> GoalStatement {
    GoalStatement::try_new(value.to_owned()).expect("fixture goal statement is admitted")
}

fn turn_candidates(value: u128) -> GoalTurnCandidates {
    GoalTurnCandidates::new(
        AcceptedInputId::from_uuid(Uuid::from_u128(value)),
        TurnId::from_uuid(Uuid::from_u128(value + 0x100)),
    )
}

fn latest_event(goal: &Goal) -> signalbox_domain::GoalEvent {
    goal.events()
        .last()
        .cloned()
        .expect("fixture goal has a latest event")
}

#[track_caller]
fn activated_turn(outcome: StartEligibleTurnOutcome) -> TurnId {
    match outcome {
        StartEligibleTurnOutcome::Activated(activated) => activated.turn(),
        StartEligibleTurnOutcome::NoEligibleTurn => {
            panic!("fixture goal turn must be eligible for activation")
        }
    }
}

fn activation_identities(value: u128) -> AcceptedInputTurnActivationIdentities {
    AcceptedInputTurnActivationIdentities::new(
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value)),
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value + 1)),
        ContextFrontierId::from_uuid(Uuid::from_u128(value + 2)),
        TurnAttemptId::from_uuid(Uuid::from_u128(value + 3)),
    )
}

async fn activate_goal_turn(pool: &PgPool, value: u128) -> Result<TurnId, Box<dyn Error>> {
    let outcome = StartEligibleTurnRepository::new(pool.clone())
        .handle(session(SESSION), activation_identities(value))
        .await?;
    Ok(activated_turn(outcome))
}

struct FixedStartupIds {
    failure_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
}

impl StartupScanIdGenerator for FixedStartupIds {
    fn next_failure_entry_id(&mut self) -> SemanticTranscriptEntryId {
        self.failure_entry
    }

    fn next_terminal_frontier_id(&mut self) -> ContextFrontierId {
        self.terminal_frontier
    }

    fn next_reclassified_turn_id(&mut self, _accepted_input: AcceptedInputId) -> TurnId {
        panic!("goal failure fixture has no pending steering")
    }
}

async fn terminalize_goal_turn_as_failed(pool: &PgPool, value: u128) -> Result<(), Box<dyn Error>> {
    let mut ids = FixedStartupIds {
        failure_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value)),
        terminal_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(value + 1)),
    };
    let outcome = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            session(SESSION),
            AcceptedInputTurnFailureIdentities::new(ids.failure_entry, ids.terminal_frontier),
            &mut ids,
        )
        .await?;
    let StartupScanSessionOutcome::Recovered(_) = outcome else {
        panic!("prepared active goal turn must recover as failed");
    };
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn call_free_failure_recovery_cause_round_trips_as_a_closed_type()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let attached_turn = turn_candidates(0xb5f);
    GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                command(ATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement("finish the commissioned task")),
            ),
            Some(attached_turn),
            |_| None,
        )
        .await?;
    let activation = StartEligibleTurnRepository::new(pool.clone());
    let preview = activation
        .preview(session(SESSION), activation_identities(0xd5f))
        .await?
        .expect("the queued goal turn has an activation preview");
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        DirectModelSelection::from_uuid(Uuid::from_u128(0xa01)),
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(0xa02))),
    )])
    .expect("one fixture target forms a catalog");
    let model_calls = PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        ModelCallCredentialReference::new("compaction-failure-test-provider"),
    );
    let expected = GoalExecutionFailureRecoveryCause::ContextCompactionInputDoesNotFit;
    let closure = activation
        .commit_compaction_failure_preview(
            preview,
            &model_calls,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xe5f)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xe60)),
            ),
            TurnTerminalCause::ContextCompactionWall,
            Some(expected),
        )
        .await?;
    assert_eq!(
        closure,
        CommitCompactionFailurePreviewOutcome::Failed(attached_turn.turn())
    );

    let actual = GoalRepository::new(pool.clone())
        .execution_failure_recovery_cause(session(SESSION), attached_turn.turn())
        .await?;

    assert_eq!(actual, Some(expected));
    pool.close().await;
    drop(container);
    Ok(())
}

/// a fresh durable sweep rediscovers a pursuing goal whose
/// current turn terminalized before its scheduler disposition could commit,
/// and the goal-owned origin records its frozen model settings.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_goal_disposition_survives_scheduler_restart() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let attached_turn = turn_candidates(0xb61);
    GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                command(ATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement("finish the commissioned task")),
            ),
            Some(attached_turn),
            |_| None,
        )
        .await?;
    let settings_evidence: (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_model_settings_resolved
              WHERE accepted_input_id = $1 AND turn_id = $2),
            (SELECT count(*)
               FROM turn_model_settings_resolved_outbox_event
              WHERE accepted_input_id = $1 AND session_id = $3)",
    )
    .bind(attached_turn.accepted_input().into_uuid())
    .bind(attached_turn.turn().into_uuid())
    .bind(session(SESSION).into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(settings_evidence, (1, 1));
    assert_eq!(
        activate_goal_turn(&pool, 0xd61).await?,
        attached_turn.turn()
    );
    terminalize_goal_turn_as_failed(&pool, 0xe61).await?;

    let (sessions, _dispatch_starts, continuation) = PostgresEligibilitySweep::new(pool.clone())
        .find_sessions()
        .await?
        .into_parts();

    assert_eq!(sessions, vec![session(SESSION)]);
    assert!(!continuation);

    pool.close().await;
    drop(container);
    Ok(())
}

async fn insert_goal_tool_request(
    pool: &PgPool,
    turn: TurnId,
    request: ToolRequestId,
    tool_name: &str,
    arguments: &str,
    declaration_text: &str,
) -> Result<(), Box<dyn Error>> {
    sqlx::query("ALTER TABLE tool_request DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO tool_request
            (request_id, session_id, turn_id, producing_model_call_id,
             request_ordinal, tool_name, arguments_kind, arguments_text)
         VALUES ($1, $2, $3, $4, 0, $5, 'json', $6)",
    )
    .bind(request.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind(turn.into_uuid())
    .bind(Uuid::from_u128(request.into_uuid().as_u128() + 0x1000))
    .bind(tool_name)
    .bind(arguments)
    .execute(pool)
    .await?;
    let producing_call = Uuid::from_u128(request.into_uuid().as_u128() + 0x1000);
    sqlx::query("ALTER TABLE semantic_transcript_entry DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             assistant_text_value, producing_model_call_id,
             assistant_response_part_ordinal, assistant_tool_request_id,
             assistant_response_text_start_bytes)
         VALUES ($1, $2, 'assistant_text', $4, $3, 0, NULL, 0),
                ($1, $5, 'assistant_tool_use', NULL, $3, 1, $6, NULL)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(Uuid::from_u128(request.into_uuid().as_u128() + 0x2000))
    .bind(producing_call)
    .bind(declaration_text)
    .bind(Uuid::from_u128(request.into_uuid().as_u128() + 0x3000))
    .bind(request.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE semantic_transcript_entry ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;

    sqlx::query("ALTER TABLE tool_request ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

async fn insert_following_tool_request(
    pool: &PgPool,
    turn: TurnId,
    declaration_request: ToolRequestId,
    following_request: ToolRequestId,
) -> Result<(), Box<dyn Error>> {
    let producing_call = Uuid::from_u128(declaration_request.into_uuid().as_u128() + 0x1000);
    sqlx::query("ALTER TABLE tool_request DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO tool_request
            (request_id, session_id, turn_id, producing_model_call_id,
             request_ordinal, tool_name, arguments_kind, arguments_text)
         VALUES ($1, $2, $3, $4, 1, 'inspect', 'json', '{}')",
    )
    .bind(following_request.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind(turn.into_uuid())
    .bind(producing_call)
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE semantic_transcript_entry DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             assistant_text_value, producing_model_call_id,
             assistant_response_part_ordinal, assistant_tool_request_id,
             assistant_response_text_start_bytes)
         VALUES ($1, $2, 'assistant_tool_use', NULL, $3, 2, $4, NULL)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(Uuid::from_u128(
        following_request.into_uuid().as_u128() + 0x4000,
    ))
    .bind(producing_call)
    .bind(following_request.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE semantic_transcript_entry ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE tool_request ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

async fn set_goal_turn_acceptance_position(
    pool: &PgPool,
    turn: TurnId,
    position: SessionInputPosition,
) -> Result<(), Box<dyn Error>> {
    sqlx::query("ALTER TABLE accepted_input DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE queued_input_origin DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE input_accepted_outbox_event DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE accepted_input
            SET acceptance_position = $3::numeric
          WHERE session_id = $1 AND origin_turn_id = $2",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(turn.into_uuid())
    .bind(position.as_u64().to_string())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE queued_input_origin
            SET acceptance_position = $3::numeric
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(turn.into_uuid())
    .bind(position.as_u64().to_string())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET acceptance_position = $3::numeric
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(turn.into_uuid())
    .bind(position.as_u64().to_string())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE input_accepted_outbox_event
            SET acceptance_position = $3::numeric
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(turn.into_uuid())
    .bind(position.as_u64().to_string())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    sqlx::query("ALTER TABLE accepted_input ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE queued_input_origin ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE input_accepted_outbox_event ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

#[track_caller]
fn assert_database_constraint(error: sqlx::Error, constraint: &str) {
    let database = error
        .as_database_error()
        .expect("deferred goal correlation reports a database constraint");
    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(database.constraint(), Some(constraint));
}

#[track_caller]
fn assert_model_declaration_request_rejected(error: GoalRepositoryError) {
    let GoalRepositoryError::Database(error) = error else {
        panic!("declaration-request mismatch must be a database rejection");
    };
    let database = error
        .as_database_error()
        .expect("declaration-request mismatch reports a database constraint");
    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(
        database.constraint(),
        Some("goal_event_model_declaration_request")
    );
}

async fn mark_goal_turn_completed(pool: &PgPool, turn: TurnId) -> Result<(), Box<dyn Error>> {
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal', terminal_disposition_kind = 'completed',
                terminal_cause_kind = 'completed',
                terminal_frontier_id = $3, active_phase_kind = NULL,
                terminal_attempt_id = current_attempt_id, current_attempt_id = NULL,
                terminal_model_call_id = $4
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(turn.into_uuid())
    .bind(Uuid::from_u128(0xd75))
    .bind(Uuid::from_u128(0xe75))
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

async fn completed_goal_with_successor(
    pool: &PgPool,
    attached: GoalTurnCandidates,
    successor: GoalTurnCandidates,
) -> Result<(), Box<dyn Error>> {
    let repository = GoalRepository::new(pool.clone());
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(ATTACH_COMMAND),
                    session(SESSION),
                    GoalUserAction::Attach(statement("continue through a successor")),
                ),
                Some(attached),
                |_| None,
            )
            .await?,
    );
    assert_eq!(activate_goal_turn(pool, 0xd58).await?, attached.turn());
    mark_goal_turn_completed(pool, attached.turn()).await?;
    assert_eq!(
        repository
            .reconcile_current_after_execution(
                session(SESSION),
                successor,
                GoalNeed::try_new(String::from("repair execution"))
                    .expect("fixture need is admitted"),
                |_| None,
            )
            .await?,
        GoalTurnContinuationOutcome::Scheduled {
            turn: successor.turn()
        }
    );
    Ok(())
}

/// A release serialized after the current goal turn completes retires the
/// daemon's liveness obligation before it can queue another goal turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_released_session_does_not_continue_its_completed_goal_turn() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attached = turn_candidates(0xb58);
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0x958),
                    session(SESSION),
                    GoalUserAction::Attach(statement("release after this goal turn")),
                ),
                Some(attached),
                |_| None,
            )
            .await?,
    );
    assert_eq!(activate_goal_turn(&pool, 0xd59).await?, attached.turn());
    mark_goal_turn_completed(&pool, attached.turn()).await?;
    assert_eq!(
        SessionLifecycleCommandRepository::new(pool.clone())
            .handle(
                SessionLifecycleCommand::new(
                    command(0x959),
                    session(SESSION),
                    SessionLifecycleOperation::Release,
                ),
                CommandPrincipal::Operator,
            )
            .await?,
        SessionLifecycleCommandHandlingOutcome::Recorded(SessionLifecycleCommandResult::Applied(
            SessionLifecycleApplication::OwnershipChanged
        ))
    );

    assert_eq!(
        repository
            .reconcile_current_after_execution(
                session(SESSION),
                turn_candidates(0xb59),
                GoalNeed::try_new(String::from("repair execution"))
                    .expect("fixture need is admitted"),
                |_| None,
            )
            .await?,
        GoalTurnContinuationOutcome::NotPursuing
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM goal_turn WHERE session_id = $1")
            .bind(Uuid::from_u128(SESSION))
            .fetch_one(&pool)
            .await?,
        1
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A release serialized after goal reconciliation retires the successor that
/// was already queued, so unmonitored goal work cannot later activate.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn release_retires_an_already_queued_goal_successor() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let attached = turn_candidates(0xb5a);
    let successor = turn_candidates(0xb5b);
    completed_goal_with_successor(&pool, attached, successor).await?;
    let queued: String = sqlx::query_scalar(
        "SELECT state_kind FROM turn_lifecycle WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(successor.turn().into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(queued, "queued");

    assert_eq!(
        SessionLifecycleCommandRepository::new(pool.clone())
            .handle(
                SessionLifecycleCommand::new(
                    command(0x95a),
                    session(SESSION),
                    SessionLifecycleOperation::Release,
                ),
                CommandPrincipal::Operator,
            )
            .await?,
        SessionLifecycleCommandHandlingOutcome::Recorded(SessionLifecycleCommandResult::Applied(
            SessionLifecycleApplication::OwnershipChanged
        ))
    );

    let retired: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind, terminal_cause_kind
           FROM turn_lifecycle WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(successor.turn().into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        retired,
        (
            String::from("terminal"),
            Some(String::from("retired")),
            Some(String::from("goal_turn_ineligible")),
        )
    );
    let retired_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM turn_terminal_outbox_event
          WHERE session_id = $1 AND turn_id = $2 AND disposition_kind = 'retired'",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(successor.turn().into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(retired_events, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

async fn mark_completed_goal_turn_failed(
    pool: &PgPool,
    turn: TurnId,
) -> Result<(), Box<dyn Error>> {
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET terminal_disposition_kind = 'failed',
                terminal_cause_kind = 'model_call_failed'
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(turn.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

#[track_caller]
fn assert_applied_transition(outcome: GoalTransitionOutcome) {
    let GoalTransitionOutcome::Applied(_) = outcome else {
        panic!("fixture transition must apply");
    };
}

#[track_caller]
fn applied_transition_event(outcome: GoalTransitionOutcome) -> GoalEvent {
    match outcome {
        GoalTransitionOutcome::Applied(event) => event,
        other => panic!("fixture transition must apply, got {other:?}"),
    }
}

#[track_caller]
fn assert_applied_command(outcome: GoalCommandHandlingOutcome) {
    let GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(_)) = outcome else {
        panic!("fixture command must apply");
    };
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn attaching_a_goal_dispatches_the_first_turn_under_the_command_issuer()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let attached = GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                command(ATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement("finish the commissioned task")),
            ),
            Some(turn_candidates(0xb68)),
            |_| None,
        )
        .await?;
    let lifecycle: (String, String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, actor_kind, actor_module
           FROM session_lifecycle WHERE session_id = $1",
    )
    .bind(session(SESSION).into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_applied_command(attached);
    assert_eq!(
        lifecycle,
        (String::from("dispatched"), String::from("operator"), None)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn owned_pending_failure_selection_requires_the_exact_need_under_the_session_lock()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attached_turn = turn_candidates(0xb69);
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(ATTACH_COMMAND),
                    session(SESSION),
                    GoalUserAction::Attach(statement("finish the commissioned task")),
                ),
                Some(attached_turn),
                |_| None,
            )
            .await?,
    );
    assert_eq!(
        activate_goal_turn(&pool, 0xd69).await?,
        attached_turn.turn()
    );
    terminalize_goal_turn_as_failed(&pool, 0xe69).await?;
    let unmonitored_need = GoalNeed::try_new(String::from("await adoption to repair execution"))?;
    let operator_need = GoalNeed::try_new(String::from("operator must repair execution"))?;
    let blocked = applied_transition_event(
        repository
            .block_execution_failure(
                session(SESSION),
                unmonitored_need.clone(),
                GoalSchedulerProvenance::new(attached_turn.turn()),
            )
            .await?,
    );

    assert_eq!(
        repository
            .pending_owned_execution_failure_with_need(session(SESSION), &operator_need)
            .await?,
        None
    );
    assert_eq!(
        repository
            .pending_owned_execution_failure_with_need(session(SESSION), &unmonitored_need)
            .await?,
        Some(blocked.ordinal())
    );
    SessionLifecycleRepository::new(pool.clone())
        .release(session(SESSION), LifecycleActor::Operator)
        .await?;
    assert_eq!(
        repository
            .pending_owned_execution_failure_with_need(session(SESSION), &unmonitored_need)
            .await?,
        None
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// a goal-owned accepted input dispatches its frozen model
/// settings and activates without a synthetic user command, then remains a
/// canonical active origin for steer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s_goal_goal_owned_input_activates_without_a_user_command() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let candidates = turn_candidates(0xb01);
    let goal_content = String::from("finish the commissioned task");
    GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                command(ATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement(&goal_content)),
            ),
            Some(candidates),
            |_| None,
        )
        .await?;

    let accepted_events: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM input_accepted_outbox_event
          WHERE session_id = $1
            AND accepted_input_id = $2
            AND turn_id = $3",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(candidates.accepted_input().into_uuid())
    .bind(candidates.turn().into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(accepted_events, 1);

    let dispatched = drain_dispatched(&pool).await?;
    assert_eq!(
        dispatched_kind_names(&dispatched),
        [
            "session_created",
            "goal_changed",
            "session_ownership_changed",
            "turn_model_settings_resolved",
            "input_accepted",
        ]
    );
    assert_eq!(dispatched[0].session(), Some(session(SESSION)));
    let settings = &dispatched[3];
    assert_eq!(settings.session(), Some(session(SESSION)));
    let settings = turn_model_settings_event(settings);
    assert_eq!(settings.accepted_input(), candidates.accepted_input());
    assert_eq!(settings.turn(), candidates.turn());
    let accepted = &dispatched[4];
    assert_eq!(accepted.session(), Some(session(SESSION)));
    assert_eq!(
        accepted.kind(),
        &DispatchedOutboxEventKind::InputAccepted {
            accepted_input: candidates.accepted_input(),
            turn: candidates.turn(),
            acceptance_position: SessionInputPosition::first(),
            content: UserContent::try_text(goal_content)
                .expect("the goal fixture content is admitted"),
        }
    );

    let activation = StartEligibleTurnRepository::new(pool.clone())
        .handle(
            session(SESSION),
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd01)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd02)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xd03)),
                TurnAttemptId::from_uuid(Uuid::from_u128(0xd04)),
            ),
        )
        .await?;

    assert_eq!(activated_turn(activation), candidates.turn());

    assert_eq!(
        GoalRepository::new(pool.clone())
            .reconcile_current_after_execution(
                session(SESSION),
                turn_candidates(0xc01),
                GoalNeed::try_new(String::from("repair execution"))
                    .expect("fixture need is admitted"),
                |_| None,
            )
            .await?,
        GoalTurnContinuationOutcome::NotTerminal
    );

    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            SubmitInput::new(
                command(STEER_COMMAND),
                session(SESSION),
                UserContent::try_text(String::from("keep the current scope; use this detail"))
                    .expect("fixture steering content is admitted"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: candidates.turn(),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xe01)),
            None,
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xf01)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xf02)),
            ),
            |_| panic!("steering cannot be reclassified while its source remains active"),
            |_| {
                panic!("steering cannot cancel a tool request without a terminal model observation")
            },
        )
        .await?;
    let pending_steering: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM accepted_input
          WHERE accepting_command_id = $1 AND disposition_kind = 'pending_steering'",
    )
    .bind(Uuid::from_u128(STEER_COMMAND))
    .fetch_one(&pool)
    .await?;
    assert_eq!(pending_steering, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// an expected-head resume applies to exactly the blocked event it
/// names, and an unmet expectation appends nothing and spends no identity.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s_goal_expected_resume_binds_to_one_blocked_event() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attached_turn = turn_candidates(0xb70);
    let attached = repository
        .handle_user_command(
            GoalUserCommand::new(
                command(ATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement("finish the commissioned task")),
            ),
            Some(attached_turn),
            |_| None,
        )
        .await?;
    let GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(attached)) = attached
    else {
        panic!("fixture attach must apply");
    };
    assert_eq!(
        activate_goal_turn(&pool, 0xd70).await?,
        attached_turn.turn()
    );
    terminalize_goal_turn_as_failed(&pool, 0xe70).await?;
    let scheduled_need = GoalNeed::try_new(String::from(
        "automatic resumption is scheduled; repair execution",
    ))
    .expect("fixture need is admitted");
    let blocked = repository
        .block_execution_failure(
            session(SESSION),
            scheduled_need.clone(),
            GoalSchedulerProvenance::new(attached_turn.turn()),
        )
        .await?;
    let GoalTransitionOutcome::Applied(blocked) = blocked else {
        panic!("fixture execution-failure block must apply");
    };
    let pending = repository
        .pending_execution_failures_with_need(&scheduled_need)
        .await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].session(), session(SESSION));
    assert_eq!(pending[0].blocked(), blocked.ordinal());

    // The attach event is no longer the lineage head, so a command expecting it
    // answers a state that has moved on.
    let stale = repository
        .handle_expected_user_command(
            GoalUserCommand::new(
                command(RESUME_COMMAND),
                session(SESSION),
                GoalUserAction::Resume(None),
            ),
            Some(turn_candidates(0xb71)),
            attached.ordinal(),
            |_| None,
        )
        .await?;
    assert_eq!(stale, GoalCommandHandlingOutcome::LineageMoved);
    let unspent: i64 =
        sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
            .bind(Uuid::from_u128(RESUME_COMMAND))
            .fetch_one(&pool)
            .await?;
    assert_eq!(unspent, 0);

    let resumed = repository
        .handle_expected_user_command(
            GoalUserCommand::new(
                command(RESUME_COMMAND),
                session(SESSION),
                GoalUserAction::Resume(None),
            ),
            Some(turn_candidates(0xb72)),
            blocked.ordinal(),
            |_| None,
        )
        .await?;
    assert_applied_command(resumed);
    assert!(
        repository
            .pending_execution_failures_with_need(&scheduled_need)
            .await?
            .is_empty()
    );
    // The automatic resume is daemon core's, and the transition it projects
    // says so: the envelope issuer classifies it, not the command's presence.
    let actor: (String, Option<String>) = sqlx::query_as(
        "SELECT actor_kind, actor_module FROM session_lifecycle WHERE session_id = $1",
    )
    .bind(session(SESSION).into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(actor, (String::from("core"), None));
    let spent: i64 =
        sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
            .bind(Uuid::from_u128(RESUME_COMMAND))
            .fetch_one(&pool)
            .await?;
    assert_eq!(spent, 1);
    let state = repository
        .load_goal(session(SESSION))
        .await?
        .expect("resumed goal is attached");
    assert_eq!(state.current().state(), &GoalState::Pursuing);

    pool.close().await;
    drop(container);
    Ok(())
}

/// resuming a blocked goal schedules exactly one next turn whose
/// accepted input is the exact optional user guidance.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s_goal_resume_delivers_guidance_to_the_next_turn() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attached_turn = turn_candidates(0xb10);
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(ATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement("finish the commissioned task")),
            ),
            Some(attached_turn),
            |_| None,
        )
        .await?;
    let failure_need =
        GoalNeed::try_new(String::from("repair execution")).expect("fixture need is admitted");
    assert_eq!(
        repository
            .block_execution_failure(
                session(SESSION),
                failure_need.clone(),
                GoalSchedulerProvenance::new(attached_turn.turn()),
            )
            .await?,
        GoalTransitionOutcome::NotCurrentGoalTurn
    );
    assert_eq!(
        activate_goal_turn(&pool, 0xd20).await?,
        attached_turn.turn()
    );
    terminalize_goal_turn_as_failed(&pool, 0xe20).await?;
    let blocked = repository
        .block_execution_failure(
            session(SESSION),
            failure_need.clone(),
            GoalSchedulerProvenance::new(attached_turn.turn()),
        )
        .await?;
    assert_applied_transition(blocked.clone());

    let guidance = GoalGuidance::try_new(String::from("use the newly granted credential"))
        .expect("fixture guidance is admitted");
    let resumed_turn = turn_candidates(0xb20);
    let resumed = repository
        .handle_user_command(
            GoalUserCommand::new(
                command(RESUME_COMMAND),
                session(SESSION),
                GoalUserAction::Resume(Some(guidance.clone())),
            ),
            Some(resumed_turn),
            |_| None,
        )
        .await?;
    assert_applied_command(resumed);
    assert_eq!(
        repository
            .block_execution_failure(
                session(SESSION),
                failure_need,
                GoalSchedulerProvenance::new(attached_turn.turn()),
            )
            .await?,
        blocked
    );
    let scheduler_blocks: i64 =
        sqlx::query_scalar("SELECT count(*) FROM goal_event WHERE scheduler_turn_id = $1")
            .bind(attached_turn.turn().into_uuid())
            .fetch_one(&pool)
            .await?;

    let resumed_content: String = sqlx::query_scalar(
        "SELECT part.text_value
           FROM accepted_input AS accepted
           JOIN accepted_input_content_part AS part
             ON part.accepted_input_id = accepted.accepted_input_id
            AND part.position = 0
          WHERE accepted.accepted_input_id = $1
            AND accepted.session_id = $2",
    )
    .bind(resumed_turn.accepted_input().into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .fetch_one(&pool)
    .await?;
    let resumed_goal_turns: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM goal_turn
          WHERE turn_id = $1
            AND source_event_ordinal IS NOT NULL",
    )
    .bind(resumed_turn.turn().into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(scheduler_blocks, 1);
    assert_eq!(resumed_content, guidance.as_str());
    assert_eq!(resumed_goal_turns, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// PostgreSQL round-trips the complete immutable goal lineage,
/// including its user receipts and atomic statement supersession.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s_goal_complete_lineage_round_trips() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let first_statement = statement("finish the commissioned task");
    let replacement_statement = statement("finish the replacement task");
    let attach = GoalUserCommand::new(
        command(ATTACH_COMMAND),
        session(SESSION),
        GoalUserAction::Attach(first_statement.clone()),
    );
    let commissioned = Goal::commission(
        session(SESSION),
        first_statement,
        GoalUserProvenance::new(command(ATTACH_COMMAND)),
    );
    let commissioned_event = latest_event(&commissioned);
    let attach_outcome =
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(commissioned_event));

    assert_eq!(
        repository
            .handle_user_command(attach.clone(), Some(turn_candidates(0xb01)), |_| None,)
            .await?,
        attach_outcome
    );
    assert_eq!(
        repository
            .handle_user_command(attach.clone(), Some(turn_candidates(0xb02)), |_| None,)
            .await?,
        attach_outcome
    );
    assert_eq!(
        repository
            .load_command(command(ATTACH_COMMAND))
            .await?
            .expect("attach receipt exists")
            .command(),
        &attach
    );

    let supersede = GoalUserCommand::new(
        command(SUPERSEDE_COMMAND),
        session(SESSION),
        GoalUserAction::Supersede(replacement_statement.clone()),
    );
    let superseded = commissioned
        .supersede(
            replacement_statement,
            GoalUserProvenance::new(command(SUPERSEDE_COMMAND)),
        )
        .expect("a pursuing goal can be superseded");
    let supersede_event = latest_event(&superseded);

    assert_eq!(
        repository
            .handle_user_command(supersede, Some(turn_candidates(0xb03)), |_| None,)
            .await?,
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(supersede_event))
    );

    let stop = GoalUserCommand::new(
        command(STOP_COMMAND),
        session(SESSION),
        GoalUserAction::Stop {
            descendant_scope: DescendantTerminationScope::ParentAlone,
        },
    );
    let stopped = superseded
        .stop(GoalUserProvenance::new(command(STOP_COMMAND)))
        .expect("a pursuing replacement can be stopped");
    let stop_event = latest_event(&stopped);

    assert_eq!(
        repository.handle_user_command(stop, None, |_| None).await?,
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(stop_event))
    );

    let later_statement = statement("finish a later commissioned task");
    let reattach = GoalUserCommand::new(
        command(REATTACH_COMMAND),
        session(SESSION),
        GoalUserAction::Attach(later_statement.clone()),
    );
    let recommissioned = stopped
        .commission_successor(
            later_statement,
            GoalUserProvenance::new(command(REATTACH_COMMAND)),
        )
        .expect("a user-stopped generation admits a later commission");
    let reattach_event = latest_event(&recommissioned);

    assert_eq!(
        repository
            .handle_user_command(reattach, Some(turn_candidates(0xb04)), |_| None,)
            .await?,
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(reattach_event))
    );
    assert_eq!(
        repository
            .load_goal(session(SESSION))
            .await?
            .expect("goal lineage exists"),
        recommissioned
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// persisted goal history rejects mutation after commit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn goal_event_history_is_append_only() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attach = GoalUserCommand::new(
        command(ATTACH_COMMAND),
        session(SESSION),
        GoalUserAction::Attach(statement("finish the commissioned task")),
    );
    repository
        .handle_user_command(attach, Some(turn_candidates(0xb01)), |_| None)
        .await?;

    let error =
        sqlx::query("UPDATE goal_event SET statement = 'edited in place' WHERE session_id = $1")
            .bind(Uuid::from_u128(SESSION))
            .execute(&pool)
            .await
            .expect_err("goal event mutation must be rejected");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.code())
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// superseding before activation makes the old queued statement
/// ineligible while the replacement remains the first runnable goal turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn supersede_retires_the_obsolete_queued_turn() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let obsolete = turn_candidates(0xb31);
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(ATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement("finish the original task")),
            ),
            Some(obsolete),
            |_| None,
        )
        .await?;
    let replacement = turn_candidates(0xb32);
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(SUPERSEDE_COMMAND),
                session(SESSION),
                GoalUserAction::Supersede(statement("finish the replacement task")),
            ),
            Some(replacement),
            |_| None,
        )
        .await?;

    let retired_rows: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM turn_terminal_outbox_event
          WHERE session_id = $1 AND turn_id = $2 AND disposition_kind = 'retired'",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(obsolete.turn().into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(retired_rows, 1);

    let dispatched = drain_dispatched(&pool).await?;
    // Retirement is published before the replacement's acceptance.
    assert_eq!(
        dispatched_kind_names(&dispatched),
        [
            "session_created",
            "goal_changed",
            "session_ownership_changed",
            "turn_model_settings_resolved",
            "input_accepted",
            "goal_changed",
            "turn_terminal",
            "turn_model_settings_resolved",
            "input_accepted",
        ]
    );
    assert_eq!(dispatched[6].session(), Some(session(SESSION)));
    assert_eq!(
        dispatched[6].kind(),
        &DispatchedOutboxEventKind::TurnTerminal {
            turn: obsolete.turn(),
            disposition: DispatchedTurnTerminalDisposition::Retired,
        }
    );
    assert_eq!(
        acceptance_position(&dispatched, replacement.turn()),
        Some(8)
    );

    assert_eq!(activate_goal_turn(&pool, 0xd31).await?, replacement.turn());

    pool.close().await;
    drop(container);
    Ok(())
}

/// a terminal current goal turn whose goal remains pursuing is a
/// durable reconciliation hint after process loss.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_current_goal_turn_is_a_reconciliation_hint() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attached = turn_candidates(0xb40);
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0x940),
                    session(SESSION),
                    GoalUserAction::Attach(statement("recover terminal goal work")),
                ),
                Some(attached),
                |_| None,
            )
            .await?,
    );
    assert_eq!(activate_goal_turn(&pool, 0xd40).await?, attached.turn());
    mark_goal_turn_completed(&pool, attached.turn()).await?;
    let (sessions, _dispatch_starts, continuation) = PostgresEligibilitySweep::new(pool.clone())
        .find_sessions()
        .await?
        .into_parts();

    assert_eq!(sessions, vec![session(SESSION)]);
    assert!(!continuation);

    pool.close().await;
    drop(container);
    Ok(())
}

/// stopping before activation retains the exact descendant
/// scope for replay, leaves no runnable goal work, and cannot block a later
/// explicit commission.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stop_scope_replays_and_retires_queued_work() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(ATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement("finish the stopped task")),
            ),
            Some(turn_candidates(0xb41)),
            |_| None,
        )
        .await?;
    let stop = GoalUserCommand::new(
        command(STOP_COMMAND),
        session(SESSION),
        GoalUserAction::Stop {
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
        },
    );
    let stopped = repository
        .handle_user_command(stop.clone(), None, |_| None)
        .await?;
    let replayed = repository.handle_user_command(stop, None, |_| None).await?;

    assert_eq!(replayed, stopped);

    assert_eq!(
        StartEligibleTurnRepository::new(pool.clone())
            .handle(session(SESSION), activation_identities(0xd41))
            .await?,
        StartEligibleTurnOutcome::NoEligibleTurn
    );
    let (stopped_sessions, _dispatch_starts, stopped_continuation) =
        PostgresEligibilitySweep::new(pool.clone())
            .find_sessions()
            .await?
            .into_parts();

    assert!(stopped_sessions.is_empty());
    assert!(!stopped_continuation);

    let successor = turn_candidates(0xb42);
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(REATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement("finish the later task")),
            ),
            Some(successor),
            |_| None,
        )
        .await?;
    assert_eq!(activate_goal_turn(&pool, 0xd42).await?, successor.turn());

    pool.close().await;
    drop(container);
    Ok(())
}

/// a stopped queued goal turn is immutable history and does not
/// remain a periodic reconciliation hint.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stopped_queued_goal_is_absent_from_reconciliation_hints() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(ATTACH_COMMAND),
                    session(SESSION),
                    GoalUserAction::Attach(statement("retire the queued goal")),
                ),
                Some(turn_candidates(0xb48)),
                |_| None,
            )
            .await?,
    );
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(STOP_COMMAND),
                    session(SESSION),
                    GoalUserAction::Stop {
                        descendant_scope: DescendantTerminationScope::ParentAlone,
                    },
                ),
                None,
                |_| None,
            )
            .await?,
    );
    let (sessions, _dispatch_starts, continuation) = PostgresEligibilitySweep::new(pool.clone())
        .find_sessions()
        .await?
        .into_parts();

    assert!(sessions.is_empty());
    assert!(!continuation);

    pool.close().await;
    drop(container);
    Ok(())
}

/// retiring a queued replacement keeps its immutable tail position
/// while excluding its turn from runtime scheduling.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stopped_replacement_does_not_corrupt_the_active_acceptance_tail()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let active = turn_candidates(0xb61);
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(ATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement("finish the original task")),
            ),
            Some(active),
            |_| None,
        )
        .await?;
    assert_eq!(activate_goal_turn(&pool, 0xd61).await?, active.turn());
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(SUPERSEDE_COMMAND),
                session(SESSION),
                GoalUserAction::Supersede(statement("finish the replacement task")),
            ),
            Some(turn_candidates(0xb62)),
            |_| None,
        )
        .await?;
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(STOP_COMMAND),
                session(SESSION),
                GoalUserAction::Stop {
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                },
            ),
            None,
            |_| None,
        )
        .await?;

    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            SubmitInput::new(
                command(STEER_COMMAND),
                session(SESSION),
                UserContent::try_text(String::from("steer the still-active original turn"))
                    .expect("fixture steering content is admitted"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xe61)),
            None,
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xf61)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xf62)),
            ),
            |_| panic!("steering cannot be reclassified while its source remains active"),
            |_| {
                panic!("steering cannot cancel a tool request without a terminal model observation")
            },
        )
        .await?;
    let pending_position: i64 = sqlx::query_scalar(
        "SELECT acceptance_position::bigint FROM accepted_input
          WHERE accepting_command_id = $1 AND disposition_kind = 'pending_steering'",
    )
    .bind(Uuid::from_u128(STEER_COMMAND))
    .fetch_one(&pool)
    .await?;
    let replacement_runtime_relevant: bool =
        sqlx::query_scalar("SELECT goal_turn_is_runtime_relevant($1, $2)")
            .bind(Uuid::from_u128(SESSION))
            .bind(turn_candidates(0xb62).turn().into_uuid())
            .fetch_one(&pool)
            .await?;

    assert_eq!(pending_position, 3);
    assert!(!replacement_runtime_relevant);
    let transcript = ProcessReadRepository::new(pool.clone())
        .read_transcript(session(SESSION))
        .await?
        .expect("the session transcript exists");
    assert_eq!(transcript.turns().len(), 1);
    assert_eq!(transcript.turns()[0].turn(), active.turn());

    pool.close().await;
    drop(container);
    Ok(())
}

/// an alias absent at acceptance is a replayable command rejection,
/// not repository corruption or a partially commissioned lineage.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unknown_goal_model_alias_is_durably_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let alias = ModelAlias::from_uuid(Uuid::from_u128(0xa11));
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_with_model(ModelSelectionRequest::Alias(alias)))
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attach = GoalUserCommand::new(
        command(ATTACH_COMMAND),
        session(SESSION),
        GoalUserAction::Attach(statement("finish the commissioned task")),
    );
    let expected = GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Rejected(
        GoalCommandRejection::UnknownModelAlias,
    ));

    assert_eq!(
        repository
            .handle_user_command(attach.clone(), Some(turn_candidates(0xb51)), |_| None)
            .await?,
        expected
    );
    assert_eq!(
        repository
            .handle_user_command(attach, Some(turn_candidates(0xb52)), |_| None)
            .await?,
        expected
    );
    assert_eq!(
        repository
            .load_command(command(ATTACH_COMMAND))
            .await?
            .expect("unknown-alias rejection receipt exists")
            .result(),
        &GoalCommandResult::Rejected(GoalCommandRejection::UnknownModelAlias)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM goal_event WHERE session_id = $1",)
            .bind(Uuid::from_u128(SESSION))
            .fetch_one(&pool)
            .await?,
        0
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// an applied command receipt can reference only the goal event that
/// carries that same durable command identity.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn applied_receipt_cannot_cross_wire_another_command_event() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let first = command(0x921);
    let second = command(0x922);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'goal', 1, transaction_timestamp(), 'operator'),
                ($2, 'goal', 1, transaction_timestamp(), 'operator')",
    )
    .bind(first.into_uuid())
    .bind(second.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_command
            (command_id, command_kind, storage_version, session_id,
             operation_kind, statement, result_kind, result_event_ordinal)
         VALUES ($1, 'goal', 1, $2, 'attach', $3, 'applied', $4::bigint),
                ($5, 'goal', 1, $2, 'supersede', $6, 'applied', $7::bigint)",
    )
    .bind(first.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind("finish the first task")
    .bind(2_i64)
    .bind(second.into_uuid())
    .bind("finish the replacement task")
    .bind(1_i64)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind, statement, user_command_id)
         VALUES ($1, $2::bigint, $3::bigint, 'commissioned', $4, $5),
                ($1, $6::bigint, $3::bigint, 'superseded', $7, $8)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(1_i64)
    .bind(1_i64)
    .bind("finish the first task")
    .bind(first.into_uuid())
    .bind(2_i64)
    .bind("finish the replacement task")
    .bind(second.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("cross-wired goal command receipts must fail at commit");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.code())
            .as_deref(),
        Some("23503")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// an applied goal command names only the event kind corresponding
/// to its immutable operation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn goal_command_operation_matches_the_applied_event_kind() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let mismatched = command(0x923);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'goal', 1, transaction_timestamp(), 'operator')",
    )
    .bind(mismatched.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_command
            (command_id, command_kind, storage_version, session_id,
             operation_kind, descendant_scope, result_kind, result_event_ordinal)
         VALUES ($1, 'goal', 1, $2, 'stop', 'parent_alone', 'applied', 1)",
    )
    .bind(mismatched.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind, statement, user_command_id)
         VALUES ($1, 1, 1, 'commissioned', $2, $3)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind("finish the mismatched task")
    .bind(mismatched.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("mismatched goal command operation and event kind must fail at commit");
    let database = error
        .as_database_error()
        .expect("deferred kind correlation reports a database constraint");

    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(
        database.constraint(),
        Some("goal_command_applied_event_kind")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// exhausting the session acceptance ordinal yields typed scheduler
/// backpressure and a durable, replayable user-command rejection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn goal_turn_acceptance_position_exhaustion_is_typed_and_durable()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attached_turn = turn_candidates(0xb81);
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0x981),
                    session(SESSION),
                    GoalUserAction::Attach(statement("exhaust the acceptance position")),
                ),
                Some(attached_turn),
                |_| None,
            )
            .await?,
    );
    let maximum = SessionInputPosition::try_from_u64(u64::MAX)
        .expect("the maximum positive position is admitted");
    set_goal_turn_acceptance_position(&pool, attached_turn.turn(), maximum).await?;
    assert_eq!(
        activate_goal_turn(&pool, 0xd81).await?,
        attached_turn.turn()
    );
    mark_goal_turn_completed(&pool, attached_turn.turn()).await?;
    let history_before_rejection = repository
        .load_goal(session(SESSION))
        .await?
        .expect("the attached lineage exists");

    assert_eq!(
        repository
            .reconcile_current_after_execution(
                session(SESSION),
                turn_candidates(0xb82),
                GoalNeed::try_new(String::from("repair execution"))
                    .expect("fixture need is admitted"),
                |_| None,
            )
            .await?,
        GoalTurnContinuationOutcome::AcceptancePositionExhausted { last: maximum }
    );

    let supersede = GoalUserCommand::new(
        command(0x982),
        session(SESSION),
        GoalUserAction::Supersede(statement("replacement cannot yet be queued")),
    );
    let rejected = GoalCommandResult::Rejected(GoalCommandRejection::AcceptancePositionExhausted);
    assert_eq!(
        repository
            .handle_user_command(supersede.clone(), Some(turn_candidates(0xb83)), |_| None,)
            .await?,
        GoalCommandHandlingOutcome::Recorded(rejected.clone())
    );
    let receipt = repository
        .load_command(supersede.command_id())
        .await?
        .expect("the exhaustion receipt is durable");
    assert_eq!(receipt.command(), &supersede);
    assert_eq!(receipt.result(), &rejected);
    let history_after_rejection = repository
        .load_goal(session(SESSION))
        .await?
        .expect("the attached lineage remains");
    assert_eq!(
        history_after_rejection.events(),
        history_before_rejection.events()
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// neither a delayed model declaration nor an unrecorded scheduler
/// failure from an older turn can block a resumed goal whose newer turn is current.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delayed_old_turn_transitions_do_not_block_the_resumed_turn() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let failed_turn = turn_candidates(0xb91);
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0x991),
                    session(SESSION),
                    GoalUserAction::Attach(statement("resume beyond the failed turn")),
                ),
                Some(failed_turn),
                |_| None,
            )
            .await?,
    );
    assert_eq!(activate_goal_turn(&pool, 0xd91).await?, failed_turn.turn());
    terminalize_goal_turn_as_failed(&pool, 0xe91).await?;
    let need_text = "wait for user input";
    let need = GoalNeed::try_new(String::from(need_text)).expect("fixture need is admitted");
    let request = tool_request(0xf91);
    let declaration_arguments =
        String::from(r#"{"reason":"user_input_required","transition":"blocked"}"#);
    insert_goal_tool_request(
        &pool,
        failed_turn.turn(),
        request,
        "goal_declare",
        &declaration_arguments,
        need_text,
    )
    .await?;
    assert_applied_transition(
        repository
            .declare_blocked(
                session(SESSION),
                GoalModelBlockedReasonKind::UserInputRequired,
                need,
                GoalModelProvenance::new(failed_turn.turn(), request),
            )
            .await?,
    );
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0x992),
                    session(SESSION),
                    GoalUserAction::Resume(None),
                ),
                Some(turn_candidates(0xb92)),
                |_| None,
            )
            .await?,
    );

    assert_eq!(
        repository
            .declare_blocked(
                session(SESSION),
                GoalModelBlockedReasonKind::UserInputRequired,
                GoalNeed::try_new(String::from("stale model need"))
                    .expect("fixture need is admitted"),
                GoalModelProvenance::new(failed_turn.turn(), request),
            )
            .await?,
        GoalTransitionOutcome::NotCurrentGoalTurn
    );
    assert_eq!(
        repository
            .block_execution_failure(
                session(SESSION),
                GoalNeed::try_new(String::from("repair the old failed turn"))
                    .expect("fixture failure need is admitted"),
                GoalSchedulerProvenance::new(failed_turn.turn()),
            )
            .await?,
        GoalTransitionOutcome::NotCurrentGoalTurn
    );
    let scheduler_blocks: i64 =
        sqlx::query_scalar("SELECT count(*) FROM goal_event WHERE scheduler_turn_id = $1")
            .bind(failed_turn.turn().into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(scheduler_blocks, 0);

    pool.close().await;
    drop(container);
    Ok(())
}

/// a continuation can name only the acceptance-latest goal turn,
/// so a resumed turn prevents a stale completed predecessor from branching.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn continuation_requires_the_latest_goal_turn() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let first = turn_candidates(0xbc1);
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0x9c1),
                    session(SESSION),
                    GoalUserAction::Attach(statement("resume before continuing")),
                ),
                Some(first),
                |_| None,
            )
            .await?,
    );
    assert_eq!(activate_goal_turn(&pool, 0xdc1).await?, first.turn());
    terminalize_goal_turn_as_failed(&pool, 0xec1).await?;
    assert_applied_transition(
        repository
            .block_execution_failure(
                session(SESSION),
                GoalNeed::try_new(String::from("repair the failed turn"))
                    .expect("fixture need is admitted"),
                GoalSchedulerProvenance::new(first.turn()),
            )
            .await?,
    );
    let resumed = turn_candidates(0xbc2);
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0x9c2),
                    session(SESSION),
                    GoalUserAction::Resume(None),
                ),
                Some(resumed),
                |_| None,
            )
            .await?,
    );
    assert_eq!(activate_goal_turn(&pool, 0xdc2).await?, resumed.turn());
    mark_goal_turn_completed(&pool, resumed.turn()).await?;
    let successor = turn_candidates(0xbc3);
    assert_eq!(
        repository
            .reconcile_current_after_execution(
                session(SESSION),
                successor,
                GoalNeed::try_new(String::from("repair execution"))
                    .expect("fixture need is admitted"),
                |_| None,
            )
            .await?,
        GoalTurnContinuationOutcome::Scheduled {
            turn: successor.turn(),
        }
    );
    sqlx::query("ALTER TABLE goal_turn DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM goal_turn WHERE session_id = $1 AND turn_id = $2")
        .bind(Uuid::from_u128(SESSION))
        .bind(successor.turn().into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE goal_turn ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal', terminal_disposition_kind = 'completed',
                terminal_cause_kind = 'completed',
                terminal_frontier_id = $3, active_phase_kind = NULL,
                terminal_attempt_id = $4, current_attempt_id = NULL,
                terminal_model_call_id = $5
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(first.turn().into_uuid())
    .bind(Uuid::from_u128(0xdc4))
    .bind(Uuid::from_u128(0xdc5))
    .bind(Uuid::from_u128(0xdc6))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO goal_turn
            (session_id, goal_generation, turn_id, accepted_input_id,
             source_event_ordinal, predecessor_turn_id)
         VALUES ($1, 1, $2, $3, NULL, $4)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(successor.turn().into_uuid())
    .bind(successor.accepted_input().into_uuid())
    .bind(first.turn().into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a stale completed goal turn cannot branch after resume");

    assert_database_constraint(error, "goal_turn_latest_predecessor");

    pool.close().await;
    drop(container);
    Ok(())
}

/// a direct model event cannot name an older turn from the current
/// goal generation after a successor has become current.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_event_requires_the_current_goal_turn() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let attached = turn_candidates(0xb58);
    let successor = turn_candidates(0xb59);
    completed_goal_with_successor(&pool, attached, successor).await?;
    let request = tool_request(0xf58);
    insert_goal_tool_request(
        &pool,
        attached.turn(),
        request,
        "goal_declare",
        r#"{"reason":"user_input_required","transition":"blocked"}"#,
        "stale model need",
    )
    .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind, blocked_reason,
             need, model_turn_id, model_tool_request_id)
         VALUES ($1, 2, 1, 'blocked', 'user_input_required', $2, $3, $4)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind("stale model need")
    .bind(attached.turn().into_uuid())
    .bind(request.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("an older same-generation turn cannot source a model event");

    assert_database_constraint(error, "goal_event_model_current_turn");

    pool.close().await;
    drop(container);
    Ok(())
}

/// a direct scheduler event cannot name an unsuccessful older turn
/// after a same-generation successor has become current.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn scheduler_event_requires_the_current_goal_turn() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let attached = turn_candidates(0xb5a);
    let successor = turn_candidates(0xb5b);
    completed_goal_with_successor(&pool, attached, successor).await?;
    mark_completed_goal_turn_failed(&pool, attached.turn()).await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind, blocked_reason,
             need, scheduler_turn_id)
         VALUES ($1, 2, 1, 'blocked', 'execution_failure', $2, $3)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind("repair the stale failed turn")
    .bind(attached.turn().into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("an older same-generation turn cannot source a scheduler event");

    assert_database_constraint(error, "goal_event_scheduler_failure_turn");

    pool.close().await;
    drop(container);
    Ok(())
}

/// a direct scheduler failure event must name a terminal turn whose
/// disposition is unsuccessful.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn scheduler_event_requires_an_unsuccessful_terminal_turn() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let queued = turn_candidates(0xb5c);
    assert_applied_command(
        GoalRepository::new(pool.clone())
            .handle_user_command(
                GoalUserCommand::new(
                    command(ATTACH_COMMAND),
                    session(SESSION),
                    GoalUserAction::Attach(statement("reject premature scheduler blocking")),
                ),
                Some(queued),
                |_| None,
            )
            .await?,
    );
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind, blocked_reason,
             need, scheduler_turn_id)
         VALUES ($1, 2, 1, 'blocked', 'execution_failure', $2, $3)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind("turn has not failed")
    .bind(queued.turn().into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a queued turn cannot source scheduler failure blocking");

    assert_database_constraint(error, "goal_event_scheduler_failure_turn");

    pool.close().await;
    drop(container);
    Ok(())
}

/// model goal events bind to the exact `goal_declare` name and
/// canonical arguments and adjacent declaration text carried by their trusted
/// request identity.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_goal_declaration_request_matches_name_and_arguments() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attached_turn = turn_candidates(0xba1);
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0x9a1),
                    session(SESSION),
                    GoalUserAction::Attach(statement("bind the declaration request")),
                ),
                Some(attached_turn),
                |_| None,
            )
            .await?,
    );
    let need =
        GoalNeed::try_new(String::from("wait for user input")).expect("fixture need is admitted");
    let matching_arguments = r#"{"reason":"user_input_required","transition":"blocked"}"#;
    let wrong_name = tool_request(0xfa1);
    insert_goal_tool_request(
        &pool,
        attached_turn.turn(),
        wrong_name,
        "inspect",
        matching_arguments,
        "wait for user input",
    )
    .await?;
    let name_error = repository
        .declare_blocked(
            session(SESSION),
            GoalModelBlockedReasonKind::UserInputRequired,
            need.clone(),
            GoalModelProvenance::new(attached_turn.turn(), wrong_name),
        )
        .await
        .expect_err("an unrelated tool request cannot source a goal event");
    assert_model_declaration_request_rejected(name_error);

    let wrong_arguments = tool_request(0xfa2);
    insert_goal_tool_request(
        &pool,
        attached_turn.turn(),
        wrong_arguments,
        "goal_declare",
        r#"{"reason":"external_change_required","transition":"blocked"}"#,
        "wait for user input",
    )
    .await?;
    let arguments_error = repository
        .declare_blocked(
            session(SESSION),
            GoalModelBlockedReasonKind::UserInputRequired,
            need.clone(),
            GoalModelProvenance::new(attached_turn.turn(), wrong_arguments),
        )
        .await
        .expect_err("mismatched canonical arguments cannot source a goal event");
    assert_model_declaration_request_rejected(arguments_error);

    let wrong_text = tool_request(0xfa3);
    insert_goal_tool_request(
        &pool,
        attached_turn.turn(),
        wrong_text,
        "goal_declare",
        matching_arguments,
        "different need",
    )
    .await?;
    let text_error = repository
        .declare_blocked(
            session(SESSION),
            GoalModelBlockedReasonKind::UserInputRequired,
            need,
            GoalModelProvenance::new(attached_turn.turn(), wrong_text),
        )
        .await
        .expect_err("mismatched adjacent declaration text cannot source a goal event");
    assert_model_declaration_request_rejected(text_error);

    pool.close().await;
    drop(container);
    Ok(())
}

/// goal_declare is rejected when another response part follows it,
/// preventing later tool effects after a terminal goal transition.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_goal_declaration_is_the_final_response_part() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attached_turn = turn_candidates(0xba4);
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0x9a4),
                    session(SESSION),
                    GoalUserAction::Attach(statement("finish before any later tool request")),
                ),
                Some(attached_turn),
                |_| None,
            )
            .await?,
    );
    let report_text = String::from("finished without later effects");
    let declaration_request = tool_request(0xfa4);
    insert_goal_tool_request(
        &pool,
        attached_turn.turn(),
        declaration_request,
        "goal_declare",
        r#"{"transition":"achieved"}"#,
        &report_text,
    )
    .await?;
    insert_following_tool_request(
        &pool,
        attached_turn.turn(),
        declaration_request,
        tool_request(0xfa5),
    )
    .await?;
    let provenance = GoalModelProvenance::new(attached_turn.turn(), declaration_request);

    assert_eq!(
        repository
            .load_model_declaration_text(session(SESSION), provenance)
            .await?,
        None
    );
    let error = repository
        .declare_achieved(
            session(SESSION),
            GoalReport::try_new(report_text).expect("fixture report is admitted"),
            provenance,
            signalbox_domain::FinishCheckVerdict::Unverified,
        )
        .await
        .expect_err("a nonfinal declaration cannot source a goal event");
    assert_model_declaration_request_rejected(error);

    pool.close().await;
    drop(container);
    Ok(())
}

/// A committed closure makes a later model achievement non-current, so
/// settlement cannot be stranded behind a contradictory terminal goal event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_committed_closure_refuses_late_model_achievement() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attached_turn = turn_candidates(0xba6);
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0x9a6),
                    session(SESSION),
                    GoalUserAction::Attach(statement("finish before the closure commits")),
                ),
                Some(attached_turn),
                |_| None,
            )
            .await?,
    );
    assert_eq!(
        activate_goal_turn(&pool, 0xda6).await?,
        attached_turn.turn()
    );
    let report = String::from("late achievement");
    let declaration_request = tool_request(0xfa6);
    insert_goal_tool_request(
        &pool,
        attached_turn.turn(),
        declaration_request,
        "goal_declare",
        r#"{"transition":"achieved"}"#,
        &report,
    )
    .await?;
    SessionLifecycleRepository::new(pool.clone())
        .commit_pending_terminal(
            session(SESSION),
            SessionTerminalOutcome::FailedUnknown,
            LifecycleActor::Watchdog,
        )
        .await?;

    let outcome = repository
        .declare_achieved(
            session(SESSION),
            GoalReport::try_new(report).expect("fixture report is admitted"),
            GoalModelProvenance::new(attached_turn.turn(), declaration_request),
            signalbox_domain::FinishCheckVerdict::Unverified,
        )
        .await?;

    assert_eq!(outcome, GoalTransitionOutcome::NotCurrentGoalTurn);
    assert_eq!(
        repository
            .load_goal(session(SESSION))
            .await?
            .expect("the goal remains attached")
            .current()
            .state(),
        &GoalState::Pursuing
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// the adjacent transcript representation carries the domain's full
/// 1 MiB goal-report bound without widening normalized tool arguments.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_goal_declaration_carries_full_report_bound() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attached_turn = turn_candidates(0xbb1);
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0x9b1),
                    session(SESSION),
                    GoalUserAction::Attach(statement("carry the full final report")),
                ),
                Some(attached_turn),
                |_| None,
            )
            .await?,
    );
    let report_text = "r".repeat(1_048_576);
    let report = GoalReport::try_new(report_text.clone()).expect("the exact bound is admitted");
    let request = tool_request(0xfb1);
    insert_goal_tool_request(
        &pool,
        attached_turn.turn(),
        request,
        "goal_declare",
        r#"{"transition":"achieved"}"#,
        &report_text,
    )
    .await?;

    assert_eq!(
        repository
            .load_model_declaration_text(
                session(SESSION),
                GoalModelProvenance::new(attached_turn.turn(), request),
            )
            .await?,
        Some(report_text)
    );
    assert_applied_transition(
        repository
            .declare_achieved(
                session(SESSION),
                report,
                GoalModelProvenance::new(attached_turn.turn(), request),
                signalbox_domain::FinishCheckVerdict::Unverified,
            )
            .await?,
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// the schema admits one goal declaration event per trusted model
/// tool-request identity.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_goal_declaration_request_is_single_use() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let unique_request: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1
              FROM pg_constraint
             WHERE conrelid = 'goal_event'::regclass
               AND contype = 'u'
               AND pg_get_constraintdef(oid) = 'UNIQUE (model_tool_request_id)'
        )",
    )
    .fetch_one(&pool)
    .await?;

    assert!(unique_request);

    pool.close().await;
    drop(container);
    Ok(())
}

/// a changed current alias that is unavailable at reconciliation is
/// a typed continuation outcome, not durable-state corruption.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn changed_unknown_alias_is_a_typed_continuation_outcome() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let first_alias = ModelAlias::from_uuid(Uuid::from_u128(0xa21));
    let changed_alias = ModelAlias::from_uuid(Uuid::from_u128(0xa22));
    let frozen =
        FrozenAliasDefinition::selecting(DirectModelSelection::from_uuid(Uuid::from_u128(0xa23)));
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_with_model(ModelSelectionRequest::Alias(
            first_alias,
        )))
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attached_turn = turn_candidates(0xb71);
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(0x971),
                session(SESSION),
                GoalUserAction::Attach(statement("continue after the defaults change")),
            ),
            Some(attached_turn),
            |alias| {
                assert_eq!(alias, first_alias);
                Some(frozen)
            },
        )
        .await?;
    assert_eq!(
        activate_goal_turn(&pool, 0xd71).await?,
        attached_turn.turn()
    );
    mark_goal_turn_completed(&pool, attached_turn.turn()).await?;
    let replacement = ReplaceSessionDefaults::new(
        command(0x972),
        session(SESSION),
        SessionConfigurationDefaultsVersion::first(),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Alias(changed_alias)),
    );
    let replaced = ReplaceSessionDefaultsRepository::new(pool.clone())
        .handle(replacement)
        .await?;
    assert!(matches!(
        replaced,
        ReplaceSessionDefaultsHandlingOutcome::Applied(_)
    ));
    assert_eq!(
        repository
            .reconcile_current_after_execution(
                session(SESSION),
                turn_candidates(0xb72),
                GoalNeed::try_new(String::from("repair execution"))
                    .expect("fixture need is admitted"),
                |_| None,
            )
            .await?,
        GoalTurnContinuationOutcome::UnknownModelAlias {
            alias: changed_alias
        }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// every user-provenance event names an applied receipt at that exact
/// event ordinal; rejected commands cannot source events.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn rejected_goal_command_cannot_source_an_event() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let rejected = command(0x972);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'goal', 1, transaction_timestamp(), 'operator')",
    )
    .bind(rejected.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_command
            (command_id, command_kind, storage_version, session_id,
             operation_kind, statement, result_kind, rejection_kind)
         VALUES ($1, 'goal', 1, $2, 'attach', $3, 'rejected',
                 'goal_already_attached')",
    )
    .bind(rejected.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind("rejected statement")
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind, statement, user_command_id)
         VALUES ($1, 1, 1, 'commissioned', $2, $3)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind("rejected statement")
    .bind(rejected.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a rejected command cannot source a goal event");
    let database = error
        .as_database_error()
        .expect("deferred receipt correlation reports a database constraint");

    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(
        database.constraint(),
        Some("goal_event_applied_command_receipt")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// applied command receipts and their exact events carry the same
/// immutable statement or optional guidance payload.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn goal_command_payload_matches_the_applied_event() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let mismatched = command(0x973);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'goal', 1, transaction_timestamp(), 'operator')",
    )
    .bind(mismatched.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_command
            (command_id, command_kind, storage_version, session_id,
             operation_kind, statement, result_kind, result_event_ordinal)
         VALUES ($1, 'goal', 1, $2, 'attach', $3, 'applied', 1)",
    )
    .bind(mismatched.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind("receipt statement")
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind, statement, user_command_id)
         VALUES ($1, 1, 1, 'commissioned', $2, $3)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind("different event statement")
    .bind(mismatched.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("an applied receipt cannot disagree with its event payload");
    let database = error
        .as_database_error()
        .expect("deferred payload correlation reports a database constraint");

    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(
        database.constraint(),
        Some("goal_command_applied_event_kind")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// every pursuit-starting user event atomically creates exactly one
/// goal-owned accepted input and turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pursuing_goal_event_requires_its_goal_turn() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let applied = command(0x974);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'goal', 1, transaction_timestamp(), 'operator')",
    )
    .bind(applied.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_command
            (command_id, command_kind, storage_version, session_id,
             operation_kind, statement, result_kind, result_event_ordinal)
         VALUES ($1, 'goal', 1, $2, 'attach', $3, 'applied', 1)",
    )
    .bind(applied.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind("unscheduled statement")
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind, statement, user_command_id)
         VALUES ($1, 1, 1, 'commissioned', $2, $3)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind("unscheduled statement")
    .bind(applied.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a pursuit-starting event cannot omit its goal turn");
    let database = error
        .as_database_error()
        .expect("deferred turn correlation reports a database constraint");

    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(database.constraint(), Some("goal_event_pursuing_turn"));

    pool.close().await;
    drop(container);
    Ok(())
}

/// a queued goal turn's requested and frozen configuration derive
/// from the exact defaults epoch named by its accepted input.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn goal_turn_configuration_matches_its_defaults_epoch() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let applied = command(0x975);
    let accepted = AcceptedInputId::from_uuid(Uuid::from_u128(0xa75));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xb75));
    let mismatched_selection = Uuid::from_u128(0xc75);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'goal', 1, transaction_timestamp(), 'operator')",
    )
    .bind(applied.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_command
            (command_id, command_kind, storage_version, session_id,
             operation_kind, statement, result_kind, result_event_ordinal)
         VALUES ($1, 'goal', 1, $2, 'attach', $3, 'applied', 1)",
    )
    .bind(applied.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind("misconfigured turn")
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind, statement, user_command_id)
         VALUES ($1, 1, 1, 'commissioned', $2, $3)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind("misconfigured turn")
    .bind(applied.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO accepted_input
            (accepted_input_id, accepting_command_id, session_id,
             delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             acceptance_position, disposition_kind, origin_turn_id)
         VALUES ($1, NULL, $2, 'start_when_no_active_turn',
                 NULL, 1, 'use_session_default', NULL, NULL, NULL,
                 1, 'origin_of', $3)",
    )
    .bind(accepted.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind(turn.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO accepted_input_content_part
            (accepted_input_id, position, part_kind, text_value)
         VALUES ($1, 0, 'text', $2)",
    )
    .bind(accepted.into_uuid())
    .bind("misconfigured turn")
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO queued_input_origin
            (turn_id, accepted_input_id, session_id, acceptance_position,
             priority_kind, defaults_version, interrupt_predecessor_turn_id,
             requested_model_kind, requested_direct_model_selection_id,
             requested_model_alias_id, frozen_model_kind,
             frozen_direct_model_selection_id, frozen_model_alias_id,
             frozen_alias_selected_direct_id, model_parameters,
             known_provider_failure_retry, model_fallback,
             dangerous_tool_auto_approval)
         VALUES ($1, $2, $3, 1, 'ordinary', 1, NULL,
                 'direct', $4, NULL, 'direct', $4, NULL, NULL,
                 'provider_defaults', 'disabled', 'disabled', 'disabled')",
    )
    .bind(turn.into_uuid())
    .bind(accepted.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind(mismatched_selection)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_accepted_input_id,
             acceptance_position, state_kind)
         VALUES ($1, $2, $3, 1, 'queued')",
    )
    .bind(turn.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind(accepted.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_turn
            (session_id, goal_generation, turn_id, accepted_input_id,
             source_event_ordinal, predecessor_turn_id)
         VALUES ($1, 1, $2, $3, 1, NULL)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(turn.into_uuid())
    .bind(accepted.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a goal turn cannot cross-wire another defaults selection");
    let database = error
        .as_database_error()
        .expect("deferred configuration correlation reports a database constraint");

    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(database.constraint(), Some("goal_turn_runtime_shape"));

    pool.close().await;
    drop(container);
    Ok(())
}

/// rejection reasons are closed over the operation paths that can
/// produce them.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn goal_command_rejection_matches_its_operation() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let impossible = command(0x976);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'goal', 1, transaction_timestamp(), 'operator')",
    )
    .bind(impossible.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = sqlx::query(
        "INSERT INTO goal_command
            (command_id, command_kind, storage_version, session_id,
             operation_kind, descendant_scope, result_kind, rejection_kind)
         VALUES ($1, 'goal', 1, $2, 'stop', 'parent_alone',
                 'rejected', 'unknown_model_alias')",
    )
    .bind(impossible.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .execute(&mut *transaction)
    .await
    .expect_err("stop cannot record an alias-resolution rejection");
    let database = error
        .as_database_error()
        .expect("operation rejection correlation reports a database constraint");

    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(
        database.constraint(),
        Some("goal_command_rejection_operation")
    );
    drop(transaction);

    pool.close().await;
    drop(container);
    Ok(())
}

/// goal-owned turn admission takes the scheduler row lock before it
/// inserts any accepted-input or turn-lifecycle fact.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn goal_turn_insert_waits_for_scheduler_lock() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;

    let mut scheduler_blocker = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
        .bind(Uuid::from_u128(SESSION))
        .execute(&mut *scheduler_blocker)
        .await?;

    let attach = tokio::spawn({
        let repository = GoalRepository::new(pool.clone());
        async move {
            repository
                .handle_user_command(
                    GoalUserCommand::new(
                        command(0x9a0),
                        session(SESSION),
                        GoalUserAction::Attach(statement("wait behind the scheduler lock")),
                    ),
                    Some(turn_candidates(0xba0)),
                    |_| None,
                )
                .await
        }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "goal turn admission must wait on the held scheduler row"
    );
    let turns_while_blocked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM turn_lifecycle WHERE session_id = $1")
            .bind(Uuid::from_u128(SESSION))
            .fetch_one(&pool)
            .await?;
    assert_eq!(turns_while_blocked, 0);

    scheduler_blocker.rollback().await?;
    assert_applied_command(attach.await??);
    let turns_after_release: i64 =
        sqlx::query_scalar("SELECT count(*) FROM turn_lifecycle WHERE session_id = $1")
            .bind(Uuid::from_u128(SESSION))
            .fetch_one(&pool)
            .await?;
    assert_eq!(turns_after_release, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// Delivery evidence one descendant-scoped cascade publishes: the bound child's
/// single relationship result, the dispositions addressed to the commanding
/// parent and to each terminalized child, the result updates, and the wake the
/// bound child's result raises.
#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct CascadeDeliveryFacts {
    bound_child_results: i64,
    parent_addressed_dispositions: i64,
    child_addressed_dispositions: i64,
    child_result_updates: i64,
    bound_child_result_wakes: i64,
}

struct DescendantLockOrderFixture {
    parent: SessionId,
    child: SessionId,
    spawning_request: ToolRequestId,
    active_turn: TurnId,
}

async fn descendant_lock_order_fixture(
    pool: &PgPool,
    seed: u128,
) -> Result<DescendantLockOrderFixture, Box<dyn Error>> {
    let parent = seed + 2;
    let child = seed + 1;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(seed + 3, parent, seed + 4))
        .await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(seed + 5, child, seed + 6))
        .await?;
    insert_queued_delegation_fixture(
        pool,
        DelegationFixture {
            spawning_request: seed + 7,
            parent_session: parent,
            parent_turn: seed + 8,
            child_session: child,
            child_turn: seed + 9,
            task_entry: seed + 10,
            selection: seed + 6,
            policy_kind: "bound",
            on_parent_stopped: Some("stop"),
            on_parent_cancelled: Some("cancel"),
        },
    )
    .await?;
    let candidates = turn_candidates(seed + 20);
    assert_applied_command(
        GoalRepository::new(pool.clone())
            .handle_user_command(
                GoalUserCommand::new(
                    command(seed + 21),
                    session(parent),
                    GoalUserAction::Attach(statement("exercise descendant lock ordering")),
                ),
                Some(candidates),
                |_| None,
            )
            .await?,
    );
    let active_turn = activated_turn(
        StartEligibleTurnRepository::new(pool.clone())
            .handle(session(parent), activation_identities(seed + 30))
            .await?,
    );
    Ok(DescendantLockOrderFixture {
        parent: session(parent),
        child: session(child),
        spawning_request: tool_request(seed + 7),
        active_turn,
    })
}

async fn acquire_peer_message_suffix(
    connection: &mut sqlx::PgConnection,
    fixture: &DescendantLockOrderFixture,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE")
        .bind(fixture.parent.into_uuid())
        .execute(&mut *connection)
        .await?;
    sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
        .bind(fixture.child.into_uuid())
        .execute(&mut *connection)
        .await?;
    sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
        .bind(fixture.parent.into_uuid())
        .execute(&mut *connection)
        .await?;
    sqlx::query(
        "SELECT spawning_tool_request_id
           FROM session_delegation
          WHERE spawning_tool_request_id = $1
          FOR UPDATE",
    )
    .bind(fixture.spawning_request.into_uuid())
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// S19: descendant-scoped goal stop takes its canonical
/// cascade prefix before the ordinary root lock, so an overlapping peer-message
/// prefix cannot form the child/root inversion that PostgreSQL reports as
/// `40P01`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s19_goal_stop_orders_cascade_before_peer_message() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let seed = 0xfb00;
    let fixture = descendant_lock_order_fixture(&pool, seed).await?;
    let mut peer_message = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE")
        .bind(fixture.child.into_uuid())
        .execute(&mut *peer_message)
        .await?;
    let stop = tokio::spawn({
        let repository = GoalRepository::new(pool.clone());
        async move {
            repository
                .handle_user_command(
                    GoalUserCommand::new(
                        command(seed + 40),
                        fixture.parent,
                        GoalUserAction::Stop {
                            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                        },
                    ),
                    None,
                    |_| None,
                )
                .await
        }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "goal stop must wait for the lower child before taking its root lock"
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        acquire_peer_message_suffix(&mut peer_message, &fixture),
    )
    .await??;
    peer_message.rollback().await?;
    assert_applied_command(stop.await??);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S19: descendant-scoped input interrupt takes the same
/// canonical cascade prefix before its root and scheduler locks, preventing the
/// peer-message child/root inversion.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s19_input_interrupt_orders_cascade_before_peer_message() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let seed = 0xfc00;
    let fixture = descendant_lock_order_fixture(&pool, seed).await?;
    let mut peer_message = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE")
        .bind(fixture.child.into_uuid())
        .execute(&mut *peer_message)
        .await?;
    let interrupt = tokio::spawn({
        let repository = SubmitInputRepository::new(pool.clone());
        async move {
            repository
                .handle_with_candidates(
                    SubmitInput::new(
                        command(seed + 40),
                        fixture.parent,
                        UserContent::try_text(String::from("interrupt descendants"))
                            .expect("fixture input content is admitted"),
                        DeliveryRequest::Interrupt {
                            expected_active_turn: fixture.active_turn,
                            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                            configuration: PerInputConfigurationChoices::new(
                                SessionConfigurationDefaultsVersion::first(),
                                ModelSelectionOverride::UseSessionDefault,
                            ),
                        },
                    ),
                    AcceptedInputId::from_uuid(Uuid::from_u128(seed + 41)),
                    Some(TurnId::from_uuid(Uuid::from_u128(seed + 42))),
                    CancelledModelCallTurnIdentities::new(
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 43)),
                        ContextFrontierId::from_uuid(Uuid::from_u128(seed + 44)),
                    ),
                    |_| panic!("the fixture has no steering to reclassify"),
                    |_| panic!("the fixture has no tool batch to cancel"),
                )
                .await
        }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "input interrupt must wait for the lower child before taking its root lock"
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        acquire_peer_message_suffix(&mut peer_message, &fixture),
    )
    .await??;
    peer_message.rollback().await?;
    assert!(matches!(
        interrupt.await??,
        signalbox_persistence::submit_input::SubmitInputHandlingOutcome::Recorded(
            SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(_))
        )
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S19: when the descendant-scope root is itself a
/// delegated child, the canonical session frontier includes its parent
/// endpoint in the same ascending lock set.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s19_descendant_frontier_includes_root_parent_endpoint() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let seed = 0xfca0;
    let grandparent = seed + 1;
    let descendant = seed + 2;
    let parent = seed + 3;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(seed + 4, grandparent, seed + 5))
        .await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(seed + 6, parent, seed + 7))
        .await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(seed + 8, descendant, seed + 9))
        .await?;
    assert_applied_command(
        GoalRepository::new(pool.clone())
            .handle_user_command(
                GoalUserCommand::new(
                    command(seed + 10),
                    session(grandparent),
                    GoalUserAction::Attach(statement("establish the ancestor turn")),
                ),
                Some(turn_candidates(seed + 11)),
                |_| None,
            )
            .await?,
    );
    let grandparent_turn = activated_turn(
        StartEligibleTurnRepository::new(pool.clone())
            .handle(session(grandparent), activation_identities(seed + 20))
            .await?,
    );
    let parent_turn = seed + 30;
    insert_queued_delegation_fixture(
        &pool,
        DelegationFixture {
            spawning_request: seed + 31,
            parent_session: grandparent,
            parent_turn: grandparent_turn.as_uuid().as_u128(),
            child_session: parent,
            child_turn: parent_turn,
            task_entry: seed + 32,
            selection: seed + 7,
            policy_kind: "bound",
            on_parent_stopped: Some("stop"),
            on_parent_cancelled: Some("cancel"),
        },
    )
    .await?;
    insert_queued_delegation_fixture(
        &pool,
        DelegationFixture {
            spawning_request: seed + 33,
            parent_session: parent,
            parent_turn,
            child_session: descendant,
            child_turn: seed + 34,
            task_entry: seed + 35,
            selection: seed + 9,
            policy_kind: "bound",
            on_parent_stopped: Some("stop"),
            on_parent_cancelled: Some("cancel"),
        },
    )
    .await?;

    let mut ancestor_lock = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE")
        .bind(Uuid::from_u128(grandparent))
        .execute(&mut *ancestor_lock)
        .await?;
    let lock_pool = pool.clone();
    let frontier = tokio::spawn(async move {
        sqlx::query("SELECT lock_delegation_termination_session_frontier($1, 'cancelled')")
            .bind(Uuid::from_u128(parent))
            .execute(&lock_pool)
            .await
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the descendant frontier must wait on the root's parent endpoint"
    );
    sqlx::query("SET LOCAL lock_timeout = '250ms'")
        .execute(&mut *ancestor_lock)
        .await?;
    let locked_descendant: Uuid = sqlx::query_scalar(
        "SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE",
    )
    .bind(Uuid::from_u128(descendant))
    .fetch_one(&mut *ancestor_lock)
    .await?;
    let locked_parent: Uuid = sqlx::query_scalar(
        "SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE",
    )
    .bind(Uuid::from_u128(parent))
    .fetch_one(&mut *ancestor_lock)
    .await?;
    assert_eq!(locked_descendant, Uuid::from_u128(descendant));
    assert_eq!(locked_parent, Uuid::from_u128(parent));
    ancestor_lock.commit().await?;
    frontier.await??;

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: an applied descendant-scoped goal stop
/// atomically records every edge, logically terminalizes active and queued
/// bound children with exact provenance, and leaves the background child
/// runnable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_goal_stop_materializes_complete_delegation_cascade() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let parent = 0xf100;
    let bound_child = 0xf101;
    let background_child = 0xf102;
    let queued_bound_child = 0xf103;
    let bound_request = 0xf110;
    let background_request = 0xf120;
    let bound_turn = 0xf111;
    let background_turn = 0xf121;
    let queued_bound_request = 0xf310;
    let queued_bound_turn = 0xf311;
    let stop_command = 0xf130;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(0xf001, parent, 0xf201))
        .await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(0xf002, bound_child, 0xf202))
        .await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(0xf003, background_child, 0xf203))
        .await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(0xf004, queued_bound_child, 0xf204))
        .await?;
    insert_queued_delegation_fixture(
        &pool,
        DelegationFixture {
            spawning_request: bound_request,
            parent_session: parent,
            parent_turn: 0xf112,
            child_session: bound_child,
            child_turn: bound_turn,
            task_entry: 0xf113,
            selection: 0xf202,
            policy_kind: "bound",
            on_parent_stopped: Some("stop"),
            on_parent_cancelled: Some("cancel"),
        },
    )
    .await?;
    insert_queued_delegation_fixture(
        &pool,
        DelegationFixture {
            spawning_request: queued_bound_request,
            parent_session: parent,
            parent_turn: 0xf312,
            child_session: queued_bound_child,
            child_turn: queued_bound_turn,
            task_entry: 0xf313,
            selection: 0xf204,
            policy_kind: "bound",
            on_parent_stopped: Some("cancel"),
            on_parent_cancelled: Some("cancel"),
        },
    )
    .await?;
    insert_queued_delegation_fixture(
        &pool,
        DelegationFixture {
            spawning_request: background_request,
            parent_session: parent,
            parent_turn: 0xf122,
            child_session: background_child,
            child_turn: background_turn,
            task_entry: 0xf123,
            selection: 0xf203,
            policy_kind: "background",
            on_parent_stopped: None,
            on_parent_cancelled: None,
        },
    )
    .await?;

    let activation = StartEligibleTurnRepository::new(pool.clone())
        .handle(session(bound_child), activation_identities(0xf135))
        .await?;
    let StartEligibleTurnOutcome::Activated(_) = activation else {
        panic!("the bound child must activate before its parent stops");
    };
    record_empty_instruction_manifest(&pool, session(bound_child)).await?;
    let call = ModelCallId::from_uuid(Uuid::from_u128(0xf139));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        DirectModelSelection::from_uuid(Uuid::from_u128(0xf202)),
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(0xf138))),
    )])
    .expect("one bound-child target forms a catalog");
    let mut model_calls = PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        ModelCallCredentialReference::new("cascade-test-provider"),
    );
    let prepared = model_calls
        .prepare_initial_call(
            session(bound_child),
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xf13a)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xf13b)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xf13c)),
            |_| panic!("the bound child has no pending steering"),
        )
        .await?;
    let PrepareInitialModelCallOutcome::Checkpointed(checkpointed) = prepared else {
        panic!("the bound child's first model call must checkpoint");
    };
    assert_eq!(checkpointed, call);
    let AuthorizeModelCallOutcome::Authorized(authorized) = model_calls
        .authorize_send(session(bound_child), call)
        .await?
    else {
        panic!("the prepared child call must authorize before its parent stops");
    };

    let repository = GoalRepository::new(pool.clone());
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0xf129),
                    session(parent),
                    GoalUserAction::Attach(statement("stop the delegated descendants")),
                ),
                Some(turn_candidates(0xf131)),
                |_| None,
            )
            .await?,
    );
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(stop_command),
                    session(parent),
                    GoalUserAction::Stop {
                        descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                ),
                None,
                |_| None,
            )
            .await?,
    );

    let cascade: (String, String, i64) = sqlx::query_as(
        "SELECT root_source_kind, termination_kind, disposition_count::bigint
           FROM session_delegation_termination_cascade
          WHERE root_command_id = $1",
    )
    .bind(Uuid::from_u128(stop_command))
    .fetch_one(&pool)
    .await?;
    assert_eq!(cascade, ("goal_command".into(), "stopped".into(), 3));
    let bound_terminal: (Uuid, Uuid, String, Uuid) = sqlx::query_as(
        "SELECT child_session_id, child_turn_id, disposition_kind, root_command_id
           FROM session_delegation_logical_terminal
          WHERE spawning_tool_request_id = $1",
    )
    .bind(Uuid::from_u128(bound_request))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        bound_terminal,
        (
            Uuid::from_u128(bound_child),
            Uuid::from_u128(bound_turn),
            "stopped".into(),
            Uuid::from_u128(stop_command),
        )
    );
    let terminal_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(session(bound_child))
        .await?
        .expect("the bound child transcript remains readable");
    let terminal_turn = terminal_snapshot
        .turns()
        .first()
        .expect("the child transcript retains its delegated root turn");
    let ProcessTurnState::DelegationTerminated {
        spawning_request,
        outcome,
        reason,
        provenance,
    } = terminal_turn.state()
    else {
        panic!("the retained delegated turn must project its logical terminal state");
    };
    assert_eq!(*spawning_request, tool_request(bound_request));
    assert_eq!(*outcome, DispatchedDelegationOutcome::ChildStopped);
    assert_eq!(
        *reason,
        DispatchedDelegationReason::ParentStoppedWithDescendants
    );
    assert_eq!(
        *provenance,
        DispatchedDelegationProvenance::ParentGoalCommand {
            session: session(parent),
            goal_generation: 1,
            command: command(stop_command),
        }
    );
    let queued_terminal_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(session(queued_bound_child))
        .await?
        .expect("the queued bound child transcript remains readable");
    let queued_terminal_turn = queued_terminal_snapshot
        .turns()
        .first()
        .expect("the queued child transcript retains its delegated root turn");
    assert_eq!(
        queued_terminal_turn.state(),
        &ProcessTurnState::DelegationTerminated {
            spawning_request: tool_request(queued_bound_request),
            outcome: DispatchedDelegationOutcome::ChildCancelled,
            reason: DispatchedDelegationReason::ParentStoppedWithDescendants,
            provenance: DispatchedDelegationProvenance::ParentGoalCommand {
                session: session(parent),
                goal_generation: 1,
                command: command(stop_command),
            },
        }
    );
    let outcomes: Vec<(Uuid, String, String, String, Uuid)> = sqlx::query_as(
        "SELECT spawning_tool_request_id, outcome_kind, reason_kind,
                provenance_kind, provenance_command_id
           FROM session_delegation_event
          WHERE provenance_command_id = $1
          ORDER BY spawning_tool_request_id",
    )
    .bind(Uuid::from_u128(stop_command))
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        outcomes,
        [
            (
                Uuid::from_u128(bound_request),
                "child_stopped".into(),
                "parent_stopped_parent_and_descendants".into(),
                "parent_goal_command".into(),
                Uuid::from_u128(stop_command),
            ),
            (
                Uuid::from_u128(background_request),
                "continue_running".into(),
                "parent_stopped_parent_and_descendants".into(),
                "parent_goal_command".into(),
                Uuid::from_u128(stop_command),
            ),
            (
                Uuid::from_u128(queued_bound_request),
                "child_cancelled".into(),
                "parent_stopped_parent_and_descendants".into(),
                "parent_goal_command".into(),
                Uuid::from_u128(stop_command),
            ),
        ]
    );
    let delivered: CascadeDeliveryFacts = sqlx::query_as(
        "SELECT
            (SELECT count(*)::bigint FROM session_child_result
              WHERE spawning_tool_request_id = $1)
                AS bound_child_results,
            (SELECT count(*)::bigint FROM delegation_update_outbox_event
              WHERE provenance_command_id = $2
                AND update_kind = 'child_lifecycle_disposition'
                AND session_id = $3)
                AS parent_addressed_dispositions,
            (SELECT count(*)::bigint FROM delegation_update_outbox_event
                AS addressed
              WHERE addressed.provenance_command_id = $2
                AND addressed.update_kind = 'child_lifecycle_disposition'
                AND addressed.session_id = addressed.child_session_id)
                AS child_addressed_dispositions,
            (SELECT count(*)::bigint FROM delegation_update_outbox_event
              WHERE provenance_command_id = $2
                AND update_kind = 'child_result')
                AS child_result_updates,
            (SELECT count(*)::bigint FROM delegation_wake_outbox_event
              WHERE result_spawning_request_id = $1)
                AS bound_child_result_wakes",
    )
    .bind(Uuid::from_u128(bound_request))
    .bind(Uuid::from_u128(stop_command))
    .bind(Uuid::from_u128(parent))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        delivered,
        CascadeDeliveryFacts {
            bound_child_results: 1,
            parent_addressed_dispositions: 3,
            child_addressed_dispositions: 2,
            child_result_updates: 2,
            bound_child_result_wakes: 1,
        }
    );
    let terminalized_children: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
        "SELECT session_id, child_session_id, outcome_kind
           FROM delegation_update_outbox_event
          WHERE provenance_command_id = $1
            AND update_kind = 'child_lifecycle_disposition'
            AND session_id <> $2
          ORDER BY child_session_id",
    )
    .bind(Uuid::from_u128(stop_command))
    .bind(Uuid::from_u128(parent))
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        terminalized_children,
        [
            (
                Uuid::from_u128(bound_child),
                Uuid::from_u128(bound_child),
                "child_stopped".into(),
            ),
            (
                Uuid::from_u128(queued_bound_child),
                Uuid::from_u128(queued_bound_child),
                "child_cancelled".into(),
            ),
        ]
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        model_calls.cancellation_signal(session(bound_child), call),
    )
    .await
    .expect("the logical terminal proof must resolve the provider poll");
    let discarded = CommitModelCallObservationTransaction::commit_observation(
        &mut model_calls,
        session(bound_child),
        authorized
            .observation_correlation()
            .bind_terminal_observation(ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    AssistantText::try_new("late provider result".into())
                        .expect("fixture assistant text is valid"),
                ],
            }),
        ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Completed(
            CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    0xf13d,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xf13e)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xf13f)),
            ),
        )),
        |_| panic!("discarded provider content cannot reclassify steering"),
    )
    .await?;
    assert_eq!(discarded, None);
    assert!(
        StartEligibleTurnRepository::new(pool.clone())
            .preview(session(bound_child), activation_identities(0xf140),)
            .await?
            .is_none()
    );
    assert!(
        StartEligibleTurnRepository::new(pool.clone())
            .preview(session(background_child), activation_identities(0xf150),)
            .await?
            .is_some()
    );
    let restarted_input = SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            SubmitInput::new(
                command(0xf160),
                session(bound_child),
                UserContent::try_text(String::from("run independent work after the cascade"))
                    .expect("fixture input content is admitted"),
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xf161)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xf162))),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xf163)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xf164)),
            ),
            |_| panic!("the new input has no steering to reclassify"),
            |_| panic!("the new input has no tool batch to cancel"),
        )
        .await?;
    assert!(matches!(
        restarted_input,
        signalbox_persistence::submit_input::SubmitInputHandlingOutcome::Recorded(
            SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(_))
        )
    ));
    let restarted = StartEligibleTurnRepository::new(pool.clone())
        .handle(session(bound_child), activation_identities(0xf170))
        .await?;
    let StartEligibleTurnOutcome::Activated(restarted) = restarted else {
        panic!("the logical terminal proof must release the child session slot");
    };
    assert_ne!(
        restarted.turn(),
        TurnId::from_uuid(Uuid::from_u128(bound_turn))
    );
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0xf320),
                    session(queued_bound_child),
                    GoalUserAction::Attach(statement(
                        "run independent work after a queued cascade",
                    )),
                ),
                Some(turn_candidates(0xf321)),
                |_| None,
            )
            .await?,
    );
    let restarted_queued = StartEligibleTurnRepository::new(pool.clone())
        .handle(session(queued_bound_child), activation_identities(0xf330))
        .await?;
    let StartEligibleTurnOutcome::Activated(restarted_queued) = restarted_queued else {
        panic!("the queued logical terminal must release the child session slot");
    };
    assert_ne!(
        restarted_queued.turn(),
        TurnId::from_uuid(Uuid::from_u128(queued_bound_turn))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: a descendant-scoped lifecycle stop whose live
/// turn is closed by its core interrupt carries `stopped` into the cascade, so
/// a bound child follows `on_parent_stopped` rather than `on_parent_cancelled`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_lifecycle_stop_interrupt_uses_stopped_child_policy() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let parent = 0xf500;
    let child = 0xf501;
    let spawning_request = 0xf510;
    let child_turn = 0xf511;
    let lifecycle_command = 0xf520;
    let interrupt_command = 0xf521;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(0xf502, parent, 0xf503))
        .await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(0xf504, child, 0xf505))
        .await?;
    insert_queued_delegation_fixture(
        &pool,
        DelegationFixture {
            spawning_request,
            parent_session: parent,
            parent_turn: 0xf512,
            child_session: child,
            child_turn,
            task_entry: 0xf513,
            selection: 0xf505,
            policy_kind: "bound",
            on_parent_stopped: Some("stop"),
            on_parent_cancelled: Some("keep_running"),
        },
    )
    .await?;
    let candidates = turn_candidates(0xf530);
    assert_applied_command(
        GoalRepository::new(pool.clone())
            .handle_user_command(
                GoalUserCommand::new(
                    command(0xf531),
                    session(parent),
                    GoalUserAction::Attach(statement("exercise lifecycle stop cascade")),
                ),
                Some(candidates),
                |_| None,
            )
            .await?,
    );
    let active_turn = activated_turn(
        StartEligibleTurnRepository::new(pool.clone())
            .handle(session(parent), activation_identities(0xf540))
            .await?,
    );
    let stop = SessionLifecycleCommand::new(
        command(lifecycle_command),
        session(parent),
        SessionLifecycleOperation::Stop {
            sticky: StopStickiness::Sticky,
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
        },
    );
    let committed = SessionLifecycleCommandRepository::new(pool.clone())
        .handle(stop, CommandPrincipal::Operator)
        .await?;
    assert_eq!(
        committed,
        SessionLifecycleCommandHandlingOutcome::Recorded(SessionLifecycleCommandResult::Applied(
            SessionLifecycleApplication::ClosurePending {
                outcome: SessionTerminalOutcome::Stopped {
                    sticky: StopStickiness::Sticky,
                },
                live_turn: active_turn,
                defaults_version: SessionConfigurationDefaultsVersion::first(),
            },
        ))
    );

    let successor = TurnId::from_uuid(Uuid::from_u128(0xf550));
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates_alias_resolver_as(
            SubmitInput::new(
                command(interrupt_command),
                session(parent),
                UserContent::try_text(String::from("close the stopped parent"))
                    .expect("fixture input content is admitted"),
                DeliveryRequest::Interrupt {
                    expected_active_turn: active_turn,
                    descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
            ),
            CommandPrincipal::Core,
            ParentTerminationKind::Stopped,
            AcceptedInputId::from_uuid(Uuid::from_u128(0xf551)),
            Some(successor),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xf552)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xf553)),
            ),
            |_| successor,
            |_| panic!("the fixture has no tool batch to cancel"),
            || panic!("the fixture has no approval wait"),
            || panic!("the fixture has no approval wait"),
            |_| None,
        )
        .await?;

    let cascade: (String, String) = sqlx::query_as(
        "SELECT root_source_kind, termination_kind
           FROM session_delegation_termination_cascade
          WHERE root_command_id = $1",
    )
    .bind(Uuid::from_u128(interrupt_command))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        cascade,
        (String::from("turn_command"), String::from("stopped"))
    );
    let child_disposition: (String, String, String) = sqlx::query_as(
        "SELECT event.outcome_kind, event.reason_kind, terminal.disposition_kind
           FROM session_delegation_event AS event
           JOIN session_delegation_logical_terminal AS terminal
             ON terminal.spawning_tool_request_id = event.spawning_tool_request_id
            AND terminal.root_command_id = event.provenance_command_id
          WHERE event.spawning_tool_request_id = $1
            AND event.event_kind = 'outcome_recorded'",
    )
    .bind(Uuid::from_u128(spawning_request))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        child_disposition,
        (
            String::from("child_stopped"),
            String::from("parent_stopped_parent_and_descendants"),
            String::from("stopped"),
        )
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// One derived cascade edge: which relationship it dispositions, the immediate
/// parent kind that selected its action, and the causal source that supplied
/// that kind.
#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct NestedCascadeEdgeFacts {
    spawning_tool_request_id: Uuid,
    parent_session_id: Uuid,
    termination_kind: String,
    source_kind: String,
    source_spawning_tool_request_id: Option<Uuid>,
}

/// The recorded disposition one cascade edge published against its relationship.
#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct NestedCascadeOutcomeFacts {
    spawning_tool_request_id: Uuid,
    outcome_kind: String,
    reason_kind: String,
}

/// Every durable record a pruned edge must not have acquired.
#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct PrunedEdgeRecordCounts {
    authorities: i64,
    outcomes: i64,
    logical_terminals: i64,
}

/// S19: a descendant-scoped stop descends into a nested
/// relationship under its immediate parent's disposition, not under the root
/// command's kind.
///
/// The tree is two levels deep, which is what separates this behavior from the
/// direct-child case the sibling cascade test already covers. Under the root
/// stop, `bound_child` is *cancelled* by its own policy, so the nested child's
/// `on_parent_cancelled` selects the plan. The nested policy is chosen so that
/// reading the root kind instead would invert the answer: `nested_child` would
/// keep running rather than stop, and would carry
/// `parent_stopped_parent_and_descendants`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s19_nested_cascade_descends_under_immediate_parent_disposition()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let parent = 0xf600;
    let bound_child = 0xf601;
    let nested_child = 0xf602;
    let bound_request = 0xf610;
    let nested_request = 0xf620;
    let bound_turn = 0xf611;
    let nested_turn = 0xf621;
    let stop_command = 0xf650;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(0xf001, parent, 0xf201))
        .await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(0xf002, bound_child, 0xf202))
        .await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(0xf003, nested_child, 0xf203))
        .await?;
    // The root stop cancels this edge, so its own child is dispositioned as a
    // cancelled parent rather than a stopped one.
    insert_queued_delegation_fixture(
        &pool,
        DelegationFixture {
            spawning_request: bound_request,
            parent_session: parent,
            parent_turn: 0xf612,
            child_session: bound_child,
            child_turn: bound_turn,
            task_entry: 0xf613,
            selection: 0xf202,
            policy_kind: "bound",
            on_parent_stopped: Some("cancel"),
            on_parent_cancelled: Some("cancel"),
        },
    )
    .await?;
    insert_queued_delegation_fixture(
        &pool,
        DelegationFixture {
            spawning_request: nested_request,
            parent_session: bound_child,
            parent_turn: bound_turn,
            child_session: nested_child,
            child_turn: nested_turn,
            task_entry: 0xf623,
            selection: 0xf203,
            policy_kind: "bound",
            on_parent_stopped: Some("keep_running"),
            on_parent_cancelled: Some("stop"),
        },
    )
    .await?;

    assert_applied_command(
        GoalRepository::new(pool.clone())
            .handle_user_command(
                GoalUserCommand::new(
                    command(0xf660),
                    session(parent),
                    GoalUserAction::Attach(statement("stop the nested delegated descendants")),
                ),
                Some(turn_candidates(0xf661)),
                |_| None,
            )
            .await?,
    );
    assert_applied_command(
        GoalRepository::new(pool.clone())
            .handle_user_command(
                GoalUserCommand::new(
                    command(stop_command),
                    session(parent),
                    GoalUserAction::Stop {
                        descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                ),
                None,
                |_| None,
            )
            .await?,
    );

    let disposition_count: i64 = sqlx::query_scalar(
        "SELECT disposition_count::bigint
           FROM session_delegation_termination_cascade
          WHERE root_command_id = $1",
    )
    .bind(Uuid::from_u128(stop_command))
    .fetch_one(&pool)
    .await?;
    assert_eq!(disposition_count, 2);
    let edges: Vec<NestedCascadeEdgeFacts> = sqlx::query_as(
        "SELECT spawning_tool_request_id, parent_session_id, termination_kind,
                source_kind, source_spawning_tool_request_id
           FROM session_delegation_parent_termination
          WHERE root_command_id = $1
          ORDER BY spawning_tool_request_id",
    )
    .bind(Uuid::from_u128(stop_command))
    .fetch_all(&pool)
    .await?;
    let nested_edge = &edges[1];
    assert_eq!(
        nested_edge.termination_kind, "cancelled",
        "the nested edge takes its immediate parent's disposition, not the root stop"
    );
    assert_eq!(nested_edge.source_kind, "parent_disposition");
    assert_eq!(
        nested_edge.source_spawning_tool_request_id,
        Some(Uuid::from_u128(bound_request))
    );
    let outcomes: Vec<NestedCascadeOutcomeFacts> = sqlx::query_as(
        "SELECT spawning_tool_request_id, outcome_kind, reason_kind
           FROM session_delegation_event
          WHERE provenance_command_id = $1
          ORDER BY spawning_tool_request_id",
    )
    .bind(Uuid::from_u128(stop_command))
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        outcomes[1],
        NestedCascadeOutcomeFacts {
            spawning_tool_request_id: Uuid::from_u128(nested_request),
            outcome_kind: "child_stopped".into(),
            reason_kind: "parent_cancelled_parent_and_descendants".into(),
        }
    );

    expect![[r#"
        ┌──────────────────────────────────────┬──────────────────────────────────────┬──────────────────┬────────────────────┬──────────────────────────────────────┐
        │ spawning_tool_request_id             │ parent_session_id                    │ termination_kind │ source_kind        │ source_spawning_tool_request_id      │
        ├──────────────────────────────────────┼──────────────────────────────────────┼──────────────────┼────────────────────┼──────────────────────────────────────┤
        │ 00000000-0000-0000-0000-00000000f610 │ 00000000-0000-0000-0000-00000000f600 │ stopped          │ root               │ None                                 │
        │ 00000000-0000-0000-0000-00000000f620 │ 00000000-0000-0000-0000-00000000f601 │ cancelled        │ parent_disposition │ 00000000-0000-0000-0000-00000000f610 │
        └──────────────────────────────────────┴──────────────────────────────────────┴──────────────────┴────────────────────┴──────────────────────────────────────┘
    "#]]
    .assert_eq(&table(edges));
    expect![[r#"
        ┌──────────────────────────────────────┬─────────────────┬─────────────────────────────────────────┐
        │ spawning_tool_request_id             │ outcome_kind    │ reason_kind                             │
        ├──────────────────────────────────────┼─────────────────┼─────────────────────────────────────────┤
        │ 00000000-0000-0000-0000-00000000f610 │ child_cancelled │ parent_stopped_parent_and_descendants   │
        │ 00000000-0000-0000-0000-00000000f620 │ child_stopped   │ parent_cancelled_parent_and_descendants │
        └──────────────────────────────────────┴─────────────────┴─────────────────────────────────────────┘
    "#]]
    .assert_eq(&table(outcomes));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S19: a descendant-scoped stop stops descending below a
/// relationship that survives it, leaving that whole subtree runnable.
///
/// `background_child` keeps running under any parent termination, so the
/// frontier never reaches its own child. The pruned grandchild's policy would
/// stop it if the cascade did reach it, so its absence is a decision rather
/// than a coincidence: it acquires no authority row, no outcome, and no logical
/// terminal, it contributes nothing to the cascade's disposition count, and its
/// queued delegated turn stays eligible.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s19_nested_cascade_prunes_below_a_surviving_edge() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let parent = 0xf700;
    let background_child = 0xf701;
    let pruned_child = 0xf702;
    let background_request = 0xf710;
    let pruned_request = 0xf720;
    let background_turn = 0xf711;
    let pruned_turn = 0xf721;
    let stop_command = 0xf750;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(0xf001, parent, 0xf201))
        .await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(0xf002, background_child, 0xf202))
        .await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(0xf003, pruned_child, 0xf203))
        .await?;
    insert_queued_delegation_fixture(
        &pool,
        DelegationFixture {
            spawning_request: background_request,
            parent_session: parent,
            parent_turn: 0xf712,
            child_session: background_child,
            child_turn: background_turn,
            task_entry: 0xf713,
            selection: 0xf202,
            policy_kind: "background",
            on_parent_stopped: None,
            on_parent_cancelled: None,
        },
    )
    .await?;
    // This policy would stop the child if the cascade ever reached this edge.
    insert_queued_delegation_fixture(
        &pool,
        DelegationFixture {
            spawning_request: pruned_request,
            parent_session: background_child,
            parent_turn: background_turn,
            child_session: pruned_child,
            child_turn: pruned_turn,
            task_entry: 0xf723,
            selection: 0xf203,
            policy_kind: "bound",
            on_parent_stopped: Some("stop"),
            on_parent_cancelled: Some("stop"),
        },
    )
    .await?;

    assert_applied_command(
        GoalRepository::new(pool.clone())
            .handle_user_command(
                GoalUserCommand::new(
                    command(0xf760),
                    session(parent),
                    GoalUserAction::Attach(statement("stop below the surviving delegated edge")),
                ),
                Some(turn_candidates(0xf761)),
                |_| None,
            )
            .await?,
    );
    assert_applied_command(
        GoalRepository::new(pool.clone())
            .handle_user_command(
                GoalUserCommand::new(
                    command(stop_command),
                    session(parent),
                    GoalUserAction::Stop {
                        descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                ),
                None,
                |_| None,
            )
            .await?,
    );

    let disposition_count: i64 = sqlx::query_scalar(
        "SELECT disposition_count::bigint
           FROM session_delegation_termination_cascade
          WHERE root_command_id = $1",
    )
    .bind(Uuid::from_u128(stop_command))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        disposition_count, 1,
        "only the surviving direct edge is dispositioned"
    );
    let pruned_records: PrunedEdgeRecordCounts = sqlx::query_as(
        "SELECT
            (SELECT count(*)::bigint FROM session_delegation_parent_termination
              WHERE spawning_tool_request_id = $1) AS authorities,
            (SELECT count(*)::bigint FROM session_delegation_event
              WHERE spawning_tool_request_id = $1
                AND event_kind = 'outcome_recorded') AS outcomes,
            (SELECT count(*)::bigint FROM session_delegation_logical_terminal
              WHERE spawning_tool_request_id = $1) AS logical_terminals",
    )
    .bind(Uuid::from_u128(pruned_request))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        pruned_records,
        PrunedEdgeRecordCounts {
            authorities: 0,
            outcomes: 0,
            logical_terminals: 0,
        }
    );
    let runtime_terminal_sessions: Vec<Uuid> = sqlx::query_scalar(
        "SELECT session_id FROM turn_lifecycle
          WHERE delegation_runtime_terminal
          ORDER BY session_id",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(runtime_terminal_sessions, [] as [Uuid; 0]);
    assert!(
        StartEligibleTurnRepository::new(pool.clone())
            .preview(session(pruned_child), activation_identities(0xf770))
            .await?
            .is_some(),
        "a pruned grandchild keeps its queued delegated turn eligible"
    );

    let edges: Vec<NestedCascadeEdgeFacts> = sqlx::query_as(
        "SELECT spawning_tool_request_id, parent_session_id, termination_kind,
                source_kind, source_spawning_tool_request_id
           FROM session_delegation_parent_termination
          WHERE root_command_id = $1
          ORDER BY spawning_tool_request_id",
    )
    .bind(Uuid::from_u128(stop_command))
    .fetch_all(&pool)
    .await?;
    expect![[r#"
        ┌──────────────────────────────────────┬──────────────────────────────────────┬──────────────────┬─────────────┬─────────────────────────────────┐
        │ spawning_tool_request_id             │ parent_session_id                    │ termination_kind │ source_kind │ source_spawning_tool_request_id │
        ├──────────────────────────────────────┼──────────────────────────────────────┼──────────────────┼─────────────┼─────────────────────────────────┤
        │ 00000000-0000-0000-0000-00000000f710 │ 00000000-0000-0000-0000-00000000f700 │ stopped          │ root        │ None                            │
        └──────────────────────────────────────┴──────────────────────────────────────┴──────────────────┴─────────────┴─────────────────────────────────┘
    "#]]
    .assert_eq(&table(edges));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: a delegated turn that completes while holding
/// next-safe-point steering reclassifies that steering into a successor turn.
///
/// A delegated turn has no accepted-input queue origin, so reclassification
/// must resolve its configuration through the delegated origin rather than the
/// queue chain that an accepted-input turn walks.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_delegated_turn_reclassifies_its_pending_steering() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let parent = 0xfb00;
    let child = 0xfb01;
    let spawning_request = 0xfb10;
    let child_turn = 0xfb11;
    let steer_command = 0xfb40;
    let steered_input = 0xfb41;
    let successor_turn = 0xfb42;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(0xfb01, parent, 0xfb21))
        .await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(0xfb02, child, 0xfb22))
        .await?;
    insert_queued_delegation_fixture(
        &pool,
        DelegationFixture {
            spawning_request,
            parent_session: parent,
            parent_turn: 0xfb12,
            child_session: child,
            child_turn,
            task_entry: 0xfb13,
            selection: 0xfb22,
            policy_kind: "background",
            on_parent_stopped: None,
            on_parent_cancelled: None,
        },
    )
    .await?;
    let StartEligibleTurnOutcome::Activated(_) = StartEligibleTurnRepository::new(pool.clone())
        .handle(session(child), activation_identities(0xfb35))
        .await?
    else {
        panic!("the delegated child must activate before it is steered");
    };
    record_empty_instruction_manifest(&pool, session(child)).await?;
    let call = ModelCallId::from_uuid(Uuid::from_u128(0xfb50));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        DirectModelSelection::from_uuid(Uuid::from_u128(0xfb22)),
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(0xfb51))),
    )])
    .expect("one delegated-child target forms a catalog");
    let mut model_calls = PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        ModelCallCredentialReference::new("delegated-steering-test-provider"),
    );
    let PrepareInitialModelCallOutcome::Checkpointed(checkpointed) = model_calls
        .prepare_initial_call(
            session(child),
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xfb52)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xfb53)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xfb54)),
            |_| panic!("the delegated child holds no steering before its call is authorized"),
        )
        .await?
    else {
        panic!("the delegated child's first model call must checkpoint");
    };
    assert_eq!(checkpointed, call);

    let (eligible, _dispatch_starts, continuation) = PostgresEligibilitySweep::new(pool.clone())
        .find_sessions()
        .await?
        .into_parts();
    assert!(eligible.contains(&session(child)));
    assert!(!continuation);

    let AuthorizeModelCallOutcome::Authorized(authorized) =
        model_calls.authorize_send(session(child), call).await?
    else {
        panic!("the prepared delegated call must authorize");
    };

    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            SubmitInput::new(
                command(steer_command),
                session(child),
                UserContent::try_text(String::from("steer the delegated child"))
                    .expect("fixture steering content is admitted"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: TurnId::from_uuid(Uuid::from_u128(child_turn)),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(steered_input)),
            None,
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xfb43)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xfb44)),
            ),
            |_| panic!("steering cannot be reclassified while its source remains active"),
            |_| panic!("the delegated child has no tool request to cancel"),
        )
        .await?;

    let committed = CommitModelCallObservationTransaction::commit_observation(
        &mut model_calls,
        session(child),
        authorized
            .observation_correlation()
            .bind_terminal_observation(ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    AssistantText::try_new("delegated answer".into())
                        .expect("fixture assistant text is valid"),
                ],
            }),
        ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Completed(
            CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    0xfb56,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xfb57)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xfb58)),
            ),
        )),
        |_| TurnId::from_uuid(Uuid::from_u128(successor_turn)),
    )
    .await?;
    assert!(committed.is_some());

    let reclassified: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT disposition_kind, origin_turn_id
           FROM accepted_input
          WHERE accepted_input_id = $1",
    )
    .bind(Uuid::from_u128(steered_input))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        reclassified,
        (
            "reclassified_as_turn_origin".into(),
            Some(Uuid::from_u128(successor_turn)),
        )
    );
    let successor_state: String = sqlx::query_scalar(
        "SELECT state_kind FROM turn_lifecycle WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(Uuid::from_u128(child))
    .bind(Uuid::from_u128(successor_turn))
    .fetch_one(&pool)
    .await?;
    assert_eq!(successor_state, "queued");

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: a cascade-terminalized child releases its
/// compaction boundary. The retained delegated turn stays physically active, so
/// preparation must read runtime relevance rather than the physical state and
/// must source the logical terminal's frontier.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_logically_terminal_child_admits_compaction() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let parent = 0xfa00;
    let bound_child = 0xfa01;
    let bound_request = 0xfa10;
    let bound_turn = 0xfa11;
    let stop_command = 0xfa30;
    let compaction_command = 0xfa40;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(0xfa01, parent, 0xfa21))
        .await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_fixture(0xfa02, bound_child, 0xfa22))
        .await?;
    insert_queued_delegation_fixture(
        &pool,
        DelegationFixture {
            spawning_request: bound_request,
            parent_session: parent,
            parent_turn: 0xfa12,
            child_session: bound_child,
            child_turn: bound_turn,
            task_entry: 0xfa13,
            selection: 0xfa22,
            policy_kind: "bound",
            on_parent_stopped: Some("stop"),
            on_parent_cancelled: Some("cancel"),
        },
    )
    .await?;
    let StartEligibleTurnOutcome::Activated(_) = StartEligibleTurnRepository::new(pool.clone())
        .handle(session(bound_child), activation_identities(0xfa35))
        .await?
    else {
        panic!("the bound child must activate before its parent stops");
    };

    let repository = GoalRepository::new(pool.clone());
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0xfa29),
                    session(parent),
                    GoalUserAction::Attach(statement("stop the delegated descendants")),
                ),
                Some(turn_candidates(0xfa31)),
                |_| None,
            )
            .await?,
    );
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(stop_command),
                    session(parent),
                    GoalUserAction::Stop {
                        descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                ),
                None,
                |_| None,
            )
            .await?,
    );
    let retained: (String, bool) = sqlx::query_as(
        "SELECT state_kind, delegation_runtime_terminal
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(Uuid::from_u128(bound_child))
    .bind(Uuid::from_u128(bound_turn))
    .fetch_one(&pool)
    .await?;
    assert_eq!(retained, ("active".into(), true));

    let outcome = ContextCompactionRepository::new(pool.clone())
        .prepare(PrepareContextCompactionRequest {
            command: command(compaction_command),
            session: session(bound_child),
            requested_through_position: None,
            automatic_for_turn: None,
            defaults_version: SessionConfigurationDefaultsVersion::first(),
            selection: DirectModelSelection::from_uuid(Uuid::from_u128(0xfa22)),
            target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                Uuid::from_u128(0xfa41),
            )),
            input_includes_cache_tokens: false,
            credential_reference: String::from("cascade-compaction-test-provider"),
            call: ModelCallId::from_uuid(Uuid::from_u128(0xfa42)),
            compaction: ContextCompactionId::from_uuid(Uuid::from_u128(0xfa43)),
            summary_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xfa44)),
            result_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(0xfa45)),
        })
        .await?;
    let PrepareContextCompactionOutcome::Prepared(prepared) = outcome else {
        panic!("the logical terminal must release the child's compaction boundary");
    };
    let logical_terminal_frontier: Uuid = sqlx::query_scalar(
        "SELECT terminal_frontier_id
           FROM session_delegation_logical_terminal
          WHERE child_session_id = $1",
    )
    .bind(Uuid::from_u128(bound_child))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        prepared.source_frontier(),
        ContextFrontierId::from_uuid(logical_terminal_frontier)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// an applied stop waits on the scheduler row before recording its
/// terminal event, so queued activation cannot cross the user receipt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stop_waits_for_scheduler_lock() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0x9a1),
                    session(SESSION),
                    GoalUserAction::Attach(statement("stop behind the scheduler lock")),
                ),
                Some(turn_candidates(0xba1)),
                |_| None,
            )
            .await?,
    );

    let mut scheduler_blocker = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
        .bind(Uuid::from_u128(SESSION))
        .execute(&mut *scheduler_blocker)
        .await?;
    let stop = tokio::spawn({
        let repository = GoalRepository::new(pool.clone());
        async move {
            repository
                .handle_user_command(
                    GoalUserCommand::new(
                        command(0x9a2),
                        session(SESSION),
                        GoalUserAction::Stop {
                            descendant_scope: DescendantTerminationScope::ParentAlone,
                        },
                    ),
                    None,
                    |_| None,
                )
                .await
        }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "goal stop must wait on the held scheduler row"
    );
    let before_release = repository
        .load_goal(session(SESSION))
        .await?
        .expect("the attached goal remains visible");
    assert_eq!(before_release.current().state(), &GoalState::Pursuing);

    scheduler_blocker.rollback().await?;
    assert_applied_command(stop.await??);
    let after_release = repository
        .load_goal(session(SESSION))
        .await?
        .expect("the stopped goal remains visible");
    assert_eq!(after_release.current().state(), &GoalState::UserStopped);

    pool.close().await;
    drop(container);
    Ok(())
}

/// Attaches a goal whose first turn is active and records the model's
/// `goal_declare` request on it.
async fn attach_and_declare(
    pool: &PgPool,
    command: u128,
    candidates: u128,
    request: u128,
    activate: bool,
) -> Result<(TurnId, GoalModelProvenance), Box<dyn Error>> {
    let repository = GoalRepository::new(pool.clone());
    let attached_turn = turn_candidates(candidates);
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    signalbox_domain::DurableCommandId::from_uuid(Uuid::from_u128(command)),
                    session(SESSION),
                    GoalUserAction::Attach(statement("finish the fixture work")),
                ),
                Some(attached_turn),
                |_| None,
            )
            .await?,
    );
    let turn = attached_turn.turn();
    if activate {
        assert_eq!(activate_goal_turn(pool, candidates + 0x10).await?, turn);
    }
    let declaration_request = tool_request(request);
    insert_goal_tool_request(
        pool,
        turn,
        declaration_request,
        "goal_declare",
        r#"{"transition":"achieved"}"#,
        "the fixture work is finished",
    )
    .await?;
    Ok((turn, GoalModelProvenance::new(turn, declaration_request)))
}

/// A failing finish check appends no achievement, keeps
/// the goal pursuing, and leaves its detail for the failure that follows.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_failing_finish_check_blocks_the_goal_with_its_result() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let (_, provenance) = attach_and_declare(&pool, 0x9c1, 0xbc1, 0xfc1, true).await?;

    let outcome = repository
        .declare_achieved(
            session(SESSION),
            GoalReport::try_new(String::from("the fixture work is finished"))?,
            provenance,
            FinishCheckVerdict::Failed {
                detail: String::from("two review threads are unresolved"),
            },
        )
        .await?;

    assert_applied_transition(outcome);
    let goal = repository
        .load_goal(session(SESSION))
        .await?
        .expect("the goal stays attached");
    assert_eq!(
        *goal.current().state(),
        GoalState::Blocked {
            reason: signalbox_domain::GoalBlockedReasonKind::FinishCheckFailed,
            need: GoalNeed::try_new(String::from("two review threads are unresolved"))?,
        }
    );
    assert_eq!(
        goal.events().len(),
        2,
        "the failing check appended its block"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A passing finish check appends the achievement and
/// commits `achieved_verified` to the handoff; the settlement that retires the
/// generation's queued turn records the session terminal.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_passing_finish_check_settles_achieved_verified() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let lifecycle = SessionLifecycleRepository::new(pool.clone());
    let (_, provenance) = attach_and_declare(&pool, 0x9c2, 0xbc2, 0xfc2, false).await?;

    assert_applied_transition(
        repository
            .declare_achieved(
                session(SESSION),
                GoalReport::try_new(String::from("the fixture work is finished"))?,
                provenance,
                FinishCheckVerdict::Passed,
            )
            .await?,
    );
    let settled = lifecycle
        .load(session(SESSION))
        .await?
        .expect("the session keeps its lifecycle row");

    assert_eq!(
        settled.state(),
        SessionLifecycleState::Terminal {
            outcome: SessionTerminalOutcome::AchievedVerified,
        }
    );
    assert_eq!(settled.pending_terminal(), None);

    pool.close().await;
    drop(container);
    Ok(())
}

/// An achievement no finish check verifies closes the session `achieved_declared`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_declared_achievement_settles_achieved_declared() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let lifecycle = SessionLifecycleRepository::new(pool.clone());
    let (_, provenance) = attach_and_declare(&pool, 0x9c4, 0xbc4, 0xfc4, false).await?;

    assert_applied_transition(
        repository
            .declare_achieved(
                session(SESSION),
                GoalReport::try_new(String::from("the fixture work is finished"))?,
                provenance,
                FinishCheckVerdict::Unverified,
            )
            .await?,
    );
    let settled = lifecycle
        .load(session(SESSION))
        .await?
        .expect("the session keeps its lifecycle row");

    assert_eq!(
        settled.state(),
        SessionLifecycleState::Terminal {
            outcome: SessionTerminalOutcome::AchievedDeclared,
        }
    );
    assert_eq!(settled.pending_terminal(), None);

    pool.close().await;
    drop(container);
    Ok(())
}
