#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{collections::VecDeque, error::Error, future::Future, sync::Arc};

use rust_decimal::Decimal;
use signalbox_application::{
    AuthorizeModelCallOutcome, CommitModelCallObservationTransaction, CreateSessionError,
    CreateSessionOutcome, CreateSessionRequest, CreateSessionService, EligibilityNudge,
    EligibilityNudgeOutcome, EligibilitySweep, InProcessAttemptDispatchGate, LoadSessionService,
    ModelCallAuthorizationReread, ModelCallCredentialReference, ModelCallExecutionIdGenerator,
    ModelCallExecutionOutcome, ModelCallExecutionService, ReplaceSessionDefaultsOutcome,
    ReplaceSessionDefaultsRequest, ReplaceSessionDefaultsService, RetainedCapabilityFailureStatus,
    RetainedModelCallObservationStatus, ScriptedModelCallProvider, ScriptedModelCallStep,
    SessionIdGenerator, StartEligibleTurnIdGenerator, StartEligibleTurnOutcome,
    StartEligibleTurnService, StartupScanIdGenerator, StartupScanService,
    StartupScanSessionOutcome, SubmitInputIdGenerator, SubmitInputOutcome, SubmitInputRequest,
    SubmitInputRequestError, SubmitInputService,
};
use signalbox_domain::{
    AcceptedInputId, AcceptedInputStartingLineage, ActivatedAcceptedInputTurn, ActiveTurnPhase,
    AssistantText, AuthorizedModelCall, CompletedModelCallIdentities, ContextFrontierId,
    CreateSession, CurrentTurnAttemptState, DeliveryRequest, DurableCommandId,
    FailedModelCallTurnIdentities, ModelAlias, ModelCallId, ModelCallTerminalIdentities,
    ModelCallTerminalObservation, ModelCallTerminalOutcome, ModelSelectionOverride,
    ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition, PerInputConfigurationChoices,
    PreparedCreateSession, ProviderModelIdentity, RefusedModelCallTurnIdentities,
    ReplaceSessionDefaults, ReplaceSessionDefaultsRejectedResult, ReplaceSessionDefaultsResult,
    ResolvedProviderTarget, SemanticTranscriptEntryId, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    SessionId, SubmitInput, SubmitInputAppliedResult, SubmitInputReconstitutionFailure,
    SubmitInputRejectedResult, SubmitInputResult, TranscriptAncestry, TurnAttemptId,
    TurnConfigurationProvenance, TurnId, UserContent,
};
use signalbox_persistence::{
    MIGRATOR,
    create_session::{
        CreateSessionCorruption, CreateSessionHandlingOutcome, CreateSessionRepository,
        CreateSessionRepositoryError,
    },
    local_test_connection_options, migrate,
    model_execution::{
        ModelCallRepositoryError, PostgresModelCallRepository, PrepareInitialModelCallOutcome,
    },
    replace_session_defaults::{
        ReplaceSessionDefaultsCorruption, ReplaceSessionDefaultsHandlingOutcome,
        ReplaceSessionDefaultsRepository, ReplaceSessionDefaultsRepositoryError,
    },
    scheduler::PostgresEligibilitySweep,
    session::{SessionCorruption, SessionRepository, SessionRepositoryError},
    start_eligible_turn::{
        StartEligibleTurnCorruption, StartEligibleTurnIdentityCollision,
        StartEligibleTurnRepository, StartEligibleTurnRepositoryError,
    },
    startup::PostgresStartupScanRepository,
    submit_input::{
        SubmitInputCorruption, SubmitInputHandlingOutcome, SubmitInputRepository,
        SubmitInputRepositoryError,
    },
};
use sqlx::{PgConnection, PgPool, migrate::Migrate, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_integration";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

fn model_credential_reference() -> ModelCallCredentialReference {
    ModelCallCredentialReference::new("fixture-provider-primary")
}

#[derive(Clone, Copy, Debug, Default)]
struct AcceptingEligibilityNudge;

impl EligibilityNudge for AcceptingEligibilityNudge {
    fn nudge(&self, _session: SessionId) -> EligibilityNudgeOutcome {
        EligibilityNudgeOutcome::Enqueued
    }
}

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool, String), Box<dyn Error>> {
    let (container, pool, database_url) = unmigrated_postgres().await?;

    migrate(&pool).await?;

    Ok((container, pool, database_url))
}

async fn unmigrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool, String), Box<dyn Error>>
{
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

    Ok((container, pool, database_url))
}

async fn insert_origin_frontier(
    connection: &mut PgConnection,
    session: Uuid,
    accepted_input: Uuid,
    semantic_entry: Uuid,
    frontier: Uuid,
    declared_member_count: Decimal,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'origin_accepted_input', $3, NULL)",
    )
    .bind(session)
    .bind(semantic_entry)
    .bind(accepted_input)
    .execute(&mut *connection)
    .await?;

    insert_frontier(
        connection,
        session,
        frontier,
        declared_member_count,
        &[(Decimal::ONE, session, semantic_entry)],
    )
    .await
}

async fn insert_frontier(
    connection: &mut PgConnection,
    owning_session: Uuid,
    frontier: Uuid,
    member_count: Decimal,
    members: &[(Decimal, Uuid, Uuid)],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, $3)",
    )
    .bind(owning_session)
    .bind(frontier)
    .bind(member_count)
    .execute(&mut *connection)
    .await?;

    for (member_position, source_session, semantic_entry) in members {
        sqlx::query(
            "INSERT INTO context_frontier_member
                (owning_session_id, context_frontier_id, member_position,
                 source_session_id, semantic_entry_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(owning_session)
        .bind(frontier)
        .bind(member_position)
        .bind(source_session)
        .bind(semantic_entry)
        .execute(&mut *connection)
        .await?;
    }

    Ok(())
}

/// The session and pinned fresh identities for one production activation,
/// named so each call site states which identity it supplies.
struct EarliestQueuedTurnActivation {
    session: Uuid,
    origin_entry: Uuid,
    starting_frontier: Uuid,
    initial_attempt: Uuid,
}

/// Activates the session's earliest queued turn through the production
/// `StartEligibleTurnService`/`StartEligibleTurnRepository` chain with the
/// supplied fresh identities and returns the activated turn, so occupied-slot
/// tests exercise the exact scheduler-locked active shape the production
/// activation commits and assert its bound origin at their own call sites.
async fn activate_earliest_queued_turn(
    pool: &PgPool,
    activation: EarliestQueuedTurnActivation,
) -> Result<Box<ActivatedAcceptedInputTurn>, Box<dyn Error>> {
    let mut service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(
                activation.origin_entry,
            )],
            [ContextFrontierId::from_uuid(activation.starting_frontier)],
            [TurnAttemptId::from_uuid(activation.initial_attempt)],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) = service
        .execute(SessionId::from_uuid(activation.session))
        .await?
    else {
        panic!("the earliest queued origin must activate through the production service");
    };
    Ok(activated)
}

async fn run_mixed_occupied_acceptances(
    repository: SubmitInputRepository,
) -> Result<(Vec<u64>, u64, u64), Box<dyn Error>> {
    let mut tasks = Vec::new();
    for offset in 0..6_u128 {
        let repository = repository.clone();
        tasks.push(tokio::spawn(async move {
            let delivery = if offset % 2 == 0 {
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa51)),
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                }
            } else {
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa51)),
                }
            };
            repository
                .handle(
                    input_with_delivery(
                        0x453 + offset,
                        0x851,
                        &format!("mixed occupied {offset}"),
                        delivery,
                    ),
                    AcceptedInputId::from_uuid(Uuid::from_u128(0x952 + offset)),
                    (offset % 2 == 0).then(|| TurnId::from_uuid(Uuid::from_u128(0xa52 + offset))),
                )
                .await
        }));
    }

    let mut positions = Vec::new();
    let mut turn_origins = 0_u64;
    let mut pending_steering = 0_u64;
    for task in tasks {
        let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(applied)) =
            task.await??
        else {
            panic!("each mixed occupied-slot submission must apply");
        };
        positions.push(applied.acceptance_position().as_u64());
        match applied {
            SubmitInputAppliedResult::TurnOrigin(_) => turn_origins += 1,
            SubmitInputAppliedResult::PendingSteering(_) => pending_steering += 1,
        }
    }
    positions.sort_unstable();
    Ok((positions, turn_origins, pending_steering))
}

async fn record_stale_active_input(
    repository: &SubmitInputRepository,
    command_value: u128,
    delivery: DeliveryRequest,
    accepted_input: u128,
    turn: Option<u128>,
) -> Result<(SubmitInput, SubmitInputHandlingOutcome), SubmitInputRepositoryError> {
    let command = input_with_delivery(command_value, 0x841, "stale active", delivery);
    let outcome = repository
        .handle(
            command.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(accepted_input)),
            turn.map(|value| TurnId::from_uuid(Uuid::from_u128(value))),
        )
        .await?;
    Ok((command, outcome))
}

async fn active_origin_collision(
    repository: &SubmitInputRepository,
    pool: &PgPool,
    command_value: u128,
    delivery: DeliveryRequest,
    turn: Option<u128>,
) -> Result<(SubmitInputRepositoryError, i64), Box<dyn Error>> {
    let command = input_with_delivery(command_value, 0x841, "colliding active origin", delivery);
    let error = repository
        .handle(
            command,
            AcceptedInputId::from_uuid(Uuid::from_u128(0x941)),
            turn.map(|value| TurnId::from_uuid(Uuid::from_u128(value))),
        )
        .await
        .expect_err("new acceptance cannot reuse the active origin identity");
    let claimed = sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
        .bind(Uuid::from_u128(command_value))
        .fetch_one(pool)
        .await?;
    Ok((error, claimed))
}

fn prepared(
    command: u128,
    session: u128,
    selection: ModelSelectionRequest,
) -> PreparedCreateSession {
    CreateSession::new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::None,
        ),
        SessionConfigurationDefaults::new(selection),
    )
    .prepare(SessionId::from_uuid(Uuid::from_u128(session)))
    .expect("owner-initiated creation without ancestry is preparable")
}

async fn append_session_created_test_event(
    connection: &mut PgConnection,
    session: Uuid,
) -> Result<Decimal, sqlx::Error> {
    let sequence = sqlx::query_scalar(
        "INSERT INTO outbox_event
            (event_kind, storage_version, session_id)
         VALUES ('session_created', 1, $1)
         RETURNING event_sequence",
    )
    .bind(session)
    .fetch_one(&mut *connection)
    .await?;

    sqlx::query(
        "INSERT INTO session_created_outbox_event
            (event_sequence, event_kind, storage_version, session_id)
         VALUES ($1, 'session_created', 1, $2)",
    )
    .bind(sequence)
    .bind(session)
    .execute(&mut *connection)
    .await?;

    Ok(sequence)
}

async fn assert_outbox_truncate_rejected(
    pool: &PgPool,
    statement: &'static str,
) -> Result<(), Box<dyn Error>> {
    let error = sqlx::query(statement)
        .execute(pool)
        .await
        .expect_err("outbox storage is not removable through truncate");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.code())
            .as_deref(),
        Some("23514")
    );
    Ok(())
}

/// Inserts the complete pre-outbox session record family for allocator tests.
///
/// The command and model identities derive from the one session seed so the
/// fixture states only the session identity those tests observe.
async fn insert_outbox_session_fixture(
    pool: &PgPool,
    session_seed: u128,
) -> Result<Uuid, sqlx::Error> {
    let session = Uuid::from_u128(session_seed);
    let command = Uuid::from_u128(session_seed ^ 0x1000);
    let model = Uuid::from_u128(session_seed ^ 0x2000);
    let mut transaction = pool.begin().await?;

    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'create_session', 1, transaction_timestamp())",
    )
    .bind(command)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES ($1, 'owner_initiated', 'none')",
    )
    .bind(session)
    .execute(&mut *transaction)
    .await?;
    sqlx::query("INSERT INTO session_scheduler (session_id) VALUES ($1)")
        .bind(session)
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, 1, 'direct', $2, NULL)",
    )
    .bind(session)
    .bind(model)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ($1, 1)",
    )
    .bind(session)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO create_session_command
            (command_id, command_kind, storage_version,
             creation_cause, ancestry_kind, initial_defaults_version,
             model_selection_kind, direct_model_selection_id, model_alias_id,
             result_kind, created_session_id)
         VALUES (
            $1, 'create_session', 1,
            'owner_initiated', 'none', 1,
            'direct', $2, NULL,
            'applied', $3
         )",
    )
    .bind(command)
    .bind(model)
    .bind(session)
    .execute(&mut *transaction)
    .await?;

    transaction.commit().await?;
    Ok(session)
}

fn direct(value: u128) -> ModelSelectionRequest {
    ModelSelectionRequest::Direct(signalbox_domain::DirectModelSelection::from_uuid(
        Uuid::from_u128(value),
    ))
}

fn alias(value: u128) -> ModelSelectionRequest {
    ModelSelectionRequest::Alias(ModelAlias::from_uuid(Uuid::from_u128(value)))
}

fn replacement(
    command: u128,
    session: u128,
    expected: u64,
    selection: ModelSelectionRequest,
) -> ReplaceSessionDefaults {
    ReplaceSessionDefaults::new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        SessionId::from_uuid(Uuid::from_u128(session)),
        SessionConfigurationDefaultsVersion::try_from_u64(expected)
            .expect("test versions are positive"),
        SessionConfigurationDefaults::new(selection),
    )
}

fn replacement_request(
    command: u128,
    session: u128,
    expected: u64,
    selection: ModelSelectionRequest,
) -> ReplaceSessionDefaultsRequest {
    ReplaceSessionDefaultsRequest::try_new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        SessionId::from_uuid(Uuid::from_u128(session)),
        SessionConfigurationDefaultsVersion::try_from_u64(expected)
            .expect("test versions are positive"),
        SessionConfigurationDefaults::new(selection),
    )
    .expect("ordinary test command identities are admitted")
}

fn input_choices(expected: u64, model: ModelSelectionOverride) -> PerInputConfigurationChoices {
    PerInputConfigurationChoices::new(
        SessionConfigurationDefaultsVersion::try_from_u64(expected)
            .expect("test versions are positive"),
        model,
    )
}

fn start_input(
    command: u128,
    session: u128,
    content: &str,
    expected: u64,
    model: ModelSelectionOverride,
) -> SubmitInput {
    SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        SessionId::from_uuid(Uuid::from_u128(session)),
        UserContent::try_text(content.to_owned()).expect("test content is admitted"),
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: input_choices(expected, model),
        },
    )
}

fn input_with_delivery(
    command: u128,
    session: u128,
    content: &str,
    delivery: DeliveryRequest,
) -> SubmitInput {
    SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        SessionId::from_uuid(Uuid::from_u128(session)),
        UserContent::try_text(content.to_owned()).expect("test content is admitted"),
        delivery,
    )
}

#[allow(clippy::too_many_arguments)]
async fn insert_malformed_submit_rejection(
    pool: &PgPool,
    command_id: Uuid,
    source_command_id: Uuid,
    rejection_kind: &str,
    result_expected_active_turn: Option<Uuid>,
    result_expected_defaults: Option<Decimal>,
    result_current_defaults: Option<Decimal>,
    result_unknown_alias: Option<Uuid>,
    result_selected_defaults: Option<Decimal>,
    result_last_position: Option<Decimal>,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'submit_input', 1, transaction_timestamp())",
    )
    .bind(command_id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position)
         SELECT
             $1, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             'rejected', $3, result_session_id,
             NULL, NULL, $4, $5, $6, $7, $8, $9
           FROM submit_input_command
          WHERE command_id = $2",
    )
    .bind(command_id)
    .bind(source_command_id)
    .bind(rejection_kind)
    .bind(result_expected_active_turn)
    .bind(result_expected_defaults)
    .bind(result_current_defaults)
    .bind(result_unknown_alias)
    .bind(result_selected_defaults)
    .bind(result_last_position)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

async fn insert_cross_wired_occupied_rejection(
    pool: &PgPool,
    command_id: Uuid,
    source_command_id: Uuid,
    expected_active_turn_id: Uuid,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'submit_input', 1, transaction_timestamp())",
    )
    .bind(command_id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_actual_active_turn_id, result_expected_active_turn_id,
             result_expected_defaults_version, result_current_defaults_version,
             result_unknown_alias_id, result_selected_defaults_version,
             result_last_position)
         SELECT
             $1, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text, delivery_kind,
             $3, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_actual_active_turn_id, result_expected_active_turn_id,
             result_expected_defaults_version, result_current_defaults_version,
             result_unknown_alias_id, result_selected_defaults_version,
             result_last_position
           FROM submit_input_command
          WHERE command_id = $2",
    )
    .bind(command_id)
    .bind(source_command_id)
    .bind(expected_active_turn_id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

#[derive(Debug)]
struct FixedSessionIds {
    remaining: VecDeque<SessionId>,
}

impl FixedSessionIds {
    fn new(values: impl IntoIterator<Item = SessionId>) -> Self {
        Self {
            remaining: values.into_iter().collect(),
        }
    }
}

impl SessionIdGenerator for FixedSessionIds {
    fn next_session_id(&mut self) -> SessionId {
        self.remaining
            .pop_front()
            .expect("the integration test supplies one identity per invocation")
    }
}

#[derive(Debug)]
struct FixedSubmitInputIds {
    accepted_inputs: VecDeque<AcceptedInputId>,
    turns: VecDeque<TurnId>,
}

impl FixedSubmitInputIds {
    fn new(
        accepted_inputs: impl IntoIterator<Item = AcceptedInputId>,
        turns: impl IntoIterator<Item = TurnId>,
    ) -> Self {
        Self {
            accepted_inputs: accepted_inputs.into_iter().collect(),
            turns: turns.into_iter().collect(),
        }
    }
}

impl SubmitInputIdGenerator for FixedSubmitInputIds {
    fn next_accepted_input_id(&mut self) -> AcceptedInputId {
        self.accepted_inputs
            .pop_front()
            .expect("the integration test supplies one accepted-input candidate per invocation")
    }

    fn next_turn_id(&mut self) -> TurnId {
        self.turns
            .pop_front()
            .expect("the integration test supplies one turn candidate per invocation")
    }
}

#[derive(Debug)]
struct FixedStartEligibleTurnIds {
    origin_entries: VecDeque<SemanticTranscriptEntryId>,
    starting_frontiers: VecDeque<ContextFrontierId>,
    initial_attempts: VecDeque<TurnAttemptId>,
}

impl FixedStartEligibleTurnIds {
    fn new(
        origin_entries: impl IntoIterator<Item = SemanticTranscriptEntryId>,
        starting_frontiers: impl IntoIterator<Item = ContextFrontierId>,
        initial_attempts: impl IntoIterator<Item = TurnAttemptId>,
    ) -> Self {
        Self {
            origin_entries: origin_entries.into_iter().collect(),
            starting_frontiers: starting_frontiers.into_iter().collect(),
            initial_attempts: initial_attempts.into_iter().collect(),
        }
    }
}

impl StartEligibleTurnIdGenerator for FixedStartEligibleTurnIds {
    fn next_origin_entry_id(&mut self) -> SemanticTranscriptEntryId {
        self.origin_entries
            .pop_front()
            .expect("the integration test supplies one origin-entry candidate per pass")
    }

    fn next_starting_frontier_id(&mut self) -> ContextFrontierId {
        self.starting_frontiers
            .pop_front()
            .expect("the integration test supplies one starting-frontier candidate per pass")
    }

    fn next_initial_attempt_id(&mut self) -> TurnAttemptId {
        self.initial_attempts
            .pop_front()
            .expect("the integration test supplies one initial-attempt candidate per pass")
    }
}

#[derive(Debug)]
struct FixedStartupScanIds {
    failure_entries: VecDeque<SemanticTranscriptEntryId>,
    terminal_frontiers: VecDeque<ContextFrontierId>,
    reclassified_turns: VecDeque<TurnId>,
}

#[derive(Debug)]
struct FixedModelCallExecutionIds {
    calls: VecDeque<ModelCallId>,
    entries: VecDeque<SemanticTranscriptEntryId>,
    frontiers: VecDeque<ContextFrontierId>,
    turns: VecDeque<TurnId>,
}

impl FixedModelCallExecutionIds {
    fn new(
        calls: impl IntoIterator<Item = ModelCallId>,
        entries: impl IntoIterator<Item = SemanticTranscriptEntryId>,
        frontiers: impl IntoIterator<Item = ContextFrontierId>,
        turns: impl IntoIterator<Item = TurnId>,
    ) -> Self {
        Self {
            calls: calls.into_iter().collect(),
            entries: entries.into_iter().collect(),
            frontiers: frontiers.into_iter().collect(),
            turns: turns.into_iter().collect(),
        }
    }
}

impl ModelCallExecutionIdGenerator for FixedModelCallExecutionIds {
    fn next_model_call_id(&mut self) -> ModelCallId {
        self.calls.pop_front().expect("model-call identity fixture")
    }

    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
        self.entries
            .pop_front()
            .expect("semantic-entry identity fixture")
    }

    fn next_context_frontier_id(&mut self) -> ContextFrontierId {
        self.frontiers
            .pop_front()
            .expect("context-frontier identity fixture")
    }

    fn next_turn_id(&mut self) -> TurnId {
        self.turns
            .pop_front()
            .expect("successor-turn identity fixture")
    }
}

impl FixedStartupScanIds {
    fn new(
        failure_entries: impl IntoIterator<Item = SemanticTranscriptEntryId>,
        terminal_frontiers: impl IntoIterator<Item = ContextFrontierId>,
    ) -> Self {
        Self {
            failure_entries: failure_entries.into_iter().collect(),
            terminal_frontiers: terminal_frontiers.into_iter().collect(),
            reclassified_turns: VecDeque::new(),
        }
    }

    fn with_reclassified_turns(mut self, turns: impl IntoIterator<Item = TurnId>) -> Self {
        self.reclassified_turns = turns.into_iter().collect();
        self
    }
}

impl StartupScanIdGenerator for FixedStartupScanIds {
    fn next_failure_entry_id(&mut self) -> SemanticTranscriptEntryId {
        self.failure_entries
            .pop_front()
            .expect("the integration test supplies one failure entry per recovery")
    }

    fn next_terminal_frontier_id(&mut self) -> ContextFrontierId {
        self.terminal_frontiers
            .pop_front()
            .expect("the integration test supplies one terminal frontier per recovery")
    }

    fn next_reclassified_turn_id(&mut self, _accepted_input: AcceptedInputId) -> TurnId {
        self.reclassified_turns
            .pop_front()
            .expect("the integration test supplies one successor per recovered steering input")
    }
}

#[derive(Clone, Copy, Debug)]
struct RestartModelCallFixture {
    session: SessionId,
    turn: TurnId,
    attempt: TurnAttemptId,
    call: ModelCallId,
}

async fn checkpoint_restart_model_call(
    pool: &PgPool,
    seed: u128,
    authorize: bool,
) -> Result<RestartModelCallFixture, Box<dyn Error>> {
    let session = SessionId::from_uuid(Uuid::from_u128(seed + 1));
    let turn = TurnId::from_uuid(Uuid::from_u128(seed + 2));
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 3));
    let call = ModelCallId::from_uuid(Uuid::from_u128(seed + 4));
    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));

    CreateSessionRepository::new(pool.clone())
        .handle(prepared(
            seed + 7,
            seed + 1,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 8,
                seed + 1,
                "restart-classification request",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 9)),
            Some(turn),
        )
        .await?;
    activate_earliest_queued_turn(
        pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(seed + 10),
            starting_frontier: Uuid::from_u128(seed + 11),
            initial_attempt: attempt.into_uuid(),
        },
    )
    .await?;

    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one restart fixture target forms a catalog");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    assert!(matches!(
        repository
            .prepare_initial_call(
                session,
                call,
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 12)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 13)),
                ),
            )
            .await?,
        PrepareInitialModelCallOutcome::Checkpointed(checkpointed) if checkpointed == call
    ));
    if authorize {
        assert!(matches!(
            repository.authorize_send(session, call).await?,
            AuthorizeModelCallOutcome::Authorized(_)
        ));
    }

    Ok(RestartModelCallFixture {
        session,
        turn,
        attempt,
        call,
    })
}

async fn authorize_checkpointed_model_call(
    pool: &PgPool,
    seed: u128,
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        AuthorizedModelCall,
    ),
    Box<dyn Error>,
> {
    let fixture = checkpoint_restart_model_call(pool, seed, false).await?;
    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one issued fixture target forms a catalog");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    assert!(matches!(
        repository
            .prepare_initial_call(
                fixture.session,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 14)),
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 15)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 16)),
                ),
            )
            .await?,
        PrepareInitialModelCallOutcome::Ready { .. }
    ));
    let AuthorizeModelCallOutcome::Authorized(authorized) = repository
        .authorize_send(fixture.session, fixture.call)
        .await?
    else {
        panic!("the exact Prepared fixture authorizes")
    };
    Ok((fixture, repository, *authorized))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn embedded_migrator_connects_and_is_idempotent() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    migrate(&pool).await?;
    let connected: i32 = sqlx::query_scalar("SELECT 1").fetch_one(&pool).await?;
    assert_eq!(connected, 1);

    pool.close().await;
    drop(container);

    Ok(())
}

/// ADR-0045 / INV-006: an uncertain capability-failure closure is reconciled
/// from exact durable Prepared or complete known-failure state, including its
/// terminal attempt and call provenance, before any resubmission.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv006_model_call_capability_failure_reread_distinguishes_pending_and_committed()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7000;
    let fixture = checkpoint_restart_model_call(&pool, seed, false).await?;
    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one restart fixture target forms a catalog");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());

    let mut call_only = pool.begin().await?;
    let call_only_error = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                terminal_frontier_id = starting_frontier_id,
                active_phase_kind = NULL,
                current_attempt_id = NULL,
                terminal_disposition_kind = 'failed',
                terminal_attempt_id = NULL,
                terminal_model_call_id = $1
          WHERE turn_id = $2",
    )
    .bind(fixture.call.into_uuid())
    .bind(fixture.turn.into_uuid())
    .execute(&mut *call_only)
    .await
    .expect_err("a failed lifecycle cannot retain call-only provenance");
    assert_eq!(
        call_only_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_lifecycle_state_payload_shape")
    );
    call_only.rollback().await?;

    assert_eq!(
        repository
            .reread_capability_failure(fixture.session, fixture.call)
            .await?,
        RetainedCapabilityFailureStatus::Pending
    );
    let failed = repository
        .fail_prepared_call(
            fixture.session,
            fixture.call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 14)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 15)),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert_eq!(
        failed.call().expect("the prepared call closes").id(),
        fixture.call
    );
    assert_eq!(
        repository
            .reread_capability_failure(fixture.session, fixture.call)
            .await?,
        RetainedCapabilityFailureStatus::AlreadyCommitted
    );
    let terminal_execution: (Uuid, Uuid) = sqlx::query_as(
        "SELECT terminal_attempt_id, terminal_model_call_id
           FROM turn_lifecycle
          WHERE turn_id = $1
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'failed'",
    )
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        terminal_execution,
        (fixture.attempt.into_uuid(), fixture.call.into_uuid())
    );

    // A new durable input forces the scheduling loader to reconstruct the
    // complete failed prefix before it can append queued work.
    assert!(matches!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                start_input(
                    seed + 16,
                    seed + 1,
                    "work after failed model call",
                    1,
                    ModelSelectionOverride::UseSessionDefault,
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(seed + 17)),
                Some(TurnId::from_uuid(Uuid::from_u128(seed + 18))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));

    sqlx::query("ALTER TABLE turn_failed_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM turn_failed_outbox_event WHERE turn_id = $1")
        .bind(fixture.turn.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE turn_failed_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    assert!(matches!(
        repository
            .reread_capability_failure(fixture.session, fixture.call)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "retained capability failure durable closure is incomplete"
        ))
    ));

    let issued_seed = seed + 0x100;
    let (issued, issued_repository, authorized) =
        authorize_checkpointed_model_call(&pool, issued_seed).await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed);
    issued_repository
        .apply_terminal_observation(
            issued.session,
            observation.clone(),
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(issued_seed + 17)),
                ContextFrontierId::from_uuid(Uuid::from_u128(issued_seed + 18)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert!(matches!(
        issued_repository
            .reread_capability_failure(issued.session, issued.call)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "retained capability failure durable closure is incomplete"
        ))
    ));
    assert_eq!(
        issued_repository
            .reread_terminal_observation(issued.session, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );

    sqlx::query("ALTER TABLE turn_failed_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM turn_failed_outbox_event WHERE turn_id = $1")
        .bind(issued.turn.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE turn_failed_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    assert!(matches!(
        issued_repository
            .reread_terminal_observation(issued.session, &observation)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "retained observation terminal closure changed"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// ADR-0045: retained non-completed observations converge only when their
/// complete disposition-specific durable closure remains present.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_call_noncompleted_rereads_validate_each_durable_closure()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let cancelled_seed = 0x7200;
    let (cancelled, cancelled_repository, cancelled_authorized) =
        authorize_checkpointed_model_call(&pool, cancelled_seed).await?;
    let cancelled_observation = cancelled_authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Cancelled);
    cancelled_repository
        .apply_terminal_observation(
            cancelled.session,
            cancelled_observation.clone(),
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(cancelled_seed + 17)),
                ContextFrontierId::from_uuid(Uuid::from_u128(cancelled_seed + 18)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert_eq!(
        cancelled_repository
            .reread_terminal_observation(cancelled.session, &cancelled_observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    sqlx::query("ALTER TABLE turn_failed_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM turn_failed_outbox_event WHERE turn_id = $1")
        .bind(cancelled.turn.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE turn_failed_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    assert!(matches!(
        cancelled_repository
            .reread_terminal_observation(cancelled.session, &cancelled_observation)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "retained observation terminal closure changed"
        ))
    ));

    let refused_seed = 0x7300;
    let (refused, refused_repository, refused_authorized) =
        authorize_checkpointed_model_call(&pool, refused_seed).await?;
    let refused_observation = refused_authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Refused);
    refused_repository
        .apply_terminal_observation(
            refused.session,
            refused_observation.clone(),
            ModelCallTerminalIdentities::Refused(RefusedModelCallTurnIdentities::new(
                ContextFrontierId::from_uuid(Uuid::from_u128(refused_seed + 17)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert_eq!(
        refused_repository
            .reread_terminal_observation(refused.session, &refused_observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    sqlx::query("ALTER TABLE turn_refused_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM turn_refused_outbox_event WHERE turn_id = $1")
        .bind(refused.turn.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE turn_refused_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    assert!(matches!(
        refused_repository
            .reread_terminal_observation(refused.session, &refused_observation)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "retained observation terminal closure changed"
        ))
    ));

    let ambiguous_seed = 0x7400;
    let (ambiguous, ambiguous_repository, ambiguous_authorized) =
        authorize_checkpointed_model_call(&pool, ambiguous_seed).await?;
    let ambiguous_observation = ambiguous_authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Ambiguous);
    ambiguous_repository
        .apply_terminal_observation(
            ambiguous.session,
            ambiguous_observation.clone(),
            ModelCallTerminalIdentities::Ambiguous,
            |_| panic!("Ambiguous creates no pending-steering successors"),
        )
        .await?;
    assert_eq!(
        ambiguous_repository
            .reread_terminal_observation(ambiguous.session, &ambiguous_observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    sqlx::query("ALTER TABLE model_call_transition_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "DELETE FROM model_call_transition_outbox_event
          WHERE model_call_id = $1
            AND call_state_kind = 'terminal'",
    )
    .bind(ambiguous.call.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE model_call_transition_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    assert!(matches!(
        ambiguous_repository
            .reread_terminal_observation(ambiguous.session, &ambiguous_observation)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "retained observation terminal closure changed"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / S20 / S21 / INV-014 / INV-015 / INV-032 / INV-035: the production
/// persistence chain checkpoints Prepared with its non-secret credential
/// reference, reloads that reference instead of a changed deployment value,
/// separately authorizes send, and atomically commits exact assistant content,
/// completion, terminal frontier, lifecycle, call, attempt, and typed outbox
/// records.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_s20_s21_inv014_inv015_inv032_inv035_model_call_transactions_complete_first_reply()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8e1));
    let direct_selection =
        signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(0xce1));
    let mut create_service = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone()),
    );
    let CreateSessionOutcome::Applied(_) = create_service
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x4e1)),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct_selection)),
        )?)
        .await?
    else {
        panic!("the model-call fixture session must be created");
    };

    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9e1));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xae1));
    let mut submit_service = SubmitInputService::new(
        FixedSubmitInputIds::new([accepted_input], [turn]),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(origin),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x4e2)),
            session,
            UserContent::try_text("exact user request".to_owned())
                .expect("fixture user content is admitted"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        )?)
        .await?
    else {
        panic!("the model-call fixture input must be accepted");
    };
    assert_eq!(origin.accepted_input(), accepted_input);
    assert_eq!(origin.turn(), turn);

    let starting_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xee1));
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xbe1));
    let mut activation_service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde1))],
            [starting_frontier],
            [attempt],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) =
        activation_service.execute(session).await?
    else {
        panic!("the model-call fixture turn must activate");
    };
    assert_eq!(activated.turn(), turn);

    let provider_identity = ProviderModelIdentity::from_uuid(Uuid::from_u128(0xfe1));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        direct_selection,
        ResolvedProviderTarget::naming(provider_identity),
    )])
    .expect("one immutable direct target forms a catalog");
    let pinned_credential_reference = model_credential_reference();
    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        targets.clone(),
        pinned_credential_reference.clone(),
    );
    let call = ModelCallId::from_uuid(Uuid::from_u128(0xce2));
    let PrepareInitialModelCallOutcome::Checkpointed(checkpointed_call) = repository
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde8)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xee8)),
            ),
        )
        .await?
    else {
        panic!("a fresh call must stop at its Prepared checkpoint");
    };
    assert_eq!(checkpointed_call, call);

    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        ModelCallCredentialReference::new("replacement-provider-reference"),
    );
    let unused_call_candidate = ModelCallId::from_uuid(Uuid::from_u128(0xce3));
    let PrepareInitialModelCallOutcome::Ready {
        request: prepared,
        credential_reference,
    } = repository
        .prepare_initial_call(
            session,
            unused_call_candidate,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde9)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xee9)),
            ),
        )
        .await?
    else {
        panic!("a later invocation must reload the committed Prepared call");
    };
    assert_eq!(credential_reference, pinned_credential_reference);
    assert_eq!(prepared.session(), session);
    assert_eq!(prepared.turn(), turn);
    assert_eq!(prepared.attempt(), attempt);
    assert_eq!(prepared.call().id(), call);
    assert_eq!(prepared.call().target().identity(), provider_identity);
    assert_eq!(prepared.frontier_entries().len(), 1);
    assert_eq!(
        prepared
            .origin_content(accepted_input)
            .expect("the frontier origin must carry its checked receipt content")
            .text()
            .as_str(),
        "exact user request"
    );
    assert_eq!(
        repository
            .reread_ambiguous_authorization(session, &prepared)
            .await?,
        ModelCallAuthorizationReread::Prepared
    );

    let AuthorizeModelCallOutcome::Authorized(authorized) =
        repository.authorize_send(session, call).await?
    else {
        panic!("the exact Prepared call must authorize")
    };
    let authorized = *authorized;
    assert_eq!(
        repository.authorize_send(session, call).await?,
        AuthorizeModelCallOutcome::NoSend
    );
    assert_eq!(
        repository
            .reread_ambiguous_authorization(session, &prepared)
            .await?,
        ModelCallAuthorizationReread::InFlight(Box::new(authorized.clone()))
    );
    let observation_correlation = authorized.observation_correlation();
    assert_eq!(authorized.call().id(), call);
    assert_eq!(
        authorized.call().state(),
        signalbox_domain::CurrentModelCallState::InFlight
    );
    assert_eq!(
        authorized.attempt().state(),
        &CurrentTurnAttemptState::Running
    );
    assert_eq!(
        repository
            .reread_ambiguous_authorization(session, &prepared)
            .await?,
        ModelCallAuthorizationReread::InFlight(Box::new(authorized.clone()))
    );
    assert_eq!(
        repository
            .prepare_initial_call(
                session,
                ModelCallId::from_uuid(Uuid::from_u128(0xce4)),
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdea)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(0xeea)),
                ),
            )
            .await?,
        PrepareInitialModelCallOutcome::NoWork
    );

    let assistant_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde2));
    let completion_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde3));
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xee2));
    let assistant_text = AssistantText::try_new("exact assistant reply".to_owned())
        .expect("fixture assistant content is admitted");
    let observation = observation_correlation.bind_terminal_observation(
        ModelCallTerminalObservation::Completed {
            assistant_text: vec![assistant_text.clone()],
        },
    );
    assert_eq!(
        repository
            .reread_terminal_observation(session, &observation)
            .await?,
        RetainedModelCallObservationStatus::Pending
    );
    let outcome = repository
        .apply_terminal_observation(
            session,
            observation.clone(),
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![assistant_entry],
                completion_entry,
                terminal_frontier,
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert_eq!(
        repository
            .reread_terminal_observation(session, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    let ModelCallTerminalOutcome::Completed(completed) = outcome else {
        panic!("the definitive response must complete the turn");
    };
    assert_eq!(completed.turn(), turn);
    assert_eq!(completed.assistant_entries().len(), 1);
    assert_eq!(
        completed.assistant_entries()[0].payload(),
        &signalbox_domain::SemanticTranscriptEntryPayload::AssistantText {
            producing_call: call,
            value: assistant_text.clone(),
        }
    );

    let durable_shape: (i64, i64, i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM model_call
              WHERE model_call_id = $1
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed'),
            (SELECT count(*) FROM turn_attempt
              WHERE turn_attempt_id = $2
                AND state_kind = 'ended'
                AND end_disposition = 'turn_completed'),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE semantic_entry_id = $3
                AND payload_kind = 'assistant_text'
                AND assistant_text_value = $8
                AND producing_model_call_id = $1),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE semantic_entry_id = $4
                AND payload_kind = 'turn_completed'
                AND completed_turn_id = $5),
            (SELECT count(*) FROM turn_lifecycle
              WHERE turn_id = $5
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed'
                AND terminal_frontier_id = $6
                AND terminal_attempt_id = $2
                AND terminal_model_call_id = $1),
            (SELECT count(*) FROM model_call_transition_outbox_event
              WHERE model_call_id = $1),
            (SELECT count(*) FROM turn_completed_outbox_event
              WHERE turn_id = $5
                AND model_call_id = $1
                AND completion_entry_id = $4
                AND terminal_frontier_id = $6),
            (SELECT count(*) FROM turn_lifecycle
              WHERE turn_id = $5
                AND pinned_provider_model_identity_id = $7),
            (SELECT count(*) FROM model_call
              WHERE model_call_id = $1
                AND credential_reference = $9)",
    )
    .bind(call.into_uuid())
    .bind(attempt.into_uuid())
    .bind(assistant_entry.into_uuid())
    .bind(completion_entry.into_uuid())
    .bind(turn.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .bind(provider_identity.into_uuid())
    .bind(assistant_text.as_str())
    .bind(pinned_credential_reference.as_str())
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_shape, (1, 1, 1, 1, 1, 3, 1, 1, 1));

    sqlx::query("ALTER TABLE turn_completed_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM turn_completed_outbox_event WHERE turn_id = $1")
        .bind(turn.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE turn_completed_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    assert!(matches!(
        repository
            .reread_terminal_observation(session, &observation)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "retained observation terminal closure changed"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S02 / INV-014 / INV-015: the application service and its four PostgreSQL
/// ports preserve the separate Prepared checkpoint, provider effect, and
/// terminal observation commits for one deterministic assistant reply.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_inv014_inv015_application_service_completes_scripted_reply()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x18e1));
    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(0x1ce1));
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(
            0x14e1,
            0x18e1,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;

    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x19e1));
    let turn = TurnId::from_uuid(Uuid::from_u128(0x1ae1));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x14e2,
                0x18e1,
                "service user request",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            accepted_input,
            Some(turn),
        )
        .await?;
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0x1be1));
    let mut activation = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                0x1de1,
            ))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0x1ee1))],
            [attempt],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert!(matches!(
        activation.execute(session).await?,
        StartEligibleTurnOutcome::Activated(_)
    ));

    let provider_identity = ProviderModelIdentity::from_uuid(Uuid::from_u128(0x1fe1));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider_identity),
    )])
    .expect("one immutable direct target forms a catalog");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let call = ModelCallId::from_uuid(Uuid::from_u128(0x1ce2));
    let unused_call = ModelCallId::from_uuid(Uuid::from_u128(0x1ce3));
    let assistant_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de4));
    let completion_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de5));
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0x1ee4));
    let assistant_text = AssistantText::try_new(String::from("service assistant reply"))
        .expect("fixture assistant content is admitted");
    let mut service = ModelCallExecutionService::new(
        FixedModelCallExecutionIds::new(
            [call, unused_call],
            [
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de2)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de3)),
                assistant_entry,
                completion_entry,
            ],
            [
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1ee2)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1ee3)),
                terminal_frontier,
            ],
            [],
        ),
        repository.clone(),
        repository.clone(),
        repository.clone(),
        repository,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![assistant_text.clone()],
            },
        )]),
        InProcessAttemptDispatchGate::default(),
    );

    assert_eq!(
        service.execute(session).await?,
        ModelCallExecutionOutcome::Checkpointed(call)
    );
    let ModelCallExecutionOutcome::ObservationCommitted(outcome) = service.execute(session).await?
    else {
        panic!("the resumed prepared call must commit its scripted observation")
    };
    let ModelCallTerminalOutcome::Completed(completed) = *outcome else {
        panic!("the scripted completion must complete the turn")
    };
    assert_eq!(completed.turn(), turn);
    assert_eq!(completed.assistant_entries()[0].identity(), assistant_entry);
    assert_eq!(
        completed.assistant_entries()[0].payload(),
        &signalbox_domain::SemanticTranscriptEntryPayload::AssistantText {
            producing_call: call,
            value: assistant_text,
        }
    );
    let (_, _, _, _, _, provider, _, _) = service.into_parts();
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(provider.interaction_count(), 1);

    let durable_terminal: (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM model_call
              WHERE model_call_id = $1
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed'),
            (SELECT count(*) FROM turn_lifecycle
              WHERE turn_id = $2
                AND state_kind = 'terminal'
                AND terminal_frontier_id = $3)",
    )
    .bind(call.into_uuid())
    .bind(turn.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_terminal, (1, 1));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S03 / S04 / S08 / INV-006 / INV-014 / INV-016 / INV-034: the production
/// startup repository applies call-aware recovery under its session lock:
/// Prepared is known-failed with exact terminal execution provenance while
/// reclassifying newly observed steering, an issued call becomes an exact
/// ambiguity wait, and replay changes neither.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s03_s04_inv006_inv014_inv034_startup_scan_classifies_prepared_and_issued_model_calls()
-> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let prepared = checkpoint_restart_model_call(&pool, 0x2000, false).await?;
    let issued = checkpoint_restart_model_call(&pool, 0x3000, true).await?;
    let prepared_steering = AcceptedInputId::from_uuid(Uuid::from_u128(0x6100));
    let issued_steering = AcceptedInputId::from_uuid(Uuid::from_u128(0x6101));
    assert!(matches!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                SubmitInput::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(0x4100)),
                    prepared.session,
                    UserContent::try_text(String::from("steering accepted before restart"))
                        .expect("fixture steering content is admitted"),
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn: prepared.turn,
                    },
                ),
                prepared_steering,
                None,
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));
    assert!(matches!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                SubmitInput::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(0x4200)),
                    issued.session,
                    UserContent::try_text(String::from("steering accepted before restart"))
                        .expect("fixture steering content is admitted"),
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn: issued.turn,
                    },
                ),
                issued_steering,
                None,
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));

    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let mut scan = StartupScanService::new(
        FixedStartupScanIds::new(
            [
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x4001)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x4002)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x4003)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x4004)),
            ],
            [
                ContextFrontierId::from_uuid(Uuid::from_u128(0x5001)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x5002)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x5003)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x5004)),
            ],
        )
        .with_reclassified_turns([prepared.turn, TurnId::from_uuid(Uuid::from_u128(0x6201))]),
        PostgresStartupScanRepository::new(restarted_pool.clone()),
    );

    let first = scan.execute().await?;
    assert!(first.is_complete());
    assert_eq!(first.recovered_turn_count(), 1);

    let prepared_state: (String, String, String, String, String, Uuid, Uuid) = sqlx::query_as(
        "SELECT call.state_kind,
                call.terminal_disposition_kind,
                attempt.state_kind,
                attempt.end_disposition,
                turn.state_kind,
                turn.terminal_attempt_id,
                turn.terminal_model_call_id
           FROM model_call AS call
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = call.turn_attempt_id
           JOIN turn_lifecycle AS turn
             ON turn.turn_id = call.turn_id
          WHERE call.model_call_id = $1",
    )
    .bind(prepared.call.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        prepared_state,
        (
            "terminal".into(),
            "known_failed".into(),
            "ended".into(),
            "lost".into(),
            "terminal".into(),
            prepared.attempt.into_uuid(),
            prepared.call.into_uuid(),
        )
    );

    let issued_state: (String, String, String, String, String, Uuid) = sqlx::query_as(
        "SELECT call.state_kind,
                call.terminal_disposition_kind,
                attempt.state_kind,
                attempt.end_disposition,
                turn.active_phase_kind,
                turn.recovery_model_call_id
           FROM model_call AS call
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = call.turn_attempt_id
           JOIN turn_lifecycle AS turn
             ON turn.turn_id = call.turn_id
          WHERE call.model_call_id = $1",
    )
    .bind(issued.call.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        issued_state,
        (
            "terminal".into(),
            "ambiguous".into(),
            "ended".into(),
            "lost".into(),
            "awaiting_model_call_recovery".into(),
            issued.call.into_uuid(),
        )
    );
    let steering_state: (String, Option<Uuid>, String, Option<Uuid>) = sqlx::query_as(
        "SELECT prepared.disposition_kind,
                prepared.origin_turn_id,
                issued.disposition_kind,
                issued.origin_turn_id
           FROM accepted_input AS prepared
           CROSS JOIN accepted_input AS issued
          WHERE prepared.accepted_input_id = $1
            AND issued.accepted_input_id = $2",
    )
    .bind(prepared_steering.into_uuid())
    .bind(issued_steering.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        steering_state,
        (
            "reclassified_as_turn_origin".into(),
            Some(Uuid::from_u128(0x6201)),
            "pending_steering".into(),
            None,
        )
    );
    assert_eq!(
        PostgresStartupScanRepository::new(restarted_pool.clone())
            .recover(
                prepared.session,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x6301)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(0x6302)),
                ),
                |_| panic!("a stale terminal inventory entry needs no successor identity"),
            )
            .await?,
        StartupScanSessionOutcome::NoActiveTurn
    );

    let replay = scan.execute().await?;
    assert!(replay.is_complete());
    assert_eq!(replay.recovered_turn_count(), 0);
    let unchanged: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM model_call
              WHERE model_call_id IN ($1, $2) AND state_kind = 'terminal'),
            (SELECT count(*) FROM turn_attempt
              WHERE turn_attempt_id IN ($3, $4) AND state_kind = 'ended'),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE failed_turn_id = $5),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE failed_turn_id = $6)",
    )
    .bind(prepared.call.into_uuid())
    .bind(issued.call.into_uuid())
    .bind(prepared.attempt.into_uuid())
    .bind(issued.attempt.into_uuid())
    .bind(prepared.turn.into_uuid())
    .bind(issued.turn.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(unchanged, (2, 2, 1, 0));
    assert_ne!(prepared.session, issued.session);

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S04 / INV-014 / INV-034: restart recovery reconstructs a committed call
/// from its durable provider target even after deployment configuration remaps
/// the selected model.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_inv014_inv034_restart_recovery_preserves_durable_target_after_catalog_remap()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7000;
    let fixture = checkpoint_restart_model_call(&pool, seed, false).await?;
    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let durable_provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));
    let remapped_provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 20));
    let remapped_targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(remapped_provider),
    )])
    .expect("one remapped target forms a catalog");
    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        remapped_targets,
        model_credential_reference(),
    );

    let outcome = repository
        .recover_after_restart(
            fixture.session,
            fixture.call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 30)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 31)),
            ),
        )
        .await?;
    let ModelCallTerminalOutcome::Failed(failed) = outcome else {
        panic!("the durable Prepared call must recover as known failure");
    };
    assert_eq!(
        failed
            .call()
            .expect("restart recovery retains the physical call")
            .target()
            .identity(),
        durable_provider
    );
    assert_ne!(durable_provider, remapped_provider);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S04 / S08 / S09 / INV-016: steering accepted after send authorization is
/// atomically reclassified when the source completes. Its immutable command
/// still replays PendingSteering, while the inherited successor enters the
/// ordinary scheduler and activates after the terminal source.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_s08_s09_inv016_terminal_call_reclassifies_and_schedules_pending_steering()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8e4));
    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(0xce4));
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(
            0x4e4,
            0x8e4,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;

    let source_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9e4));
    let source_turn = TurnId::from_uuid(Uuid::from_u128(0xae4));
    let inputs = SubmitInputRepository::new(pool.clone());
    inputs
        .handle(
            start_input(
                0x4e5,
                0x8e4,
                "source request",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            source_input,
            Some(source_turn),
        )
        .await?;
    let source_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xbe4));
    let mut source_activation = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde4))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xee4))],
            [source_attempt],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert!(matches!(
        source_activation.execute(session).await?,
        StartEligibleTurnOutcome::Activated(_)
    ));

    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(0xfe4));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one target is a valid catalog");
    let mut calls =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let call = ModelCallId::from_uuid(Uuid::from_u128(0xce5));
    assert!(matches!(
        calls
            .prepare_initial_call(
                session,
                call,
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdf4)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(0xef4)),
                ),
            )
            .await?,
        PrepareInitialModelCallOutcome::Checkpointed(checkpointed) if checkpointed == call
    ));
    let AuthorizeModelCallOutcome::Authorized(authorized) =
        calls.authorize_send(session, call).await?
    else {
        panic!("the exact Prepared call must authorize")
    };
    let authorized = *authorized;

    let steering_command = DurableCommandId::from_uuid(Uuid::from_u128(0x4e6));
    let steering_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9e5));
    let recorded = inputs
        .handle(
            SubmitInput::new(
                steering_command,
                session,
                UserContent::try_text("follow-up steering".to_owned())
                    .expect("fixture content is valid"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: source_turn,
                },
            ),
            steering_input,
            None,
        )
        .await?;
    assert!(matches!(
        recorded,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));

    let successor = TurnId::from_uuid(Uuid::from_u128(0xae5));
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xee5));
    let outcome = calls
        .commit_observation(
            session,
            authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::Completed {
                    assistant_text: vec![
                        AssistantText::try_new("source reply".to_owned())
                            .expect("fixture assistant content is valid"),
                    ],
                }),
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde5))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde6)),
                terminal_frontier,
            )),
            |accepted| {
                assert_eq!(accepted, steering_input);
                successor
            },
        )
        .await?;
    let ModelCallTerminalOutcome::Completed(completed) = outcome else {
        panic!("the source call must complete");
    };
    assert_eq!(completed.reclassified_pending_steering().len(), 1);
    assert_eq!(
        completed.reclassified_pending_steering()[0].turn(),
        successor
    );

    let durable: (String, Uuid, Uuid, String, i64) = sqlx::query_as(
        "SELECT accepted.disposition_kind,
                accepted.expected_active_turn_id,
                accepted.origin_turn_id,
                successor.state_kind,
                (SELECT count(*)
                   FROM queued_input_origin AS queued
                  WHERE queued.turn_id = $3
                    AND queued.accepted_input_id = $1
                    AND queued.source_configuration_turn_id = $4
                    AND queued.defaults_version IS NULL
                    AND queued.requested_model_kind IS NULL
                    AND queued.frozen_model_kind IS NULL
                    AND queued.model_parameters IS NULL
                    AND queued.known_provider_failure_retry IS NULL
                    AND queued.model_fallback IS NULL)
           FROM accepted_input AS accepted
           JOIN turn_lifecycle AS successor
             ON successor.turn_id = accepted.origin_turn_id
          WHERE accepted.accepted_input_id = $1
            AND accepted.session_id = $2",
    )
    .bind(steering_input.into_uuid())
    .bind(session.into_uuid())
    .bind(successor.into_uuid())
    .bind(source_turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        durable,
        (
            "reclassified_as_turn_origin".into(),
            source_turn.into_uuid(),
            successor.into_uuid(),
            "queued".into(),
            1,
        )
    );

    let replay = inputs
        .load(steering_command)
        .await?
        .expect("the immutable command receipt must remain readable");
    assert!(matches!(
        replay.result(),
        SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(pending))
            if pending.accepted_input() == steering_input
                && pending.binding().source_turn() == source_turn
    ));
    let (eligible, continuation) = PostgresEligibilitySweep::new(pool.clone())
        .find_sessions()
        .await?
        .into_parts();
    assert!(!continuation);
    assert_eq!(eligible, vec![session]);

    let mut successor_activation = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde7))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xee6))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xbe5))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) =
        successor_activation.execute(session).await?
    else {
        panic!("the reclassified successor must activate");
    };
    assert_eq!(activated.turn(), successor);
    assert_eq!(
        activated.start().lineage(),
        AcceptedInputStartingLineage::After {
            immediate_predecessor: source_turn,
        }
    );
    assert_eq!(
        activated.configuration_provenance(),
        &TurnConfigurationProvenance::InheritedForReclassifiedSteering(
            signalbox_domain::SteeringBinding::new(source_turn),
        )
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S21 / INV-006 / INV-014 / INV-032: immutable target resolution failure
/// creates no targetless call and atomically closes the prepared attempt and
/// turn with its semantic failure boundary and typed outbox event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s21_inv006_inv014_inv032_target_unavailable_closes_without_model_call()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8f1));
    let direct_selection =
        signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(0xcf1));
    let mut create_service = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone()),
    );
    let CreateSessionOutcome::Applied(_) = create_service
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x4f1)),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct_selection)),
        )?)
        .await?
    else {
        panic!("the target-miss fixture session must be created");
    };

    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9f1));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xaf1));
    let mut submit_service = SubmitInputService::new(
        FixedSubmitInputIds::new([accepted_input], [turn]),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(origin),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x4f2)),
            session,
            UserContent::try_text("request with unavailable target".to_owned())
                .expect("fixture user content is admitted"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        )?)
        .await?
    else {
        panic!("the target-miss fixture input must be accepted");
    };
    assert_eq!(origin.accepted_input(), accepted_input);
    assert_eq!(origin.turn(), turn);

    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xbf1));
    let mut activation_service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdf1))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xef1))],
            [attempt],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) =
        activation_service.execute(session).await?
    else {
        panic!("the target-miss fixture turn must activate");
    };
    assert_eq!(activated.turn(), turn);

    let targets = ModelTargetCatalog::try_from_definitions([])
        .expect("an empty immutable target catalog is valid");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let call_candidate = ModelCallId::from_uuid(Uuid::from_u128(0xcf2));
    let failure_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdf2));
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xef2));
    let PrepareInitialModelCallOutcome::TargetUnavailable(failed) = repository
        .prepare_initial_call(
            session,
            call_candidate,
            FailedModelCallTurnIdentities::new(failure_entry, terminal_frontier),
        )
        .await?
    else {
        panic!("the unavailable configured target must close without a call");
    };
    assert_eq!(failed.turn(), turn);
    assert!(failed.call().is_none());

    let durable_shape: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM model_call
              WHERE model_call_id = $1),
            (SELECT count(*) FROM turn_attempt
              WHERE turn_attempt_id = $2
                AND state_kind = 'ended'
                AND end_variant = 'without_stop'
                AND end_disposition = 'known_failure'),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE semantic_entry_id = $3
                AND payload_kind = 'turn_failed'
                AND failed_turn_id = $4),
            (SELECT count(*) FROM turn_lifecycle
              WHERE turn_id = $4
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'failed'
                AND terminal_frontier_id = $5
                AND terminal_attempt_id = $2
                AND terminal_model_call_id IS NULL),
            (SELECT count(*) FROM turn_failed_outbox_event
              WHERE turn_id = $4
                AND failure_entry_id = $3
                AND terminal_frontier_id = $5)",
    )
    .bind(call_candidate.into_uuid())
    .bind(attempt.into_uuid())
    .bind(failure_entry.into_uuid())
    .bind(turn.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_shape, (0, 1, 1, 1, 1));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-007 / INV-009: migration 004 gives every preexisting session its
/// scheduler serialization row and every accepted queued origin one queued
/// lifecycle row without inventing start, frontier, semantic, or attempt
/// facts; migration 005 preserves that exact legacy receipt and correlation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv007_inv009_turn_storage_migration_backfills_existing_queued_work()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = unmigrated_postgres().await?;
    let mut connection = pool.acquire().await?;
    connection
        .ensure_migrations_table("_sqlx_migrations")
        .await?;
    for migration in MIGRATOR.iter().take(3) {
        connection.apply("_sqlx_migrations", migration).await?;
    }
    drop(connection);

    let mut transaction = pool.begin().await?;
    sqlx::raw_sql(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000401',
             'create_session', 1, transaction_timestamp());
         INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES
            ('70000000-0000-7000-8000-000000000401',
             'owner_initiated', 'none');
         INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000401', 1, 'direct',
             '80000000-0000-7000-8000-000000000401', NULL);
         INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ('70000000-0000-7000-8000-000000000401', 1);
         INSERT INTO create_session_command
            (command_id, command_kind, storage_version,
             creation_cause, ancestry_kind, initial_defaults_version,
             model_selection_kind, direct_model_selection_id, model_alias_id,
             result_kind, created_session_id)
         VALUES
            ('10000000-0000-4000-8000-000000000401',
             'create_session', 1, 'owner_initiated', 'none', 1,
             'direct', '80000000-0000-7000-8000-000000000401', NULL,
             'applied', '70000000-0000-7000-8000-000000000401');
         INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('30000000-0000-4000-8000-000000000401',
             'submit_input', 1, transaction_timestamp());
         INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position)
         VALUES
            ('30000000-0000-4000-8000-000000000401',
             'submit_input', 1,
             '70000000-0000-7000-8000-000000000401',
             'owner', NULL, NULL, 'text', 'queued before migration',
             'start_when_no_active_turn', NULL, 1,
             'use_session_default', NULL, NULL, NULL,
             'applied', NULL,
             '70000000-0000-7000-8000-000000000401',
             '90000000-0000-7000-8000-000000000401',
             'a0000000-0000-7000-8000-000000000401',
             NULL, NULL, NULL, NULL, NULL, NULL);
         INSERT INTO accepted_input
            (accepted_input_id, accepting_command_id, session_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             acceptance_position, disposition_kind, origin_turn_id)
         VALUES
            ('90000000-0000-7000-8000-000000000401',
             '30000000-0000-4000-8000-000000000401',
             '70000000-0000-7000-8000-000000000401',
             'text', 'queued before migration',
             'start_when_no_active_turn', NULL, 1,
             'use_session_default', NULL, NULL, NULL,
             1, 'origin_of',
             'a0000000-0000-7000-8000-000000000401');
         INSERT INTO queued_input_origin
            (turn_id, accepted_input_id, session_id, acceptance_position,
             priority_kind, defaults_version,
             requested_model_kind, requested_direct_model_selection_id,
             requested_model_alias_id, frozen_model_kind,
             frozen_direct_model_selection_id, frozen_model_alias_id,
             frozen_alias_selected_direct_id, model_parameters,
             known_provider_failure_retry, model_fallback)
         VALUES
            ('a0000000-0000-7000-8000-000000000401',
             '90000000-0000-7000-8000-000000000401',
             '70000000-0000-7000-8000-000000000401',
             1, 'ordinary', 1,
             'direct', '80000000-0000-7000-8000-000000000401', NULL,
             'direct', '80000000-0000-7000-8000-000000000401', NULL, NULL,
             'provider_defaults', 'disabled', 'disabled');",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    migrate(&pool).await?;

    let backfilled: (i64, String, i64, i64, i64, bool) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM session_scheduler WHERE session_id = $1),
            turn.state_kind,
            (SELECT count(*) FROM semantic_transcript_entry),
            (SELECT count(*) FROM context_frontier),
            (SELECT count(*) FROM turn_attempt),
            typed.result_actual_active_turn_id IS NULL
         FROM turn_lifecycle AS turn
         JOIN accepted_input AS accepted
           ON accepted.accepted_input_id = turn.origin_accepted_input_id
         JOIN submit_input_command AS typed
           ON typed.command_id = accepted.accepting_command_id
         WHERE turn.turn_id = $2",
    )
    .bind(Uuid::from_u128(0x70000000000070008000000000000401))
    .bind(Uuid::from_u128(0xa0000000000070008000000000000401))
    .fetch_one(&pool)
    .await?;
    assert_eq!(backfilled, (1, "queued".to_owned(), 0, 0, 0, true));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-002 / INV-008 / INV-012: the Postgres adapters preserve
/// application command outcomes, return the complete current session
/// projection, and keep infrastructure failure nonterminal.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv002_inv008_inv012_application_session_services_use_postgres_adapters()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(0x601));
    let request = CreateSessionRequest::try_new(
        command_id,
        SessionConfigurationDefaults::new(direct(0x801)),
    )?;
    let conflicting_request =
        CreateSessionRequest::try_new(command_id, SessionConfigurationDefaults::new(alias(0x802)))?;
    let winner = SessionId::from_uuid(Uuid::from_u128(0x701));
    let replay_candidate = SessionId::from_uuid(Uuid::from_u128(0x702));
    let conflicting_candidate = SessionId::from_uuid(Uuid::from_u128(0x703));
    let repository = CreateSessionRepository::new(pool.clone());
    let mut service = CreateSessionService::new(
        FixedSessionIds::new([winner, replay_candidate, conflicting_candidate]),
        repository,
    );

    let first = service.execute(request).await?;
    let replay = service.execute(request).await?;
    assert_eq!(first, replay);
    let CreateSessionOutcome::Applied(recorded_receipt) = first else {
        panic!("first application must return the recorded applied receipt");
    };
    assert_eq!(recorded_receipt.session(), winner);
    assert_ne!(recorded_receipt.session(), replay_candidate);

    assert_eq!(
        service.execute(conflicting_request).await?,
        CreateSessionOutcome::ConflictingReuse { command_id }
    );
    let committed_counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM durable_command),
            (SELECT count(*) FROM session),
            (SELECT count(*) FROM session_scheduler)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(committed_counts, (1, 1, 1));

    let load_service = LoadSessionService::new(SessionRepository::new(pool.clone()));
    let loaded = load_service
        .execute(winner)
        .await?
        .expect("the created session is visible through the application query");
    assert_eq!(loaded.id(), winner);
    assert_eq!(
        loaded.current_configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::first()
    );
    assert_eq!(
        load_service
            .execute(SessionId::from_uuid(Uuid::from_u128(0x7ff)))
            .await?,
        None
    );

    let (_session_ids, repository) = service.into_parts();
    pool.close().await;
    let unavailable_request = CreateSessionRequest::try_new(
        DurableCommandId::from_uuid(Uuid::from_u128(0x602)),
        SessionConfigurationDefaults::new(direct(0x803)),
    )?;
    let mut unavailable_service = CreateSessionService::new(
        FixedSessionIds::new([SessionId::from_uuid(Uuid::from_u128(0x704))]),
        repository,
    );
    let error = unavailable_service
        .execute(unavailable_request)
        .await
        .expect_err("a closed pool cannot become a terminal command outcome");
    assert!(matches!(
        error,
        CreateSessionError::Transaction(CreateSessionRepositoryError::Database(_))
    ));

    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv003_inv008_inv012_create_session_schema_preserves_typed_facts()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;

    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000001',
             'create_session', 1, TIMESTAMPTZ '2026-07-18 00:00:00+00')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES
            ('70000000-0000-7000-8000-000000000001',
             'owner_initiated', 'none')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_scheduler (session_id)
         VALUES ('70000000-0000-7000-8000-000000000001')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000001', 1, 'direct',
             '70000000-0000-7000-8000-000000000002', NULL)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ('70000000-0000-7000-8000-000000000001', 1)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO create_session_command
            (command_id, command_kind, storage_version,
             creation_cause, ancestry_kind, initial_defaults_version,
             model_selection_kind, direct_model_selection_id, model_alias_id,
             result_kind, created_session_id)
         VALUES
            ('10000000-0000-4000-8000-000000000001',
             'create_session', 1, 'owner_initiated', 'none', 1,
             'direct', '70000000-0000-7000-8000-000000000002', NULL,
             'applied', '70000000-0000-7000-8000-000000000001')",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    let stored: (String, String, String, String) = sqlx::query_as(
        "SELECT s.creation_cause,
                s.ancestry_kind,
                d.model_selection_kind,
                c.result_kind
         FROM session AS s
         JOIN session_defaults_version AS d USING (session_id)
         JOIN create_session_command AS c
           ON c.created_session_id = s.session_id",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored,
        (
            "owner_initiated".to_owned(),
            "none".to_owned(),
            "direct".to_owned(),
            "applied".to_owned()
        )
    );

    let generated_identity_defaults: i64 = sqlx::query_scalar(
        "SELECT count(*)
         FROM information_schema.columns
         WHERE table_schema = 'public'
           AND data_type = 'uuid'
           AND is_generated = 'NEVER'
           AND column_default IS NOT NULL",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(generated_identity_defaults, 0);

    let duplicate_command_id = sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000001',
             'create_session', 1, TIMESTAMPTZ '2026-07-18 00:00:01+00')",
    )
    .execute(&pool)
    .await
    .expect_err("the owner-global command ID must be unique");
    assert_eq!(
        duplicate_command_id
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23505")
    );

    pool.close().await;
    drop(container);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv012_registry_and_create_session_constraints_reject_torn_or_conflicting_records()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let mut registry_only = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000011',
             'create_session', 1, TIMESTAMPTZ '2026-07-18 00:00:00+00')",
    )
    .execute(&mut *registry_only)
    .await?;
    let missing_typed_record = registry_only
        .commit()
        .await
        .expect_err("a registry claim without its typed record must not commit");
    assert_eq!(
        missing_typed_record
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    let invalid_kind = sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000012',
             'unsupported_command', 1, TIMESTAMPTZ '2026-07-18 00:00:00+00')",
    )
    .execute(&pool)
    .await
    .expect_err("an unadmitted command kind must be rejected");
    assert_eq!(
        invalid_kind
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let mut session_without_command = pool.begin().await?;
    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES
            ('70000000-0000-7000-8000-000000000021',
             'owner_initiated', 'none')",
    )
    .execute(&mut *session_without_command)
    .await?;
    sqlx::query(
        "INSERT INTO session_scheduler (session_id)
         VALUES ('70000000-0000-7000-8000-000000000021')",
    )
    .execute(&mut *session_without_command)
    .await?;
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000021', 1, 'direct',
             '70000000-0000-7000-8000-000000000022', NULL)",
    )
    .execute(&mut *session_without_command)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ('70000000-0000-7000-8000-000000000021', 1)",
    )
    .execute(&mut *session_without_command)
    .await?;
    let missing_create_command = session_without_command
        .commit()
        .await
        .expect_err("a session without its CreateSession record must not commit");
    assert_eq!(
        missing_create_command
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    pool.close().await;
    drop(container);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_schema_rejects_invalid_provenance_defaults_and_mutation() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;

    for statement in [
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES
            ('70000000-0000-7000-8000-000000000011',
             'delegated', 'none')",
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES
            ('70000000-0000-7000-8000-000000000012',
             'owner_initiated', 'single_source')",
    ] {
        let error = sqlx::query(statement)
            .execute(&pool)
            .await
            .expect_err("unsupported provenance must be rejected");
        assert_eq!(
            error
                .as_database_error()
                .and_then(|database_error| database_error.code())
                .as_deref(),
            Some("23514")
        );
    }

    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000013',
             'create_session', 1, TIMESTAMPTZ '2026-07-18 00:00:00+00')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES
            ('70000000-0000-7000-8000-000000000013',
             'owner_initiated', 'none')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_scheduler (session_id)
         VALUES ('70000000-0000-7000-8000-000000000013')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000013', 1, 'alias',
             NULL, '70000000-0000-7000-8000-000000000014')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ('70000000-0000-7000-8000-000000000013', 1)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO create_session_command
            (command_id, command_kind, storage_version,
             creation_cause, ancestry_kind, initial_defaults_version,
             model_selection_kind, direct_model_selection_id, model_alias_id,
             result_kind, created_session_id)
         VALUES
            ('10000000-0000-4000-8000-000000000013',
             'create_session', 1, 'owner_initiated', 'none', 1,
             'alias', NULL, '70000000-0000-7000-8000-000000000014',
             'applied', '70000000-0000-7000-8000-000000000013')",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    let zero_version = sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000013', 0, 'direct',
             '70000000-0000-7000-8000-000000000015', NULL)",
    )
    .execute(&pool)
    .await
    .expect_err("zero is not a domain ordinal");
    assert_eq!(
        zero_version
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let invalid_selection_shape = sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000013', 2, 'direct',
             '70000000-0000-7000-8000-000000000016',
             '70000000-0000-7000-8000-000000000017')",
    )
    .execute(&pool)
    .await
    .expect_err("a typed selection must have exactly one matching UUID");
    assert_eq!(
        invalid_selection_shape
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let missing_current_version = sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 2
         WHERE session_id = '70000000-0000-7000-8000-000000000013'",
    )
    .execute(&pool)
    .await
    .expect_err("the current pointer must reference an existing version");
    assert_eq!(
        missing_current_version
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000013',
             18446744073709551615, 'direct',
             '70000000-0000-7000-8000-000000000018', NULL)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 18446744073709551615
         WHERE session_id = '70000000-0000-7000-8000-000000000013'",
    )
    .execute(&pool)
    .await?;

    let out_of_range_version = sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000013',
             18446744073709551616, 'direct',
             '70000000-0000-7000-8000-000000000019', NULL)",
    )
    .execute(&pool)
    .await
    .expect_err("an ordinal above u64::MAX must be rejected");
    assert_eq!(
        out_of_range_version
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let immutable_session = sqlx::query(
        "UPDATE session
         SET ancestry_kind = 'none'
         WHERE session_id = '70000000-0000-7000-8000-000000000013'",
    )
    .execute(&pool)
    .await
    .expect_err("session provenance is immutable");
    assert_eq!(
        immutable_session
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);

    Ok(())
}

/// S01 / INV-012: first handling commits the complete typed creation, equal
/// replay returns the recorded identity, and structural conflict changes
/// nothing. Direct and alias defaults round-trip through reconstitution.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv012_transaction_apply_replay_conflict_and_restart() -> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let repository = CreateSessionRepository::new(pool.clone());
    let first = prepared(0x101, 0x701, direct(0x801));

    assert_eq!(
        repository.handle(first).await?,
        CreateSessionHandlingOutcome::Applied(first.applied_result())
    );

    let replay_candidate = prepared(0x101, 0x702, direct(0x801));
    assert_eq!(
        repository.handle(replay_candidate).await?,
        CreateSessionHandlingOutcome::Applied(first.applied_result())
    );

    let conflicting = prepared(0x101, 0x703, alias(0x802));
    assert_eq!(
        repository.handle(conflicting).await?,
        CreateSessionHandlingOutcome::ConflictingReuse {
            command_id: first.command().command_id()
        }
    );

    let separate = prepared(0x102, 0x704, direct(0x801));
    let alias_creation = prepared(0x103, 0x705, alias(0x803));
    assert_eq!(
        repository.handle(separate).await?,
        CreateSessionHandlingOutcome::Applied(separate.applied_result())
    );
    assert_eq!(
        repository.handle(alias_creation).await?,
        CreateSessionHandlingOutcome::Applied(alias_creation.applied_result())
    );
    let loaded_alias = repository
        .load(alias_creation.command().command_id())
        .await?
        .expect("the applied alias creation must load");
    assert_eq!(loaded_alias.command(), alias_creation.command());
    assert_eq!(
        loaded_alias.applied_result(),
        alias_creation.applied_result()
    );

    let counts: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM durable_command),
            (SELECT count(*) FROM create_session_command),
            (SELECT count(*) FROM session),
            (SELECT count(*) FROM session_defaults_version),
            (SELECT count(*) FROM session_current_defaults)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (3, 3, 3, 3, 3));

    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let restarted = CreateSessionRepository::new(restarted_pool.clone());
    let reconstituted = restarted
        .load(first.command().command_id())
        .await?
        .expect("committed creation must survive a new pool");
    assert_eq!(reconstituted.command(), first.command());
    assert_eq!(reconstituted.session().id(), first.session().id());
    assert_eq!(reconstituted.applied_result(), first.applied_result());

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-012: the owner-global primary key is the concurrency boundary.
/// Equal duplicates return one winner; unequal duplicates retain that winner
/// and report one typed conflict.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv012_concurrent_duplicates_converge_on_the_committed_winner()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = CreateSessionRepository::new(pool.clone());

    let equal_left = prepared(0x111, 0x711, direct(0x811));
    let equal_right = prepared(0x111, 0x712, direct(0x811));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (left, right) = tokio::join!(
        async {
            barrier.wait().await;
            repository.handle(equal_left).await
        },
        async {
            barrier.wait().await;
            repository.handle(equal_right).await
        }
    );
    let (left, right) = (left?, right?);
    let (
        CreateSessionHandlingOutcome::Applied(left_result),
        CreateSessionHandlingOutcome::Applied(right_result),
    ) = (left, right)
    else {
        panic!("equal duplicates must both return the recorded applied result");
    };
    assert_eq!(left_result, right_result);

    let conflict_left = prepared(0x112, 0x713, direct(0x812));
    let conflict_right = prepared(0x112, 0x714, alias(0x813));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (left, right) = tokio::join!(
        async {
            barrier.wait().await;
            repository.handle(conflict_left).await
        },
        async {
            barrier.wait().await;
            repository.handle(conflict_right).await
        }
    );
    let outcomes = [left?, right?];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, CreateSessionHandlingOutcome::Applied(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(
                outcome,
                CreateSessionHandlingOutcome::ConflictingReuse { .. }
            ))
            .count(),
        1
    );

    let counts: (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM durable_command),
            (SELECT count(*) FROM session)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (2, 2));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-012: a later write failure rolls back the provisional registry
/// insert, so the same command ID remains available for a valid retry.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv012_infrastructure_failure_leaves_the_command_unclaimed() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = CreateSessionRepository::new(pool.clone());
    let existing = prepared(0x121, 0x721, direct(0x821));
    repository.handle(existing).await?;

    let colliding = prepared(0x122, 0x721, direct(0x822));
    let error = repository
        .handle(colliding)
        .await
        .expect_err("the session identity collision must abort first handling");
    assert!(matches!(error, CreateSessionRepositoryError::Database(_)));
    assert!(
        repository
            .load(colliding.command().command_id())
            .await?
            .is_none()
    );

    let retry = prepared(0x122, 0x722, direct(0x822));
    assert_eq!(
        repository.handle(retry).await?,
        CreateSessionHandlingOutcome::Applied(retry.applied_result())
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-012: an observed owner-global claim is never treated as unseen merely
/// because its typed record is missing or its storage version is unknown.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv012_incomplete_or_unknown_claims_fail_closed_as_corruption()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let defaults_repository = ReplaceSessionDefaultsRepository::new(pool.clone());
    let input_repository = SubmitInputRepository::new(pool.clone());
    let cross_wired = replacement(0x135, 0x735, 1, direct(0x835));
    defaults_repository.handle(cross_wired).await?;

    sqlx::query(
        "DROP TRIGGER durable_command_requires_typed_record
         ON durable_command",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE durable_command
         DROP CONSTRAINT durable_command_storage_version_supported",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000131',
             'create_session', 1, transaction_timestamp()),
            ('10000000-0000-4000-8000-000000000132',
             'create_session', 99, transaction_timestamp()),
            ('10000000-0000-4000-8000-000000000133',
             'replace_session_defaults', 1, transaction_timestamp()),
            ('10000000-0000-4000-8000-000000000134',
             'replace_session_defaults', 99, transaction_timestamp()),
            ('10000000-0000-4000-8000-000000000135',
             'submit_input', 1, transaction_timestamp()),
            ('10000000-0000-4000-8000-000000000136',
             'submit_input', 99, transaction_timestamp())",
    )
    .execute(&pool)
    .await?;

    let repository = CreateSessionRepository::new(pool.clone());
    let missing_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000131")?);
    let missing = repository
        .load(missing_id)
        .await
        .expect_err("a claimed identifier without its typed record is corruption");
    assert!(matches!(
        missing,
        CreateSessionRepositoryError::Corruption(CreateSessionCorruption::Missing(
            "typed_command_id"
        ))
    ));

    let unknown_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000132")?);
    let unknown = repository
        .load(unknown_id)
        .await
        .expect_err("an unknown representation version is corruption");
    assert!(matches!(
        unknown,
        CreateSessionRepositoryError::Corruption(CreateSessionCorruption::Unsupported {
            field: "registry_version",
            ..
        })
    ));

    let missing_defaults_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000133")?);
    let missing_defaults = defaults_repository
        .load(missing_defaults_id)
        .await
        .expect_err("an incomplete defaults claim is corruption");
    assert!(matches!(
        missing_defaults,
        ReplaceSessionDefaultsRepositoryError::Corruption(
            ReplaceSessionDefaultsCorruption::Missing("typed_command_id")
        )
    ));

    let unknown_defaults_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000134")?);
    let unknown_defaults = defaults_repository
        .load(unknown_defaults_id)
        .await
        .expect_err("an unknown defaults representation is corruption");
    assert!(matches!(
        unknown_defaults,
        ReplaceSessionDefaultsRepositoryError::Corruption(
            ReplaceSessionDefaultsCorruption::Unsupported {
                field: "registry_version",
                ..
            }
        )
    ));

    let missing_input_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000135")?);
    assert!(matches!(
        input_repository
            .load(missing_input_id)
            .await
            .expect_err("an incomplete input claim is corruption"),
        SubmitInputRepositoryError::Corruption(SubmitInputCorruption::Missing("typed_command_id"))
    ));
    let unknown_input_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000136")?);
    assert!(matches!(
        input_repository
            .load(unknown_input_id)
            .await
            .expect_err("an unknown input representation is corruption"),
        SubmitInputRepositoryError::Corruption(SubmitInputCorruption::Unsupported {
            field: "registry_version",
            ..
        })
    ));

    sqlx::query(
        "ALTER TABLE replace_session_defaults_command
         DROP CONSTRAINT replace_session_defaults_command_result_session_matches,
         DISABLE TRIGGER replace_session_defaults_command_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE replace_session_defaults_command
         SET result_session_id = $2
         WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x135))
    .bind(Uuid::from_u128(0x736))
    .execute(&pool)
    .await?;
    let inconsistent = defaults_repository
        .load(cross_wired.command_id())
        .await
        .expect_err("cross-wired typed result facts are corruption");
    assert!(matches!(
        inconsistent,
        ReplaceSessionDefaultsRepositoryError::Corruption(
            ReplaceSessionDefaultsCorruption::Domain(_)
        )
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-002 / INV-008 / INV-012: the second admitted command kind retains a
/// complete typed record, while the owner-global registry and append-only
/// constraints reject torn, malformed, or mutable receipts.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv002_inv008_inv012_defaults_schema_enforces_typed_receipts() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;

    let mut registry_only = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000201',
             'replace_session_defaults', 1, transaction_timestamp())",
    )
    .execute(&mut *registry_only)
    .await?;
    let torn = registry_only
        .commit()
        .await
        .expect_err("a defaults registry claim must have its exact typed record");
    assert_eq!(
        torn.as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    let mut typed_only = pool.begin().await?;
    sqlx::query(
        "INSERT INTO replace_session_defaults_command
            (command_id, command_kind, storage_version, session_id,
             expected_current_version, model_selection_kind,
             direct_model_selection_id, model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_installed_version, result_expected_version,
             result_current_version)
         VALUES
            ('10000000-0000-4000-8000-000000000204',
             'replace_session_defaults', 1,
             '70000000-0000-7000-8000-000000000204',
             1, 'direct',
             '70000000-0000-7000-8000-000000000205', NULL,
             'rejected', 'session_not_found',
             '70000000-0000-7000-8000-000000000204',
             NULL, NULL, NULL)",
    )
    .execute(&mut *typed_only)
    .await?;
    let missing_registry = typed_only
        .commit()
        .await
        .expect_err("a typed defaults record cannot commit without its registry claim");
    assert_eq!(
        missing_registry
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    let mut missing_installed = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000205',
             'replace_session_defaults', 1, transaction_timestamp())",
    )
    .execute(&mut *missing_installed)
    .await?;
    sqlx::query(
        "INSERT INTO replace_session_defaults_command
            (command_id, command_kind, storage_version, session_id,
             expected_current_version, model_selection_kind,
             direct_model_selection_id, model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_installed_version, result_expected_version,
             result_current_version)
         VALUES
            ('10000000-0000-4000-8000-000000000205',
             'replace_session_defaults', 1,
             '70000000-0000-7000-8000-000000000205',
             1, 'direct',
             '70000000-0000-7000-8000-000000000206', NULL,
             'applied', NULL,
             '70000000-0000-7000-8000-000000000205',
             2, NULL, NULL)",
    )
    .execute(&mut *missing_installed)
    .await?;
    let missing_exact_defaults = missing_installed
        .commit()
        .await
        .expect_err("an applied receipt requires its exact immutable installed defaults");
    assert_eq!(
        missing_exact_defaults
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    let malformed = sqlx::query(
        "INSERT INTO replace_session_defaults_command
            (command_id, command_kind, storage_version, session_id,
             expected_current_version, model_selection_kind,
             direct_model_selection_id, model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_installed_version, result_expected_version,
             result_current_version)
         VALUES
            ('10000000-0000-4000-8000-000000000202',
             'replace_session_defaults', 1,
             '70000000-0000-7000-8000-000000000202',
             1, 'direct',
             '70000000-0000-7000-8000-000000000203', NULL,
             'applied', NULL,
             '70000000-0000-7000-8000-000000000202',
             NULL, NULL, NULL)",
    )
    .execute(&pool)
    .await
    .expect_err("an applied result requires its typed installed version");
    assert_eq!(
        malformed
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let repository = ReplaceSessionDefaultsRepository::new(pool.clone());
    let absent = replacement(0x203, 0x703, 1, direct(0x803));
    assert!(matches!(
        repository.handle(absent).await?,
        ReplaceSessionDefaultsHandlingOutcome::Rejected(
            ReplaceSessionDefaultsRejectedResult::SessionNotFound(_)
        )
    ));
    let stored: (String, String, Option<String>) = sqlx::query_as(
        "SELECT result_kind, rejection_kind, result_installed_version::text
         FROM replace_session_defaults_command
         WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x203))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored,
        ("rejected".to_owned(), "session_not_found".to_owned(), None)
    );

    let immutable = sqlx::query(
        "UPDATE replace_session_defaults_command
         SET result_kind = result_kind
         WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x203))
    .execute(&pool)
    .await
    .expect_err("typed defaults receipts are append-only");
    assert_eq!(
        immutable
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    let immutable_delete = sqlx::query(
        "DELETE FROM replace_session_defaults_command
         WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x203))
    .execute(&pool)
    .await
    .expect_err("typed defaults receipts cannot be deleted");
    assert_eq!(
        immutable_delete
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-002 / INV-008 / INV-012: the application service through the
/// Postgres adapter records applied and stale outcomes, replays historical
/// receipts, and leaves creation history distinct from current Session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv002_inv008_inv012_defaults_apply_replay_stale_and_history()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository = CreateSessionRepository::new(pool.clone());
    let mut defaults_service =
        ReplaceSessionDefaultsService::new(ReplaceSessionDefaultsRepository::new(pool.clone()));
    let load_service = LoadSessionService::new(SessionRepository::new(pool.clone()));
    let creation = prepared(0x211, 0x711, direct(0x811));
    create_repository.handle(creation).await?;

    let first = replacement_request(0x212, 0x711, 1, alias(0x812));
    let first_outcome = defaults_service.execute(first).await?;
    let ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Applied(
        first_applied,
    )) = first_outcome
    else {
        panic!("the first replacement must apply");
    };
    assert_eq!(
        first_applied.installed().version(),
        SessionConfigurationDefaultsVersion::try_from_u64(2).expect("positive version")
    );
    assert_eq!(defaults_service.execute(first).await?, first_outcome);

    let conflict = replacement_request(0x212, 0x711, 1, direct(0x813));
    assert_eq!(
        defaults_service.execute(conflict).await?,
        ReplaceSessionDefaultsOutcome::ConflictingReuse {
            command_id: first.command_id()
        }
    );

    let stale = replacement_request(0x213, 0x711, 1, direct(0x814));
    let stale_outcome = defaults_service.execute(stale).await?;
    let ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Rejected(
        ReplaceSessionDefaultsRejectedResult::CurrentVersionMismatch(stale_result),
    )) = stale_outcome
    else {
        panic!("the unseen stale command must record a mismatch");
    };
    assert_eq!(
        stale_result.current(),
        SessionConfigurationDefaultsVersion::try_from_u64(2).expect("positive version")
    );

    let later = replacement_request(0x214, 0x711, 2, direct(0x815));
    assert!(matches!(
        defaults_service.execute(later).await?,
        ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Applied(_))
    ));

    assert_eq!(
        defaults_service.execute(first).await?,
        first_outcome,
        "historical applied replay must not require the mutable pointer"
    );
    assert_eq!(
        defaults_service.execute(stale).await?,
        stale_outcome,
        "recorded stale rejection must survive later state"
    );

    let current = load_service
        .execute(creation.session().id())
        .await?
        .expect("the session remains current");
    assert_eq!(
        current.current_configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::try_from_u64(3).expect("positive version")
    );
    assert_eq!(
        current.current_configuration_defaults().defaults().model(),
        direct(0x815)
    );

    let receipt = create_repository
        .load(creation.command().command_id())
        .await?
        .expect("creation history remains loadable");
    assert_eq!(
        receipt.session().configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::first()
    );
    assert_eq!(
        receipt
            .session()
            .configuration_defaults()
            .defaults()
            .model(),
        direct(0x811)
    );

    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM replace_session_defaults_command),
            (SELECT count(*) FROM session_defaults_version
              WHERE session_id = $1),
            (SELECT current_version::bigint FROM session_current_defaults
              WHERE session_id = $1)",
    )
    .bind(Uuid::from_u128(0x711))
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (3, 3, 3));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-012: registry dispatch remains owner-global across command kinds while
/// purpose-specific loads distinguish a valid other-kind claim from absence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv012_cross_kind_reuse_is_conflict_not_corruption_or_absence()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository = CreateSessionRepository::new(pool.clone());
    let defaults_repository = ReplaceSessionDefaultsRepository::new(pool.clone());
    let input_repository = SubmitInputRepository::new(pool.clone());
    let creation = prepared(0x221, 0x721, direct(0x821));
    create_repository.handle(creation).await?;

    let defaults_reuse = replacement(0x221, 0x721, 1, alias(0x822));
    assert_eq!(
        defaults_repository.handle(defaults_reuse).await?,
        ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse {
            command_id: defaults_reuse.command_id()
        }
    );
    assert!(matches!(
        defaults_repository
            .load(defaults_reuse.command_id())
            .await
            .expect_err("a CreateSession ID is not an unseen defaults receipt"),
        ReplaceSessionDefaultsRepositoryError::DifferentCommandKind { .. }
    ));
    let input_reuse = start_input(
        0x221,
        0x721,
        "cross-kind",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    assert_eq!(
        input_repository
            .handle(
                input_reuse.clone(),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x921)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa21))),
            )
            .await?,
        SubmitInputHandlingOutcome::ConflictingReuse {
            command_id: input_reuse.command_id(),
        }
    );
    assert!(matches!(
        input_repository
            .load(input_reuse.command_id())
            .await
            .expect_err("a CreateSession ID is not an unseen input receipt"),
        SubmitInputRepositoryError::DifferentCommandKind { .. }
    ));

    let defaults = replacement(0x222, 0x721, 1, alias(0x823));
    defaults_repository.handle(defaults).await?;
    let create_reuse = prepared(0x222, 0x722, direct(0x824));
    assert_eq!(
        create_repository.handle(create_reuse).await?,
        CreateSessionHandlingOutcome::ConflictingReuse {
            command_id: create_reuse.command().command_id()
        }
    );
    assert!(matches!(
        create_repository
            .load(defaults.command_id())
            .await
            .expect_err("a defaults ID is not an unseen creation receipt"),
        CreateSessionRepositoryError::DifferentCommandKind { .. }
    ));

    let input = start_input(
        0x223,
        0x721,
        "input winner",
        2,
        ModelSelectionOverride::ReplaceWith(direct(0x825)),
    );
    input_repository
        .handle(
            input.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x923)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa23))),
        )
        .await?;
    let defaults_reuse = replacement(0x223, 0x721, 2, direct(0x826));
    assert_eq!(
        defaults_repository.handle(defaults_reuse).await?,
        ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse {
            command_id: input.command_id(),
        }
    );
    let create_reuse = prepared(0x223, 0x723, direct(0x827));
    assert_eq!(
        create_repository.handle(create_reuse).await?,
        CreateSessionHandlingOutcome::ConflictingReuse {
            command_id: input.command_id(),
        }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-008 / INV-012: two application-service calls expecting one version use
/// the adapter's pointer CAS as their linearization boundary. Exactly one
/// installs the successor and the loser records the winner's version as stale.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv008_inv012_concurrent_defaults_replacements_have_one_winner()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository = CreateSessionRepository::new(pool.clone());
    create_repository
        .handle(prepared(0x231, 0x731, direct(0x831)))
        .await?;
    let mut left_service =
        ReplaceSessionDefaultsService::new(ReplaceSessionDefaultsRepository::new(pool.clone()));
    let mut right_service =
        ReplaceSessionDefaultsService::new(ReplaceSessionDefaultsRepository::new(pool.clone()));
    let left_command = replacement_request(0x232, 0x731, 1, direct(0x832));
    let right_command = replacement_request(0x233, 0x731, 1, alias(0x833));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    let (left, right) = tokio::join!(
        async {
            barrier.wait().await;
            left_service.execute(left_command).await
        },
        async {
            barrier.wait().await;
            right_service.execute(right_command).await
        }
    );
    let outcomes = [left?, right?];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(
                outcome,
                ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Applied(_))
            ))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(
                outcome,
                ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Rejected(
                    ReplaceSessionDefaultsRejectedResult::CurrentVersionMismatch(_)
                ))
            ))
            .count(),
        1
    );

    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM replace_session_defaults_command
              WHERE session_id = $1),
            (SELECT count(*) FROM session_defaults_version
              WHERE session_id = $1 AND version = 2),
            (SELECT current_version::bigint FROM session_current_defaults
              WHERE session_id = $1)",
    )
    .bind(Uuid::from_u128(0x731))
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (2, 1, 2));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-008 / INV-012: exhausted versions are recorded rejections, while an
/// infrastructure failure after provisional claim rolls back both the claim
/// and the attempted pointer change.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv008_inv012_exhaustion_and_precommit_failure_are_distinct() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository = CreateSessionRepository::new(pool.clone());
    let defaults_repository = ReplaceSessionDefaultsRepository::new(pool.clone());
    create_repository
        .handle(prepared(0x241, 0x741, direct(0x841)))
        .await?;
    create_repository
        .handle(prepared(0x242, 0x742, direct(0x842)))
        .await?;

    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, 18446744073709551615, 'direct', $2, NULL)",
    )
    .bind(Uuid::from_u128(0x741))
    .bind(Uuid::from_u128(0x843))
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 18446744073709551615
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x741))
    .execute(&pool)
    .await?;
    let exhausted = replacement(0x243, 0x741, u64::MAX, alias(0x844));
    let exhausted_outcome = defaults_repository.handle(exhausted).await?;
    assert!(matches!(
        exhausted_outcome,
        ReplaceSessionDefaultsHandlingOutcome::Rejected(
            ReplaceSessionDefaultsRejectedResult::VersionExhausted(_)
        )
    ));
    assert_eq!(
        defaults_repository.handle(exhausted).await?,
        exhausted_outcome
    );

    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, 2, 'direct', $2, NULL)",
    )
    .bind(Uuid::from_u128(0x742))
    .bind(Uuid::from_u128(0x845))
    .execute(&pool)
    .await?;
    let fails_after_claim = replacement_request(0x244, 0x742, 1, alias(0x846));
    let mut failing_service = ReplaceSessionDefaultsService::new(defaults_repository.clone());
    assert!(matches!(
        failing_service
            .execute(fails_after_claim)
            .await
            .expect_err("the colliding immutable successor aborts the transaction"),
        ReplaceSessionDefaultsRepositoryError::Database(_)
    ));
    assert!(
        defaults_repository
            .load(fails_after_claim.command_id())
            .await?
            .is_none(),
        "the failed transaction must not claim the command ID"
    );
    let pointer: i64 = sqlx::query_scalar(
        "SELECT current_version::bigint
         FROM session_current_defaults
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x742))
    .fetch_one(&pool)
    .await?;
    assert_eq!(pointer, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-003 / INV-008 / INV-012: load-by-session identity returns the
/// complete version selected by the current pointer, while creation receipt
/// replay remains pinned to the immutable creation-time version.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv003_inv008_inv012_current_session_load_and_receipt_replay_remain_distinct()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository = CreateSessionRepository::new(pool.clone());
    let session_repository = SessionRepository::new(pool.clone());
    let direct_creation = prepared(0x501, 0x901, direct(0x801));
    let alias_creation = prepared(0x502, 0x902, alias(0x802));

    assert!(
        session_repository
            .load_session(SessionId::from_uuid(Uuid::from_u128(0x999)))
            .await?
            .is_none(),
        "only an absent session row is a not-found result"
    );
    assert_eq!(
        create_repository.handle(direct_creation).await?,
        CreateSessionHandlingOutcome::Applied(direct_creation.applied_result())
    );
    assert_eq!(
        create_repository.handle(alias_creation).await?,
        CreateSessionHandlingOutcome::Applied(alias_creation.applied_result())
    );

    let loaded_direct = session_repository
        .load_session(direct_creation.session().id())
        .await?
        .expect("the committed direct session must load");
    assert_eq!(loaded_direct.id(), direct_creation.session().id());
    assert_eq!(
        loaded_direct.creation_provenance(),
        direct_creation.session().provenance()
    );
    assert_eq!(
        loaded_direct.current_configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::first()
    );
    assert_eq!(
        loaded_direct
            .current_configuration_defaults()
            .defaults()
            .model(),
        direct(0x801)
    );

    let loaded_alias = session_repository
        .load_session(alias_creation.session().id())
        .await?
        .expect("the committed alias session must load");
    assert_eq!(
        loaded_alias
            .current_configuration_defaults()
            .defaults()
            .model(),
        alias(0x802)
    );

    let direct_session_id = Uuid::from_u128(0x901);
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, 2, 'alias', NULL, $2)",
    )
    .bind(direct_session_id)
    .bind(Uuid::from_u128(0x803))
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 2
         WHERE session_id = $1",
    )
    .bind(direct_session_id)
    .execute(&pool)
    .await?;

    let current = session_repository
        .load_session(direct_creation.session().id())
        .await
        .expect("the advanced current session load must succeed")
        .expect("the session row remains present");
    assert_eq!(
        current.current_configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::try_from_u64(2)
            .expect("two is a positive defaults version")
    );
    assert_eq!(
        current.current_configuration_defaults().defaults().model(),
        alias(0x803)
    );

    let receipt = create_repository
        .load(direct_creation.command().command_id())
        .await?
        .expect("creation receipt remains loadable after current defaults advance");
    assert_eq!(receipt.command(), direct_creation.command());
    assert_eq!(
        receipt.session().configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::first()
    );
    assert_eq!(
        receipt
            .session()
            .configuration_defaults()
            .defaults()
            .model(),
        direct(0x801)
    );

    let replay_candidate = prepared(0x501, 0x903, direct(0x801));
    assert_eq!(
        create_repository.handle(replay_candidate).await?,
        CreateSessionHandlingOutcome::Applied(direct_creation.applied_result())
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-002 / INV-003 / INV-008: once the session row exists, absent,
/// malformed, unknown, undecodable, or non-unique current projection facts fail
/// closed as typed corruption rather than becoming `None` or nearby valid
/// defaults.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv002_inv003_inv008_current_session_corruption_fails_closed() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository = CreateSessionRepository::new(pool.clone());
    let session_repository = SessionRepository::new(pool.clone());
    let missing_pointer = prepared(0x511, 0x911, direct(0x811));
    let invalid_pointer = prepared(0x512, 0x912, direct(0x812));
    let missing_selected = prepared(0x513, 0x913, direct(0x813));
    let malformed_selected = prepared(0x514, 0x914, direct(0x814));
    let unknown_provenance = prepared(0x515, 0x915, direct(0x815));
    let duplicate_projection = prepared(0x516, 0x916, direct(0x816));
    for creation in [
        missing_pointer,
        invalid_pointer,
        missing_selected,
        malformed_selected,
        unknown_provenance,
        duplicate_projection,
    ] {
        create_repository.handle(creation).await?;
    }

    sqlx::query(
        "ALTER TABLE session
         DROP CONSTRAINT session_current_defaults_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM session_current_defaults
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x911))
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE session_current_defaults
         DROP CONSTRAINT session_current_defaults_version_fk,
         DROP CONSTRAINT session_current_defaults_version_positive_u64",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 0
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x912))
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 2
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x913))
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE session_defaults_version
         DROP CONSTRAINT session_defaults_version_model_selection_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, 2, 'direct', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0x914))
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 2
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x914))
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE create_session_command
         DROP CONSTRAINT create_session_command_provenance_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session
         DROP CONSTRAINT session_creation_cause_closed,
         DISABLE TRIGGER session_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session
         SET creation_cause = 'unknown'
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x915))
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE session_current_defaults
         DROP CONSTRAINT session_current_defaults_pkey",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ($1, 1)",
    )
    .bind(Uuid::from_u128(0x916))
    .execute(&pool)
    .await?;

    let missing = session_repository
        .load_session(missing_pointer.session().id())
        .await
        .expect_err("a missing pointer is corruption");
    assert!(matches!(
        missing,
        SessionRepositoryError::Corruption(SessionCorruption::Missing(
            "current_defaults_session_id"
        ))
    ));

    let invalid = session_repository
        .load_session(invalid_pointer.session().id())
        .await
        .expect_err("a non-positive pointer version is corruption");
    assert!(matches!(
        invalid,
        SessionRepositoryError::Corruption(SessionCorruption::InvalidOrdinal {
            field: "current_version",
            ..
        })
    ));

    let missing_selected_row = session_repository
        .load_session(missing_selected.session().id())
        .await
        .expect_err("a missing selected defaults row is corruption");
    assert!(matches!(
        missing_selected_row,
        SessionRepositoryError::Corruption(SessionCorruption::Missing(
            "selected_defaults_session_id"
        ))
    ));

    let malformed = session_repository
        .load_session(malformed_selected.session().id())
        .await
        .expect_err("a malformed selected defaults record is corruption");
    assert!(matches!(
        malformed,
        SessionRepositoryError::Corruption(SessionCorruption::Inconsistent("model selection"))
    ));

    let unknown = session_repository
        .load_session(unknown_provenance.session().id())
        .await
        .expect_err("an unknown creation cause is corruption");
    assert!(matches!(
        unknown,
        SessionRepositoryError::Corruption(SessionCorruption::Unsupported {
            field: "creation cause",
            ..
        })
    ));

    let duplicate = session_repository
        .load_session(duplicate_projection.session().id())
        .await
        .expect_err("more than one current projection row is corruption");
    assert!(matches!(
        duplicate,
        SessionRepositoryError::Corruption(SessionCorruption::Inconsistent(
            "current session projection cardinality"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-002 / INV-007 / INV-008 / INV-012: the third command family is a
/// normalized closed schema whose deferred reverse and effect constraints
/// reject a claim without its typed terminal record.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv002_inv007_inv008_inv012_submit_schema_is_closed_and_normalized()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name
           FROM information_schema.tables
          WHERE table_schema = 'public'
            AND table_name IN (
                'submit_input_command',
                'accepted_input',
                'queued_input_origin'
            )
          ORDER BY table_name",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        tables,
        vec![
            "accepted_input".to_owned(),
            "queued_input_origin".to_owned(),
            "submit_input_command".to_owned(),
        ]
    );

    let constraints: Vec<String> = sqlx::query_scalar(
        "SELECT conname
           FROM pg_constraint
          WHERE conname IN (
                'submit_input_command_applied_effect_fk',
                'submit_input_command_last_position_fk',
                'submit_input_command_current_defaults_fk',
                'submit_input_command_selected_defaults_fk',
                'submit_input_command_actor_shape',
                'submit_input_command_delivery_shape',
                'submit_input_command_result_shape',
                'accepted_input_queued_origin_fk',
                'queued_input_origin_accepted_input_fk'
          )
          ORDER BY conname",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(constraints.len(), 9);

    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'submit_input', 1, transaction_timestamp())",
    )
    .bind(Uuid::from_u128(0x3ff))
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a registry claim without its typed SubmitInput record must not commit");
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some("23503".into())
    );

    let command = start_input(
        0x3fe,
        0x7fe,
        "immutable",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    SubmitInputRepository::new(pool.clone())
        .handle(
            command,
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9fe)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xafe))),
        )
        .await?;
    let error = sqlx::query(
        "UPDATE submit_input_command
            SET content_text = 'mutated'
          WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x3fe))
    .execute(&pool)
    .await
    .expect_err("typed SubmitInput records are append-only");
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some("23514".into())
    );

    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x3fc, 0x7fc, direct(0x8fc)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x3fd,
                0x7fc,
                "complete source",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9fd)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xafd))),
        )
        .await?;

    let source_command_id = Uuid::from_u128(0x3fd);
    let malformed_rejections = [
        (
            Uuid::from_u128(0x3fa),
            "no_active_turn",
            None,
            None,
            None,
            None,
            None,
            None,
        ),
        (
            Uuid::from_u128(0x3f9),
            "session_defaults_version_mismatch",
            None,
            None,
            Some(Decimal::ONE),
            None,
            None,
            None,
        ),
        (
            Uuid::from_u128(0x3f8),
            "unknown_model_alias",
            None,
            None,
            None,
            Some(Uuid::from_u128(0x8f8)),
            None,
            None,
        ),
        (
            Uuid::from_u128(0x3f7),
            "acceptance_position_exhausted",
            None,
            None,
            None,
            None,
            None,
            None,
        ),
    ];
    for (
        command_id,
        rejection_kind,
        expected_turn,
        expected_defaults,
        current_defaults,
        unknown_alias,
        selected_defaults,
        last_position,
    ) in malformed_rejections
    {
        let error = insert_malformed_submit_rejection(
            &pool,
            command_id,
            source_command_id,
            rejection_kind,
            expected_turn,
            expected_defaults,
            current_defaults,
            unknown_alias,
            selected_defaults,
            last_position,
        )
        .await
        .expect_err(rejection_kind);
        assert_eq!(
            error.as_database_error().and_then(|error| error.code()),
            Some("23514".into())
        );
    }

    let error = insert_malformed_submit_rejection(
        &pool,
        Uuid::from_u128(0x3f6),
        source_command_id,
        "acceptance_position_exhausted",
        None,
        None,
        None,
        None,
        None,
        Some(Decimal::from(u64::MAX)),
    )
    .await
    .expect_err("exhaustion must reference the session's actual maximum-position input");
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some("23503".into())
    );

    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'submit_input', 1, transaction_timestamp())",
    )
    .bind(Uuid::from_u128(0x3fb))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position)
         SELECT
             $1, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             $2, $3,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position
           FROM submit_input_command
          WHERE command_id = $4",
    )
    .bind(Uuid::from_u128(0x3fb))
    .bind(Uuid::from_u128(0x9fb))
    .bind(Uuid::from_u128(0xafb))
    .bind(Uuid::from_u128(0x3fd))
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("an applied typed receipt without its exact effects must not commit");
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some("23503".into())
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Decision log 2026-07-20: the provisional one-mebibyte accepted-input
/// content bound is one contract enforced at correlated layers — oversized
/// text fails application admission before the typed command and never reaches SQL,
/// exact-bound text commits through the real adapter, and a direct SQL
/// insert of oversized content is refused by the schema checks.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn content_size_bound_rejects_oversized_text_at_application_and_schema()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let oversized = UserContent::try_text("a".repeat(1_048_577))
        .expect("domain text is intentionally unbounded");
    let error = SubmitInputRequest::try_new(
        DurableCommandId::from_uuid(Uuid::from_u128(0x320)),
        SessionId::from_uuid(Uuid::from_u128(0x720)),
        oversized,
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    )
    .expect_err("text over the provisional bound fails application admission");
    assert_eq!(
        error,
        SubmitInputRequestError::OversizedContent {
            utf8_byte_length: 1_048_577,
        }
    );
    let claimed: i64 = sqlx::query_scalar("SELECT count(*) FROM durable_command")
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        claimed, 0,
        "content rejected before typed-command construction claims no durable identifier"
    );

    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x321, 0x721, direct(0x821)))
        .await?;
    let at_bound = "a".repeat(1_048_576);
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x322,
                0x721,
                &at_bound,
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x921)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa21))),
        )
        .await?;
    let stored_lengths: Vec<i32> = sqlx::query_scalar(
        "SELECT octet_length(content_text) FROM submit_input_command
         UNION ALL
         SELECT octet_length(content_text) FROM accepted_input",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        stored_lengths,
        vec![1_048_576, 1_048_576],
        "the schema must admit the domain's exact maximum"
    );

    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'submit_input', 1, transaction_timestamp())",
    )
    .bind(Uuid::from_u128(0x323))
    .execute(&mut *transaction)
    .await?;
    let command_error = sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position)
         SELECT
             $1, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text || 'a', delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position
           FROM submit_input_command
          WHERE command_id = $2",
    )
    .bind(Uuid::from_u128(0x323))
    .bind(Uuid::from_u128(0x322))
    .execute(&mut *transaction)
    .await
    .expect_err("the schema refuses command content one byte over the bound");
    let database_error = command_error
        .as_database_error()
        .expect("a check violation is a database error");
    assert_eq!(database_error.code(), Some("23514".into()));
    assert_eq!(
        database_error.constraint(),
        Some("submit_input_command_content_bounded")
    );
    transaction.rollback().await?;

    let mut transaction = pool.begin().await?;
    let accepted_error = sqlx::query(
        "INSERT INTO accepted_input
            (accepted_input_id, accepting_command_id, session_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             acceptance_position, disposition_kind, origin_turn_id)
         SELECT
             $1, $2, session_id,
             content_kind, content_text || 'a', delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             $3, disposition_kind, $4
           FROM accepted_input
          WHERE accepted_input_id = $5",
    )
    .bind(Uuid::from_u128(0x922))
    .bind(Uuid::from_u128(0x323))
    .bind(Decimal::TWO)
    .bind(Uuid::from_u128(0xa22))
    .bind(Uuid::from_u128(0x921))
    .execute(&mut *transaction)
    .await
    .expect_err("the schema refuses accepted content one byte over the bound");
    let database_error = accepted_error
        .as_database_error()
        .expect("a check violation is a database error");
    assert_eq!(database_error.code(), Some("23514".into()));
    assert_eq!(
        database_error.constraint(),
        Some("accepted_input_content_bounded")
    );
    transaction.rollback().await?;

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-005 / INV-008 / INV-010 / INV-012 / INV-028: first acceptance
/// commits the complete exact receipt and immutable queued origin; equal
/// replay and a restarted adapter return that receipt without consulting new
/// candidates.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv005_inv008_inv010_inv012_inv028_submit_apply_replay_conflict_and_restart()
-> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x301, 0x701, direct(0x801)))
        .await?;
    let exact = " \tline one\r\ncafe\u{301}\n ";
    let command = start_input(
        0x302,
        0x701,
        exact,
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let accepted = AcceptedInputId::from_uuid(Uuid::from_u128(0x901));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xa01));
    let request = SubmitInputRequest::try_new(
        command.command_id(),
        command.session(),
        command.content().clone(),
        command.delivery(),
    )?;
    let mut service = SubmitInputService::new(
        FixedSubmitInputIds::new(
            [
                accepted,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x902)),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x903)),
            ],
            [
                turn,
                TurnId::from_uuid(Uuid::from_u128(0xa02)),
                TurnId::from_uuid(Uuid::from_u128(0xa03)),
            ],
        ),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
    );

    let first = service.execute(request.clone()).await?;
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(applied),
    )) = first.clone()
    else {
        panic!("no-active-turn start must apply");
    };
    assert_eq!(applied.accepted_input(), accepted);
    assert_eq!(applied.turn(), turn);
    assert_eq!(applied.acceptance_position().as_u64(), 1);
    assert_eq!(service.execute(request.clone()).await?, first);

    let conflicting = SubmitInputRequest::try_new(
        command.command_id(),
        command.session(),
        UserContent::try_text("different".to_owned())
            .expect("conflicting test content is admitted"),
        command.delivery(),
    )?;
    assert_eq!(
        service.execute(conflicting).await?,
        SubmitInputOutcome::ConflictingReuse {
            command_id: command.command_id(),
        }
    );

    let stored: (String, String, String, i64, String) = sqlx::query_as(
        "SELECT typed.content_text, accepted.content_text, queued.priority_kind,
                queued.acceptance_position::bigint, turn.state_kind
           FROM submit_input_command AS typed
           JOIN accepted_input AS accepted
             ON accepted.accepting_command_id = typed.command_id
           JOIN queued_input_origin AS queued
             ON queued.accepted_input_id = accepted.accepted_input_id
           JOIN turn_lifecycle AS turn
             ON turn.turn_id = queued.turn_id
          WHERE typed.command_id = $1",
    )
    .bind(Uuid::from_u128(0x302))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored,
        (
            exact.to_owned(),
            exact.to_owned(),
            "ordinary".into(),
            1,
            "queued".into()
        )
    );

    drop(service);
    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let restarted = SubmitInputRepository::new(restarted_pool.clone());
    let mut restarted_service = SubmitInputService::new(
        FixedSubmitInputIds::new(
            [AcceptedInputId::from_uuid(Uuid::from_u128(0x904))],
            [TurnId::from_uuid(Uuid::from_u128(0xa04))],
        ),
        restarted.clone(),
        AcceptingEligibilityNudge,
    );
    let loaded = restarted
        .load(command.command_id())
        .await?
        .expect("the committed receipt survives adapter restart");
    assert_eq!(loaded.command(), &command);
    assert_eq!(restarted_service.execute(request).await?, first);
    let effect_counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM submit_input_command WHERE command_id = $1),
            (SELECT count(*) FROM accepted_input WHERE accepting_command_id = $1),
            (SELECT count(*)
               FROM queued_input_origin AS queued
               JOIN accepted_input AS accepted
                 ON accepted.accepted_input_id = queued.accepted_input_id
              WHERE accepted.accepting_command_id = $1),
            (SELECT count(*)
               FROM turn_lifecycle AS turn
               JOIN accepted_input AS accepted
                 ON accepted.origin_turn_id = turn.turn_id
              WHERE accepted.accepting_command_id = $1)",
    )
    .bind(Uuid::from_u128(0x302))
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(effect_counts, (1, 1, 1, 1));

    drop(restarted_service);
    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / S03 / INV-002 / INV-009 / INV-015: the real application service
/// commits one complete activation, and a fresh repository and pool observe
/// the same occupied slot after restart without activating it again.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_s03_inv002_inv009_inv015_start_eligible_turn_survives_restart()
-> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x381, 0x781, direct(0x881)))
        .await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x781));
    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x981));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xa81));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x382,
                0x781,
                "restart-boundary activation",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            accepted_input,
            Some(turn),
        )
        .await?;

    let origin_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd81));
    let starting_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xe81));
    let initial_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xb81));
    let mut service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new([origin_entry], [starting_frontier], [initial_attempt]),
        StartEligibleTurnRepository::new(pool.clone()),
    );

    let StartEligibleTurnOutcome::Activated(activated) = service.execute(session).await? else {
        panic!("the sole queued turn must activate");
    };
    assert_eq!(activated.session(), session);
    assert_eq!(activated.turn(), turn);
    assert_eq!(activated.accepted_input().id(), accepted_input);
    assert_eq!(
        activated.start().lineage(),
        AcceptedInputStartingLineage::FirstInSession
    );
    assert_eq!(activated.start().frontier().snapshot(), starting_frontier);
    let ActiveTurnPhase::Running { current_attempt } = activated.phase() else {
        panic!("initial activation must return the running phase");
    };
    assert_eq!(current_attempt.id(), initial_attempt);
    assert_eq!(current_attempt.state(), &CurrentTurnAttemptState::Prepared);

    let stored: (String, String, String, Uuid, i64, i64, i64) = sqlx::query_as(
        "SELECT
            turn.state_kind,
            turn.active_phase_kind,
            attempt.state_kind,
            turn.current_attempt_id,
            frontier.member_count::bigint,
            (SELECT count(*)
               FROM turn_lifecycle AS active
              WHERE active.session_id = turn.session_id
                AND active.state_kind = 'active'),
            (SELECT count(*)
               FROM session_scheduler AS scheduler
              WHERE scheduler.session_id = turn.session_id)
         FROM turn_lifecycle AS turn
         JOIN turn_attempt AS attempt
           ON attempt.turn_attempt_id = turn.current_attempt_id
         JOIN context_frontier AS frontier
           ON frontier.owning_session_id = turn.session_id
          AND frontier.context_frontier_id = turn.starting_frontier_id
        WHERE turn.turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored,
        (
            "active".into(),
            "running".into(),
            "prepared".into(),
            initial_attempt.into_uuid(),
            1,
            1,
            1,
        )
    );

    drop(service);
    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let mut restarted_service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd82))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xe82))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xb82))],
        ),
        StartEligibleTurnRepository::new(restarted_pool.clone()),
    );
    assert_eq!(
        restarted_service.execute(session).await?,
        StartEligibleTurnOutcome::NoEligibleTurn
    );
    let persisted: (String, Uuid, i64, i64, i64) = sqlx::query_as(
        "SELECT
            state_kind,
            current_attempt_id,
            (SELECT count(*) FROM semantic_transcript_entry),
            (SELECT count(*) FROM context_frontier),
            (SELECT count(*) FROM turn_attempt)
         FROM turn_lifecycle
        WHERE turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        persisted,
        ("active".into(), initial_attempt.into_uuid(), 1, 1, 1,)
    );

    drop(restarted_service);
    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S03 / INV-007 / INV-009: the Postgres safety-net sweep finds durable queued
/// work without an active slot and excludes sessions already being progressed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s03_inv007_inv009_postgres_sweep_reconstructs_only_candidate_sessions()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x389, 0x789, direct(0x889)))
        .await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x38a, 0x78a, direct(0x88a)))
        .await?;
    let queued_session = SessionId::from_uuid(Uuid::from_u128(0x789));
    let active_session = SessionId::from_uuid(Uuid::from_u128(0x78a));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x38b,
                0x789,
                "queued sweep candidate",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x989)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa89))),
        )
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x38c,
                0x78a,
                "active sweep exclusion",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x98a)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa8a))),
        )
        .await?;
    let mut activation = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd8a))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xe8a))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xb8a))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert!(matches!(
        activation.execute(active_session).await?,
        StartEligibleTurnOutcome::Activated(_)
    ));

    let mut sweep = PostgresEligibilitySweep::new(pool.clone());
    let (candidates, continuation) = EligibilitySweep::find_sessions(&mut sweep)
        .await?
        .into_parts();
    assert!(!continuation);
    let queued_index_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)
           FROM pg_indexes
          WHERE schemaname = current_schema()
            AND tablename = 'turn_lifecycle'
            AND indexname = 'turn_lifecycle_queued_by_session'",
    )
    .fetch_one(&pool)
    .await?;

    assert_eq!(candidates, vec![queued_session]);
    assert_eq!(queued_index_count, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-009: scheduler-row locking serializes concurrent passes for one
/// session so exactly one service activates and the other observes the winner.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv009_concurrent_start_eligible_turn_passes_activate_once()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x391, 0x791, direct(0x891)))
        .await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x791));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x392,
                0x791,
                "concurrent activation",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x991)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa91))),
        )
        .await?;

    let mut first = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd91))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xe91))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xb91))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let mut second = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd92))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xe92))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xb92))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let (first_outcome, second_outcome) =
        tokio::join!(first.execute(session), second.execute(session));
    let first_outcome = first_outcome?;
    let second_outcome = second_outcome?;
    assert!(
        matches!(
            (&first_outcome, &second_outcome),
            (
                StartEligibleTurnOutcome::Activated(_),
                StartEligibleTurnOutcome::NoEligibleTurn
            ) | (
                StartEligibleTurnOutcome::NoEligibleTurn,
                StartEligibleTurnOutcome::Activated(_)
            )
        ),
        "unexpected concurrent outcomes: {first_outcome:?}, {second_outcome:?}"
    );

    let counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE session_id = $1 AND state_kind = 'active'),
            (SELECT count(*)
               FROM semantic_transcript_entry
              WHERE source_session_id = $1),
            (SELECT count(*)
               FROM context_frontier
              WHERE owning_session_id = $1),
            (SELECT count(*)
               FROM turn_attempt
              WHERE session_id = $1)",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (1, 1, 1, 1));

    pool.close().await;
    drop(container);
    Ok(())
}

/// Polls until exactly `expected` backends are lock-blocked behind another
/// backend, returning whether that count appeared within the polling budget.
/// The per-test database serves only this test's connections and each racer
/// is spawned only after the previous blocked count is observed, so spawn
/// order identifies the racers without matching their SQL text.
async fn blocked_backends_reached(pool: &PgPool, expected: i64) -> Result<bool, sqlx::Error> {
    for _ in 0..400 {
        let observed: i64 = sqlx::query_scalar(
            "SELECT count(*)
               FROM pg_stat_activity
              WHERE cardinality(pg_blocking_pids(pid)) > 0",
        )
        .fetch_one(pool)
        .await?;
        if observed == expected {
            return Ok(true);
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    Ok(false)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn blocked_backends_poll_reports_zero_for_an_idle_database() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    assert!(
        blocked_backends_reached(&pool, 0).await?,
        "an idle database has no lock-blocked backend"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn blocked_backends_poll_detects_one_scheduler_row_waiter() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x4e1, 0x8e1, direct(0xce1)))
        .await?;
    let mut holder = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
        .bind(Uuid::from_u128(0x8e1))
        .execute(&mut *holder)
        .await?;
    let waiter = tokio::spawn({
        let pool = pool.clone();
        async move {
            sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
                .bind(Uuid::from_u128(0x8e1))
                .execute(&pool)
                .await
        }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "one queued scheduler-row waiter must be detected"
    );

    holder.rollback().await?;
    waiter.await??;
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn blocked_backends_poll_reports_when_expected_count_never_forms()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x4e2, 0x8e2, direct(0xce2)))
        .await?;
    let mut holder = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
        .bind(Uuid::from_u128(0x8e2))
        .execute(&mut *holder)
        .await?;
    let waiter = tokio::spawn({
        let pool = pool.clone();
        async move {
            sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
                .bind(Uuid::from_u128(0x8e2))
                .execute(&pool)
                .await
        }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the fixture must establish its sole blocked waiter"
    );
    assert!(
        !blocked_backends_reached(&pool, 2).await?,
        "a second waiter never forms, so the poll must exhaust its budget and report false"
    );

    holder.rollback().await?;
    waiter.await??;
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn blocked_backends_poll_returns_to_zero_after_release() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x4e3, 0x8e3, direct(0xce3)))
        .await?;
    let mut holder = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
        .bind(Uuid::from_u128(0x8e3))
        .execute(&mut *holder)
        .await?;
    let waiter = tokio::spawn({
        let pool = pool.clone();
        async move {
            sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
                .bind(Uuid::from_u128(0x8e3))
                .execute(&pool)
                .await
        }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the fixture must establish a blocked waiter before releasing it"
    );
    holder.rollback().await?;
    waiter.await??;
    assert!(
        blocked_backends_reached(&pool, 0).await?,
        "the released waiter leaves no blocked backend"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-007 / INV-008 / INV-009 / INV-012: submit orders the session row
/// (`FOR NO KEY UPDATE`) before the scheduler row (`FOR UPDATE`), while
/// activation orders the scheduler row first and then requests `FOR KEY
/// SHARE` on the session row through its inserts' session foreign keys. The
/// forced overlap — the activation queued on the scheduler row first, the
/// submission verifiably holding its session-row lock while queued behind it
/// — completes with typed outcomes on both sides because referential
/// `KEY SHARE` does not conflict with submit's held session lock; a
/// session-row `FOR UPDATE` on the submit side would close this reverse
/// order into a deadlock (Postgres 40P01) surfacing as a `Database` error.
/// Postgres grants a contended row to its first queued waiter, so the
/// activation commits first and the unblocked submission records the typed
/// `ActiveTurnPresent` rejection naming the activated turn while its
/// candidate identities persist nothing. The sibling test queues the
/// submission ahead and pins the applied arm.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv007_inv008_inv009_inv012_submit_and_activation_interleave_without_deadlock()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x4b1, 0x8b1, direct(0xcb1)))
        .await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8b1));
    let queued_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9b1));
    let queued_turn = TurnId::from_uuid(Uuid::from_u128(0xab1));
    let racing_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9b2));
    let racing_turn = TurnId::from_uuid(Uuid::from_u128(0xab2));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x4b2,
                0x8b1,
                "eligible queued origin",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            queued_input,
            Some(queued_turn),
        )
        .await?;

    // Hold the scheduler row so both racers verifiably queue on it before
    // either proceeds: the activation pass blocks on it first, then the
    // submission takes its session-row lock and queues behind the activation.
    let mut scheduler_blocker = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
        .bind(Uuid::from_u128(0x8b1))
        .execute(&mut *scheduler_blocker)
        .await?;

    let activation = tokio::spawn({
        let mut service = StartEligibleTurnService::new(
            FixedStartEligibleTurnIds::new(
                [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdb1))],
                [ContextFrontierId::from_uuid(Uuid::from_u128(0xeb1))],
                [TurnAttemptId::from_uuid(Uuid::from_u128(0xbb1))],
            ),
            StartEligibleTurnRepository::new(pool.clone()),
        );
        async move { service.execute(session).await }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the eligibility pass must block on the held scheduler row"
    );

    let submission = tokio::spawn({
        let repository = SubmitInputRepository::new(pool.clone());
        async move {
            repository
                .handle(
                    start_input(
                        0x4b3,
                        0x8b1,
                        "racing start",
                        1,
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                    racing_input,
                    Some(racing_turn),
                )
                .await
        }
    });
    assert!(
        blocked_backends_reached(&pool, 2).await?,
        "the submission must hold its session row and queue behind the eligibility pass"
    );

    scheduler_blocker.rollback().await?;
    let activation_outcome = activation.await?.expect(
        "the activation side must serialize without deadlocking; a 40P01 surfaces here as a \
         Database error",
    );
    let submission_outcome = submission.await?.expect(
        "the submission side must serialize without deadlocking; a 40P01 surfaces here as a \
         Database error",
    );

    // The first-queued eligibility pass commits the sole queued origin.
    let StartEligibleTurnOutcome::Activated(activated) = activation_outcome else {
        panic!("the raced eligibility pass must activate the queued origin");
    };
    assert_eq!(activated.turn(), queued_turn);
    assert_eq!(activated.accepted_input().id(), queued_input);

    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
        SubmitInputRejectedResult::ActiveTurnPresent {
            session: rejected_session,
            active_turn,
        },
    )) = &submission_outcome
    else {
        panic!("the submission behind the activation must record the slot: {submission_outcome:?}");
    };
    assert_eq!(*rejected_session, session);
    assert_eq!(*active_turn, queued_turn);

    let rejection_effects: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM accepted_input WHERE accepted_input_id = $1),
            (SELECT count(*) FROM turn_lifecycle WHERE turn_id = $2),
            (SELECT count(*)
               FROM submit_input_command
              WHERE command_id = $3
                AND rejection_kind = 'active_turn_present'
                AND result_actual_active_turn_id = $4)",
    )
    .bind(racing_input.into_uuid())
    .bind(racing_turn.into_uuid())
    .bind(Uuid::from_u128(0x4b3))
    .bind(queued_turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        rejection_effects,
        (0, 0, 1),
        "a rejected raced submission must persist its evidence and nothing else"
    );

    let invariant_shape: (i64, Uuid, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE session_id = $1 AND state_kind = 'active'),
            (SELECT turn_id
               FROM turn_lifecycle
              WHERE session_id = $1 AND state_kind = 'active'),
            (SELECT count(*) FROM accepted_input WHERE session_id = $1),
            (SELECT max(acceptance_position)::bigint
               FROM accepted_input
              WHERE session_id = $1)",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(invariant_shape, (1, queued_turn.into_uuid(), 1, 1));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-007 / INV-008 / INV-009 / INV-012: the opposite scheduler queue order
/// to the sibling interleave test — the submission holds its session row and
/// the first place in the scheduler queue while the activation waits behind
/// it. Postgres grants a contended row to its first queued waiter, so the
/// serialized submission commits its applied origin at the next gap-free
/// position together with its queued-work effects, and the eligibility pass
/// then activates the earliest queued origin over that grown acceptance tail
/// with exactly one active turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv007_inv008_inv009_inv012_submit_queued_ahead_of_activation_interleaves()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x4d1, 0x8d1, direct(0xcd1)))
        .await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8d1));
    let queued_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9d1));
    let queued_turn = TurnId::from_uuid(Uuid::from_u128(0xad1));
    let racing_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9d2));
    let racing_turn = TurnId::from_uuid(Uuid::from_u128(0xad2));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x4d2,
                0x8d1,
                "eligible queued origin",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            queued_input,
            Some(queued_turn),
        )
        .await?;

    // Hold the scheduler row so both racers verifiably queue on it before
    // either proceeds: the submission takes its session-row lock and blocks
    // first, then the activation pass queues behind the submission.
    let mut scheduler_blocker = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
        .bind(Uuid::from_u128(0x8d1))
        .execute(&mut *scheduler_blocker)
        .await?;

    let submission = tokio::spawn({
        let repository = SubmitInputRepository::new(pool.clone());
        async move {
            repository
                .handle(
                    start_input(
                        0x4d3,
                        0x8d1,
                        "racing start",
                        1,
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                    racing_input,
                    Some(racing_turn),
                )
                .await
        }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the submission must hold its session row and block on the held scheduler row"
    );

    let activation = tokio::spawn({
        let mut service = StartEligibleTurnService::new(
            FixedStartEligibleTurnIds::new(
                [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdd1))],
                [ContextFrontierId::from_uuid(Uuid::from_u128(0xed1))],
                [TurnAttemptId::from_uuid(Uuid::from_u128(0xbd1))],
            ),
            StartEligibleTurnRepository::new(pool.clone()),
        );
        async move { service.execute(session).await }
    });
    assert!(
        blocked_backends_reached(&pool, 2).await?,
        "the eligibility pass must queue behind the blocked submission"
    );

    scheduler_blocker.rollback().await?;
    let submission_outcome = submission.await?.expect(
        "the submission side must serialize without deadlocking; a 40P01 surfaces here as a \
         Database error",
    );
    let activation_outcome = activation.await?.expect(
        "the activation side must serialize without deadlocking; a 40P01 surfaces here as a \
         Database error",
    );

    // Behind the committed submission, the eligibility pass still activates
    // the earliest queued origin.
    let StartEligibleTurnOutcome::Activated(activated) = activation_outcome else {
        panic!("the raced eligibility pass must activate the queued origin");
    };
    assert_eq!(activated.turn(), queued_turn);
    assert_eq!(activated.accepted_input().id(), queued_input);

    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(applied),
    )) = &submission_outcome
    else {
        panic!("the submission ahead of the activation must apply: {submission_outcome:?}");
    };
    assert_eq!(applied.accepted_input(), racing_input);
    assert_eq!(applied.turn(), racing_turn);
    assert_eq!(applied.acceptance_position().as_u64(), 2);

    let applied_effects: (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM accepted_input
              WHERE accepted_input_id = $1
                AND acceptance_position = 2
                AND disposition_kind = 'origin_of'
                AND origin_turn_id = $2),
            (SELECT count(*) FROM queued_input_origin WHERE accepted_input_id = $1)",
    )
    .bind(racing_input.into_uuid())
    .bind(racing_turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        applied_effects,
        (1, 1),
        "an applied raced submission must persist its acceptance and queued work"
    );

    let invariant_shape: (i64, Uuid, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE session_id = $1 AND state_kind = 'active'),
            (SELECT turn_id
               FROM turn_lifecycle
              WHERE session_id = $1 AND state_kind = 'active'),
            (SELECT count(*) FROM accepted_input WHERE session_id = $1),
            (SELECT max(acceptance_position)::bigint
               FROM accepted_input
              WHERE session_id = $1)",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(invariant_shape, (1, queued_turn.into_uuid(), 2, 2));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S03 / INV-009: nonexistent and empty sessions are false wake-ups that
/// return `NoEligibleTurn` and create no lifecycle effects.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s03_inv009_start_eligible_turn_false_wakeups_are_noops() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let missing = SessionId::from_uuid(Uuid::from_u128(0x7a0));
    let empty = SessionId::from_uuid(Uuid::from_u128(0x7a1));
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x3a1, 0x7a1, direct(0x8a1)))
        .await?;

    let mut service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xda0)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xda1)),
            ],
            [
                ContextFrontierId::from_uuid(Uuid::from_u128(0xea0)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xea1)),
            ],
            [
                TurnAttemptId::from_uuid(Uuid::from_u128(0xba0)),
                TurnAttemptId::from_uuid(Uuid::from_u128(0xba1)),
            ],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert_eq!(
        service.execute(missing).await?,
        StartEligibleTurnOutcome::NoEligibleTurn
    );
    assert_eq!(
        service.execute(empty).await?,
        StartEligibleTurnOutcome::NoEligibleTurn
    );
    let effects: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM turn_lifecycle),
            (SELECT count(*) FROM semantic_transcript_entry),
            (SELECT count(*) FROM context_frontier),
            (SELECT count(*) FROM turn_attempt)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(effects, (0, 0, 0, 0));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-009 / ADR-0035: once the scheduler lock admits and prepares one
/// exact queued candidate, a guarded activation that matches no row is durable
/// divergence, not a stale wake-up, and rolls back every preceding write.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv009_start_eligible_turn_zero_row_guard_is_inconsistent()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x3a2, 0x7a2, direct(0x8a2)))
        .await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x7a2));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xaa2));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x3a3,
                0x7a2,
                "guarded update divergence",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9a2)),
            Some(turn),
        )
        .await?;

    sqlx::query(
        "CREATE FUNCTION suppress_guarded_activation()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $$
         BEGIN
             RETURN NULL;
         END
         $$",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE TRIGGER suppress_guarded_activation
         BEFORE UPDATE OF state_kind ON turn_lifecycle
         FOR EACH ROW
         WHEN (OLD.state_kind = 'queued' AND NEW.state_kind = 'active')
         EXECUTE FUNCTION suppress_guarded_activation()",
    )
    .execute(&pool)
    .await?;

    let mut service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xda2))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xea2))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xba2))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let error = service
        .execute(session)
        .await
        .expect_err("zero-row guarded activation must surface durable divergence");
    assert!(matches!(
        error,
        StartEligibleTurnRepositoryError::Corruption(StartEligibleTurnCorruption::Inconsistent(
            "guarded activation matched no row"
        ))
    ));

    let unchanged: (String, i64, i64, i64) = sqlx::query_as(
        "SELECT
            state_kind,
            (SELECT count(*)
               FROM semantic_transcript_entry
              WHERE source_session_id = $2),
            (SELECT count(*)
               FROM context_frontier
              WHERE owning_session_id = $2),
            (SELECT count(*)
               FROM turn_attempt
              WHERE session_id = $2)
         FROM turn_lifecycle
        WHERE turn_id = $1",
    )
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(unchanged, ("queued".into(), 0, 0, 0));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-001 / INV-009: each durable candidate-identity collision is
/// typed and rolls back all earlier activation writes.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv001_inv009_start_eligible_turn_identity_collisions_roll_back()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x3b1, 0x7b1, direct(0x8b1)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x3b2,
                0x7b1,
                "identity source",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9b1)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xab1))),
        )
        .await?;
    let existing_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdb1));
    let existing_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xeb1));
    let existing_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xbb1));
    let mut source_service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new([existing_entry], [existing_frontier], [existing_attempt]),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert!(matches!(
        source_service
            .execute(SessionId::from_uuid(Uuid::from_u128(0x7b1)))
            .await?,
        StartEligibleTurnOutcome::Activated(_)
    ));

    for (offset, origin, frontier, attempt, expected) in [
        (
            2_u128,
            existing_entry,
            ContextFrontierId::from_uuid(Uuid::from_u128(0xeb2)),
            TurnAttemptId::from_uuid(Uuid::from_u128(0xbb2)),
            StartEligibleTurnIdentityCollision::OriginEntry,
        ),
        (
            3,
            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdb3)),
            existing_frontier,
            TurnAttemptId::from_uuid(Uuid::from_u128(0xbb3)),
            StartEligibleTurnIdentityCollision::StartingFrontier,
        ),
        (
            4,
            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdb4)),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xeb4)),
            existing_attempt,
            StartEligibleTurnIdentityCollision::InitialAttempt,
        ),
    ] {
        let session_uuid = Uuid::from_u128(0x7b0 + offset);
        let session = SessionId::from_uuid(session_uuid);
        let turn = TurnId::from_uuid(Uuid::from_u128(0xab0 + offset));
        CreateSessionRepository::new(pool.clone())
            .handle(prepared(
                0x3b0 + offset * 2,
                0x7b0 + offset,
                direct(0x8b0 + offset),
            ))
            .await?;
        SubmitInputRepository::new(pool.clone())
            .handle(
                start_input(
                    0x3b1 + offset * 2,
                    0x7b0 + offset,
                    "identity collision target",
                    1,
                    ModelSelectionOverride::UseSessionDefault,
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9b0 + offset)),
                Some(turn),
            )
            .await?;
        let mut service = StartEligibleTurnService::new(
            FixedStartEligibleTurnIds::new([origin], [frontier], [attempt]),
            StartEligibleTurnRepository::new(pool.clone()),
        );
        let error = service
            .execute(session)
            .await
            .expect_err("the reused durable candidate must fail");
        assert!(
            matches!(
                error,
                StartEligibleTurnRepositoryError::IdentityCollision(actual)
                    if actual == expected
            ),
            "unexpected collision result: {error:?}"
        );
        let unchanged: (String, i64, i64, i64) = sqlx::query_as(
            "SELECT
                state_kind,
                (SELECT count(*)
                   FROM semantic_transcript_entry
                  WHERE source_session_id = $2),
                (SELECT count(*)
                   FROM context_frontier
                  WHERE owning_session_id = $2),
                (SELECT count(*)
                   FROM turn_attempt
                  WHERE session_id = $2)
             FROM turn_lifecycle
            WHERE turn_id = $1",
        )
        .bind(turn.into_uuid())
        .bind(session_uuid)
        .fetch_one(&pool)
        .await?;
        assert_eq!(unchanged, ("queued".into(), 0, 0, 0));
    }

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-002 / INV-009: an incomplete scheduling inventory fails closed before
/// any origin entry, frontier, attempt, or lifecycle transition is written.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv002_inv009_start_eligible_turn_corrupt_projection_fails_closed()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x3c1, 0x7c1, direct(0x8c1)))
        .await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x7c1));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xac1));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x3c2,
                0x7c1,
                "corrupt projection",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9c1)),
            Some(turn),
        )
        .await?;
    sqlx::query(
        "ALTER TABLE queued_input_origin
            DROP CONSTRAINT queued_input_origin_turn_lifecycle_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE turn_lifecycle
            DROP CONSTRAINT turn_lifecycle_queued_origin_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM turn_lifecycle WHERE turn_id = $1")
        .bind(turn.into_uuid())
        .execute(&pool)
        .await?;

    let mut service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdc1))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xec1))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xbc1))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let error = service
        .execute(session)
        .await
        .expect_err("the incomplete inventory must not authorize activation");
    assert!(matches!(
        error,
        StartEligibleTurnRepositoryError::Corruption(StartEligibleTurnCorruption::Scheduling(
            SubmitInputCorruption::Inconsistent("complete scheduling turn inventory")
        ))
    ));
    let effects: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM semantic_transcript_entry),
            (SELECT count(*) FROM context_frontier),
            (SELECT count(*) FROM turn_attempt)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(effects, (0, 0, 0));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S09 / INV-009 / INV-015: after the first queued turn fails, the adapter
/// activates the next turn with exact predecessor lineage and a
/// prefix-preserving starting frontier.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s09_inv009_inv015_start_eligible_turn_preserves_failed_predecessor_prefix()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x3d1, 0x7d1, direct(0x8d1)))
        .await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x7d1));
    let accepted_first = AcceptedInputId::from_uuid(Uuid::from_u128(0x9d1));
    let accepted_second = AcceptedInputId::from_uuid(Uuid::from_u128(0x9d2));
    let first_turn = TurnId::from_uuid(Uuid::from_u128(0xad1));
    let second_turn = TurnId::from_uuid(Uuid::from_u128(0xad2));
    let submit = SubmitInputRepository::new(pool.clone());
    submit
        .handle(
            start_input(
                0x3d2,
                0x7d1,
                "first queued",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            accepted_first,
            Some(first_turn),
        )
        .await?;
    submit
        .handle(
            start_input(
                0x3d3,
                0x7d1,
                "second queued",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            accepted_second,
            Some(second_turn),
        )
        .await?;

    let first_origin = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdd1));
    let first_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xed1));
    let first_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xbd1));
    let mut first_service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new([first_origin], [first_frontier], [first_attempt]),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert!(matches!(
        first_service.execute(session).await?,
        StartEligibleTurnOutcome::Activated(_)
    ));

    let failure_entry = Uuid::from_u128(0xdd2);
    let terminal_frontier = Uuid::from_u128(0xed2);
    let mut terminalize = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session.into_uuid())
    .bind(failure_entry)
    .bind(first_turn.into_uuid())
    .execute(&mut *terminalize)
    .await?;
    insert_frontier(
        &mut terminalize,
        session.into_uuid(),
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, session.into_uuid(), first_origin.into_uuid()),
            (Decimal::from(2_u64), session.into_uuid(), failure_entry),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = 'without_stop',
                end_disposition = 'known_failure'
          WHERE turn_attempt_id = $1",
    )
    .bind(first_attempt.into_uuid())
    .execute(&mut *terminalize)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                terminal_frontier_id = $1,
                active_phase_kind = NULL,
                terminal_attempt_id = current_attempt_id,
                current_attempt_id = NULL,
                terminal_disposition_kind = 'failed'
          WHERE turn_id = $2",
    )
    .bind(terminal_frontier)
    .bind(first_turn.into_uuid())
    .execute(&mut *terminalize)
    .await?;
    terminalize.commit().await?;

    let second_origin = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdd3));
    let second_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xed3));
    let second_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xbd3));
    let mut second_service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new([second_origin], [second_frontier], [second_attempt]),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) = second_service.execute(session).await?
    else {
        panic!("the successor must activate after its failed predecessor");
    };
    assert_eq!(activated.turn(), second_turn);
    assert_eq!(
        activated.start().lineage(),
        AcceptedInputStartingLineage::After {
            immediate_predecessor: first_turn,
        }
    );
    assert_eq!(activated.start().frontier().snapshot(), second_frontier);

    let members: Vec<(i64, Uuid)> = sqlx::query_as(
        "SELECT member_position::bigint, semantic_entry_id
           FROM context_frontier_member
          WHERE owning_session_id = $1
            AND context_frontier_id = $2
          ORDER BY member_position",
    )
    .bind(session.into_uuid())
    .bind(second_frontier.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        members,
        vec![
            (1, first_origin.into_uuid()),
            (2, failure_entry),
            (3, second_origin.into_uuid()),
        ]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-006 / INV-009 / INV-015: one complete schema-level eligibility
/// transaction can bind the exact origin frontier and prepared attempt, while
/// the database independently rejects contradictory lifecycle histories.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv006_inv009_inv015_turn_storage_enforces_lifecycle_consistency()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x401, 0x801, direct(0xc01)))
        .await?;
    let submit = SubmitInputRepository::new(pool.clone());
    submit
        .handle(
            start_input(
                0x402,
                0x801,
                "first",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x901)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa01))),
        )
        .await?;
    submit
        .handle(
            start_input(
                0x403,
                0x801,
                "second",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x902)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa02))),
        )
        .await?;

    let session = Uuid::from_u128(0x801);
    let first_turn = Uuid::from_u128(0xa01);
    let first_attempt = Uuid::from_u128(0xb01);
    let first_entry = Uuid::from_u128(0xd01);
    let first_frontier = Uuid::from_u128(0xe01);
    let mut activation = pool.begin().await?;
    insert_origin_frontier(
        &mut activation,
        session,
        Uuid::from_u128(0x901),
        first_entry,
        first_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(first_attempt)
    .bind(first_turn)
    .bind(session)
    .execute(&mut *activation)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'active',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                active_phase_kind = 'running',
                current_attempt_id = $2
          WHERE turn_id = $3
            AND state_kind = 'queued'",
    )
    .bind(first_frontier)
    .bind(first_attempt)
    .bind(first_turn)
    .execute(&mut *activation)
    .await?;
    activation.commit().await?;

    let active_shape: (String, String, String, String, i64) = sqlx::query_as(
        "SELECT turn.state_kind, turn.start_lineage_kind,
                turn.active_phase_kind, attempt.state_kind,
                frontier.member_count::bigint
           FROM turn_lifecycle AS turn
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = turn.current_attempt_id
           JOIN context_frontier AS frontier
             ON frontier.owning_session_id = turn.session_id
            AND frontier.context_frontier_id = turn.starting_frontier_id
          WHERE turn.turn_id = $1",
    )
    .bind(first_turn)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        active_shape,
        (
            "active".into(),
            "first_in_session".into(),
            "running".into(),
            "prepared".into(),
            1
        )
    );

    let born_active = sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_accepted_input_id, acceptance_position,
             state_kind, start_lineage_kind, immediate_predecessor_turn_id,
             starting_frontier_id, terminal_frontier_id, active_phase_kind,
             current_attempt_id, terminal_disposition_kind)
         SELECT turn_id, session_id, origin_accepted_input_id, acceptance_position,
                state_kind, start_lineage_kind, immediate_predecessor_turn_id,
                starting_frontier_id, terminal_frontier_id, active_phase_kind,
                current_attempt_id, terminal_disposition_kind
           FROM turn_lifecycle
          WHERE turn_id = $1",
    )
    .bind(first_turn)
    .execute(&pool)
    .await
    .expect_err("even a complete active shape must first be inserted as queued");
    assert_eq!(
        born_active
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    assert_eq!(
        born_active
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_lifecycle_inserted_queued")
    );

    for (attempt_id, state_kind, end_variant, end_disposition) in [
        (Uuid::from_u128(0xb05), "running", None, None),
        (
            Uuid::from_u128(0xb06),
            "ended",
            Some("without_stop"),
            Some("known_failure"),
        ),
    ] {
        let born_nonprepared = sqlx::query(
            "INSERT INTO turn_attempt
                (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
                 state_kind, end_variant, end_disposition)
             VALUES ($1, $2, $3, NULL, $4, $5, $6)",
        )
        .bind(attempt_id)
        .bind(Uuid::from_u128(0xa02))
        .bind(session)
        .bind(state_kind)
        .bind(end_variant)
        .bind(end_disposition)
        .execute(&pool)
        .await
        .expect_err("every attempt must first be inserted as prepared");
        assert_eq!(
            born_nonprepared
                .as_database_error()
                .and_then(|error| error.code()),
            Some("23514".into())
        );
        assert_eq!(
            born_nonprepared
                .as_database_error()
                .and_then(|error| error.constraint()),
            Some("turn_attempt_inserted_prepared"),
            "unexpected insert guard for born-{state_kind} attempt"
        );
    }

    let mut second_activation = pool.begin().await?;
    insert_origin_frontier(
        &mut second_activation,
        session,
        Uuid::from_u128(0x902),
        Uuid::from_u128(0xd02),
        Uuid::from_u128(0xe02),
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xb02))
    .bind(Uuid::from_u128(0xa02))
    .bind(session)
    .execute(&mut *second_activation)
    .await?;
    let second_active = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'active',
                start_lineage_kind = 'after',
                immediate_predecessor_turn_id = $1,
                starting_frontier_id = $2,
                active_phase_kind = 'running',
                current_attempt_id = $3
          WHERE turn_id = $4",
    )
    .bind(first_turn)
    .bind(Uuid::from_u128(0xe02))
    .bind(Uuid::from_u128(0xb02))
    .bind(Uuid::from_u128(0xa02))
    .execute(&mut *second_activation)
    .await
    .expect_err("the partial unique index must reject a second active turn");
    assert_eq!(
        second_active
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_lifecycle_one_active_per_session")
    );
    second_activation.rollback().await?;

    let mut duplicate_live = pool.begin().await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xb03))
    .bind(Uuid::from_u128(0xa02))
    .bind(session)
    .execute(&mut *duplicate_live)
    .await?;
    let second_live = sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, $4, 'prepared', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xb04))
    .bind(Uuid::from_u128(0xa02))
    .bind(session)
    .bind(Uuid::from_u128(0xb03))
    .execute(&mut *duplicate_live)
    .await
    .expect_err("the partial unique index must reject a second live attempt");
    assert_eq!(
        second_live
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_attempt_one_live_per_turn")
    );
    duplicate_live.rollback().await?;

    let immutable_start = sqlx::query(
        "UPDATE turn_lifecycle
            SET starting_frontier_id = $1
          WHERE turn_id = $2",
    )
    .bind(Uuid::from_u128(0xeff))
    .bind(first_turn)
    .execute(&pool)
    .await
    .expect_err("a committed turn start must be write-once");
    assert_eq!(
        immutable_start
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let immutable_member = sqlx::query(
        "UPDATE context_frontier_member
            SET member_position = 2
          WHERE owning_session_id = $1
            AND context_frontier_id = $2",
    )
    .bind(session)
    .bind(first_frontier)
    .execute(&pool)
    .await
    .expect_err("committed frontier membership must be immutable");
    assert_eq!(
        immutable_member
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let out_of_bounds_member = sqlx::query(
        "INSERT INTO context_frontier_member
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES ($1, $2, 2, $1, $3)",
    )
    .bind(session)
    .bind(first_frontier)
    .bind(first_entry)
    .execute(&pool)
    .await
    .expect_err("committed frontier membership cannot exceed its declared count");
    assert_eq!(
        out_of_bounds_member
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("context_frontier_member_within_declared_count")
    );

    let duplicate_frontier = Uuid::from_u128(0xe04);
    let mut duplicate_membership = pool.begin().await?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 2)",
    )
    .bind(session)
    .bind(duplicate_frontier)
    .execute(&mut *duplicate_membership)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier_member
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES ($1, $2, 1, $1, $3)",
    )
    .bind(session)
    .bind(duplicate_frontier)
    .bind(first_entry)
    .execute(&mut *duplicate_membership)
    .await?;
    let duplicate_member = sqlx::query(
        "INSERT INTO context_frontier_member
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES ($1, $2, 2, $1, $3)",
    )
    .bind(session)
    .bind(duplicate_frontier)
    .bind(first_entry)
    .execute(&mut *duplicate_membership)
    .await
    .expect_err("one exact source-qualified entry cannot occur twice");
    assert_eq!(
        duplicate_member
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("context_frontier_member_entry_once")
    );
    duplicate_membership.rollback().await?;

    let mut unavailable_continuation = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = 'without_stop',
                end_disposition = 'known_failure'
          WHERE turn_attempt_id = $1",
    )
    .bind(first_attempt)
    .execute(&mut *unavailable_continuation)
    .await?;
    let successor_attempt = Uuid::from_u128(0xb02);
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, $4, 'prepared', NULL, NULL)",
    )
    .bind(successor_attempt)
    .bind(first_turn)
    .bind(session)
    .bind(first_attempt)
    .execute(&mut *unavailable_continuation)
    .await?;
    let replacement_error = sqlx::query(
        "UPDATE turn_lifecycle
            SET current_attempt_id = $1
          WHERE turn_id = $2",
    )
    .bind(successor_attempt)
    .bind(first_turn)
    .execute(&mut *unavailable_continuation)
    .await
    .expect_err("a running turn cannot replace its sealed current attempt");
    assert_eq!(
        replacement_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    assert!(replacement_error.as_database_error().is_some_and(|error| {
        error
            .message()
            .contains("running turn cannot replace its current attempt")
    }));
    unavailable_continuation.rollback().await?;

    let failure_entry = Uuid::from_u128(0xd03);
    let terminal_frontier = Uuid::from_u128(0xe03);
    for contradictory_disposition in [
        "turn_completed",
        "turn_refused",
        "yielded_to_durable_wait",
        "ambiguous",
    ] {
        let mut contradictory_terminal = pool.begin().await?;
        sqlx::query(
            "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 origin_accepted_input_id, failed_turn_id)
             VALUES ($1, $2, 'turn_failed', NULL, $3)",
        )
        .bind(session)
        .bind(failure_entry)
        .bind(first_turn)
        .execute(&mut *contradictory_terminal)
        .await?;
        insert_frontier(
            &mut contradictory_terminal,
            session,
            terminal_frontier,
            Decimal::from(2_u64),
            &[
                (Decimal::ONE, session, first_entry),
                (Decimal::from(2_u64), session, failure_entry),
            ],
        )
        .await?;
        sqlx::query(
            "UPDATE turn_attempt
                SET state_kind = 'ended',
                    end_variant = 'without_stop',
                    end_disposition = $1
              WHERE turn_attempt_id = $2",
        )
        .bind(contradictory_disposition)
        .bind(first_attempt)
        .execute(&mut *contradictory_terminal)
        .await?;
        sqlx::query(
            "UPDATE turn_lifecycle
                SET state_kind = 'terminal',
                    active_phase_kind = NULL,
                    terminal_attempt_id = current_attempt_id,
                    current_attempt_id = NULL,
                    terminal_frontier_id = $1,
                    terminal_disposition_kind = 'failed'
              WHERE turn_id = $2",
        )
        .bind(terminal_frontier)
        .bind(first_turn)
        .execute(&mut *contradictory_terminal)
        .await?;

        let contradictory_terminal_error = contradictory_terminal
            .commit()
            .await
            .expect_err("a failed turn cannot retain a contradictory ended attempt");
        let database_error = contradictory_terminal_error
            .as_database_error()
            .expect("deferred lifecycle validation must return a database error");
        assert_eq!(database_error.code(), Some("23514".into()));
        assert!(
            database_error
                .message()
                .contains("permits only known_failure or lost ended attempts"),
            "unexpected terminal consistency error for {contradictory_disposition}: {}",
            database_error.message()
        );
    }

    let mut terminalize = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(first_turn)
    .execute(&mut *terminalize)
    .await?;
    insert_frontier(
        &mut terminalize,
        session,
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, session, first_entry),
            (Decimal::from(2_u64), session, failure_entry),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = 'without_stop',
                end_disposition = 'known_failure'
          WHERE turn_attempt_id = $1",
    )
    .bind(first_attempt)
    .execute(&mut *terminalize)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                active_phase_kind = NULL,
                terminal_attempt_id = current_attempt_id,
                current_attempt_id = NULL,
                terminal_frontier_id = $1,
                terminal_disposition_kind = 'failed'
          WHERE turn_id = $2",
    )
    .bind(terminal_frontier)
    .bind(first_turn)
    .execute(&mut *terminalize)
    .await?;
    terminalize.commit().await?;

    let immutable_attempt = sqlx::query(
        "UPDATE turn_attempt
            SET end_disposition = 'lost'
          WHERE turn_attempt_id = $1",
    )
    .bind(first_attempt)
    .execute(&pool)
    .await
    .expect_err("an ended attempt must be immutable");
    assert_eq!(
        immutable_attempt
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let born_terminal = sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_accepted_input_id, acceptance_position,
             state_kind, start_lineage_kind, immediate_predecessor_turn_id,
             starting_frontier_id, terminal_frontier_id, active_phase_kind,
             current_attempt_id, terminal_disposition_kind)
         SELECT turn_id, session_id, origin_accepted_input_id, acceptance_position,
                state_kind, start_lineage_kind, immediate_predecessor_turn_id,
                starting_frontier_id, terminal_frontier_id, active_phase_kind,
                current_attempt_id, terminal_disposition_kind
           FROM turn_lifecycle
          WHERE turn_id = $1",
    )
    .bind(first_turn)
    .execute(&pool)
    .await
    .expect_err("even a complete terminal shape must first be inserted as queued");
    assert_eq!(
        born_terminal
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    assert_eq!(
        born_terminal
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_lifecycle_inserted_queued")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / S03 / S08 / S09 / INV-002 / INV-007 / INV-008 / INV-009 / INV-012:
/// occupied-slot After and NextSafePoint handling commits the exact distinct
/// effects, checked replay survives a pool/repository restart, and the
/// restarted adapter advances from the complete validated acceptance tail
/// without admitting an unrelated non-lifecycle frontier into the projection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn occupied_slot_after_and_safe_point_apply_replay_and_restart() -> Result<(), Box<dyn Error>>
{
    let (container, pool, database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x431, 0x831, direct(0xc31)))
        .await?;
    let active_origin_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x931));
    let active_origin_turn = TurnId::from_uuid(Uuid::from_u128(0xa31));
    let repository = SubmitInputRepository::new(pool.clone());
    repository
        .handle(
            start_input(
                0x432,
                0x831,
                "active origin",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            active_origin_input,
            Some(active_origin_turn),
        )
        .await?;
    let activated = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: Uuid::from_u128(0x831),
            origin_entry: Uuid::from_u128(0xd31),
            starting_frontier: Uuid::from_u128(0xe31),
            initial_attempt: Uuid::from_u128(0xb31),
        },
    )
    .await?;
    assert_eq!(activated.accepted_input().id(), active_origin_input);
    assert_eq!(activated.turn(), active_origin_turn);
    let mut unrelated_frontier = pool.begin().await?;
    insert_frontier(
        &mut unrelated_frontier,
        Uuid::from_u128(0x831),
        Uuid::from_u128(0xef31),
        Decimal::ONE,
        &[(Decimal::ONE, Uuid::from_u128(0x831), Uuid::from_u128(0xd31))],
    )
    .await?;
    unrelated_frontier.commit().await?;

    let after = input_with_delivery(
        0x433,
        0x831,
        "after active",
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa31)),
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let after_outcome = repository
        .handle(
            after.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x932)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa32))),
        )
        .await?;
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(after_applied),
    )) = &after_outcome
    else {
        panic!("matching AfterCurrentTurn must create queued origin work");
    };
    assert_eq!(after_applied.acceptance_position().as_u64(), 2);

    let safe_point = input_with_delivery(
        0x434,
        0x831,
        "steer active",
        DeliveryRequest::NextSafePoint {
            expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa31)),
        },
    );
    let safe_point_outcome = repository
        .handle(
            safe_point.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x933)),
            None,
        )
        .await?;
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::PendingSteering(steering),
    )) = &safe_point_outcome
    else {
        panic!("matching NextSafePoint must create pending steering");
    };
    assert_eq!(steering.acceptance_position().as_u64(), 3);
    assert_eq!(
        steering.binding().source_turn(),
        TurnId::from_uuid(Uuid::from_u128(0xa31))
    );

    assert_eq!(
        repository
            .handle(
                after.clone(),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9ff)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xaff))),
            )
            .await?,
        after_outcome
    );
    assert_eq!(
        repository
            .handle(
                safe_point.clone(),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fe)),
                None,
            )
            .await?,
        safe_point_outcome
    );

    let mut application_service = SubmitInputService::new(
        FixedSubmitInputIds::new(
            [
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fb)),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fa)),
            ],
            [TurnId::from_uuid(Uuid::from_u128(0xafb))],
        ),
        repository.clone(),
        AcceptingEligibilityNudge,
    );
    let after_request = SubmitInputRequest::try_new(
        after.command_id(),
        after.session(),
        after.content().clone(),
        after.delivery(),
    )?;
    let safe_point_request = SubmitInputRequest::try_new(
        safe_point.command_id(),
        safe_point.session(),
        safe_point.content().clone(),
        safe_point.delivery(),
    )?;
    assert_eq!(
        application_service.execute(after_request).await?,
        SubmitInputOutcome::Recorded(match &after_outcome {
            SubmitInputHandlingOutcome::Recorded(result) => result.clone(),
            SubmitInputHandlingOutcome::ConflictingReuse { .. } => {
                unreachable!("the exact occupied-slot command was recorded")
            }
        })
    );
    assert_eq!(
        application_service.execute(safe_point_request).await?,
        SubmitInputOutcome::Recorded(match &safe_point_outcome {
            SubmitInputHandlingOutcome::Recorded(result) => result.clone(),
            SubmitInputHandlingOutcome::ConflictingReuse { .. } => {
                unreachable!("the exact occupied-slot command was recorded")
            }
        })
    );

    let effect_shape: (i64, i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM accepted_input
              WHERE accepted_input_id = $1
                AND delivery_kind = 'after_current_turn'
                AND disposition_kind = 'origin_of'
                AND origin_turn_id = $2
                AND expected_defaults_version = 1),
            (SELECT count(*) FROM queued_input_origin WHERE accepted_input_id = $1),
            (SELECT count(*) FROM turn_lifecycle WHERE origin_accepted_input_id = $1),
            (SELECT count(*)
               FROM accepted_input
              WHERE accepted_input_id = $3
                AND delivery_kind = 'next_safe_point'
                AND disposition_kind = 'pending_steering'
                AND expected_active_turn_id = $4
                AND expected_defaults_version IS NULL
                AND model_override_kind IS NULL
                AND replacement_model_kind IS NULL
                AND replacement_direct_model_selection_id IS NULL
                AND replacement_model_alias_id IS NULL
                AND origin_turn_id IS NULL),
            (SELECT count(*) FROM queued_input_origin WHERE accepted_input_id = $3),
            (SELECT count(*) FROM turn_lifecycle WHERE origin_accepted_input_id = $3),
            (SELECT count(*)
               FROM information_schema.columns
              WHERE table_schema = current_schema()
                AND table_name = 'accepted_input'
                AND column_name = 'steering_source_turn_id'),
            (SELECT count(*)
               FROM submit_input_command
              WHERE command_id = $5
                AND result_actual_active_turn_id = $4)",
    )
    .bind(Uuid::from_u128(0x932))
    .bind(Uuid::from_u128(0xa32))
    .bind(Uuid::from_u128(0x933))
    .bind(Uuid::from_u128(0xa31))
    .bind(Uuid::from_u128(0x434))
    .fetch_one(&pool)
    .await?;
    assert_eq!(effect_shape, (1, 1, 1, 1, 0, 0, 0, 1));

    drop(repository);
    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let restarted = SubmitInputRepository::new(restarted_pool.clone());
    assert_eq!(
        restarted
            .handle(
                after,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fd)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xafd))),
            )
            .await?,
        after_outcome
    );
    assert_eq!(
        restarted
            .handle(
                safe_point,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fc)),
                None,
            )
            .await?,
        safe_point_outcome
    );

    let after_restart = input_with_delivery(
        0x435,
        0x831,
        "after restart",
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa31)),
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(after_restart),
    )) = restarted
        .handle(
            after_restart,
            AcceptedInputId::from_uuid(Uuid::from_u128(0x934)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa33))),
        )
        .await?
    else {
        panic!("restart must preserve occupied-slot origin submission");
    };
    assert_eq!(after_restart.acceptance_position().as_u64(), 4);

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / S03 / S08 / INV-008 / INV-009 / INV-012: the composed production
/// chain — CreateSession service, accepted start submission, and
/// StartEligibleTurn service activation — produces the occupied slot the
/// seeded occupied-slot tests assume: a matching After request queues at the
/// next gap-free position, a matching NextSafePoint binds pending steering to
/// the activated turn, and a start names the activated turn in its typed
/// rejection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn occupied_slot_handling_composes_with_service_activated_first_turn()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8a1));
    let mut create_service = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone()),
    );
    let CreateSessionOutcome::Applied(created) = create_service
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x4a1)),
            SessionConfigurationDefaults::new(direct(0xca1)),
        )?)
        .await?
    else {
        panic!("owner-initiated composed creation must apply");
    };
    assert_eq!(created.session(), session);

    let origin_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9a1));
    let origin_turn = TurnId::from_uuid(Uuid::from_u128(0xaa1));
    let mut submit_service = SubmitInputService::new(
        FixedSubmitInputIds::new(
            [
                origin_input,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9a2)),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9a3)),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9a4)),
            ],
            [
                origin_turn,
                TurnId::from_uuid(Uuid::from_u128(0xaa2)),
                TurnId::from_uuid(Uuid::from_u128(0xaa3)),
            ],
        ),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
    );
    let start = start_input(
        0x4a2,
        0x8a1,
        "composed start",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(origin),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            start.command_id(),
            start.session(),
            start.content().clone(),
            start.delivery(),
        )?)
        .await?
    else {
        panic!("the composed no-active-turn start must apply");
    };
    assert_eq!(origin.turn(), origin_turn);
    assert_eq!(origin.acceptance_position().as_u64(), 1);

    let starting_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xea1));
    let mut activation_service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xda1))],
            [starting_frontier],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xba1))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) =
        activation_service.execute(session).await?
    else {
        panic!("the sole composed queued turn must activate");
    };
    assert_eq!(activated.session(), session);
    assert_eq!(activated.turn(), origin.turn());
    assert_eq!(activated.accepted_input().id(), origin.accepted_input());
    assert_eq!(
        activated.start().lineage(),
        AcceptedInputStartingLineage::FirstInSession
    );
    assert_eq!(activated.start().frontier().snapshot(), starting_frontier);

    let after = input_with_delivery(
        0x4a3,
        0x8a1,
        "after service-activated turn",
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: activated.turn(),
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(after_applied),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            after.command_id(),
            after.session(),
            after.content().clone(),
            after.delivery(),
        )?)
        .await?
    else {
        panic!("matching AfterCurrentTurn must queue against the service-activated turn");
    };
    assert_eq!(after_applied.acceptance_position().as_u64(), 2);

    let safe_point = input_with_delivery(
        0x4a4,
        0x8a1,
        "steer service-activated turn",
        DeliveryRequest::NextSafePoint {
            expected_active_turn: activated.turn(),
        },
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::PendingSteering(steering),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            safe_point.command_id(),
            safe_point.session(),
            safe_point.content().clone(),
            safe_point.delivery(),
        )?)
        .await?
    else {
        panic!("matching NextSafePoint must bind against the service-activated turn");
    };
    assert_eq!(steering.acceptance_position().as_u64(), 3);
    assert_eq!(steering.binding().source_turn(), activated.turn());

    let blocked_start = start_input(
        0x4a5,
        0x8a1,
        "blocked composed start",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let blocked = submit_service
        .execute(SubmitInputRequest::try_new(
            blocked_start.command_id(),
            blocked_start.session(),
            blocked_start.content().clone(),
            blocked_start.delivery(),
        )?)
        .await?;
    assert!(
        matches!(
            blocked,
            SubmitInputOutcome::Recorded(SubmitInputResult::Rejected(
                SubmitInputRejectedResult::ActiveTurnPresent {
                    session: rejected_session,
                    active_turn,
                }
            )) if rejected_session == session && active_turn == activated.turn()
        ),
        "a start against the service-activated slot must name it: {blocked:?}"
    );

    let effect_shape: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE session_id = $1 AND state_kind = 'active'),
            (SELECT count(*)
               FROM accepted_input
              WHERE accepted_input_id = $2
                AND delivery_kind = 'after_current_turn'
                AND disposition_kind = 'origin_of'
                AND origin_turn_id = $3),
            (SELECT count(*) FROM queued_input_origin WHERE accepted_input_id = $2),
            (SELECT count(*)
               FROM accepted_input
              WHERE accepted_input_id = $4
                AND delivery_kind = 'next_safe_point'
                AND disposition_kind = 'pending_steering'
                AND expected_active_turn_id = $5),
            (SELECT count(*) FROM queued_input_origin WHERE accepted_input_id = $4)",
    )
    .bind(session.into_uuid())
    .bind(after_applied.accepted_input().into_uuid())
    .bind(after_applied.turn().into_uuid())
    .bind(steering.accepted_input().into_uuid())
    .bind(activated.turn().into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(effect_shape, (1, 1, 1, 1, 0));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / S08 / S09 / INV-008 / INV-009 / INV-012: after the production chain
/// activates the first turn and terminal facts close it, the production
/// activation service commits the After-lineage successor, and occupied-slot
/// handling against that successor matches the first-in-session pass: After
/// queues at the next gap-free position, NextSafePoint binds to the
/// successor, and a start names it. The predecessor's terminalization uses
/// this suite's raw terminal seam (the same seam the S09 predecessor-prefix
/// test uses) because no production terminalization adapter exists yet; every
/// other step is the production chain.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn occupied_slot_handling_composes_with_service_activated_after_lineage_turn()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8c1));
    let mut create_service = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone()),
    );
    let CreateSessionOutcome::Applied(created) = create_service
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x4c1)),
            SessionConfigurationDefaults::new(direct(0xcc1)),
        )?)
        .await?
    else {
        panic!("owner-initiated composed creation must apply");
    };
    assert_eq!(created.session(), session);

    let first_turn = TurnId::from_uuid(Uuid::from_u128(0xac1));
    let second_turn = TurnId::from_uuid(Uuid::from_u128(0xac2));
    let mut submit_service = SubmitInputService::new(
        FixedSubmitInputIds::new(
            [
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9c1)),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9c2)),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9c3)),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9c4)),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9c5)),
            ],
            [
                first_turn,
                second_turn,
                TurnId::from_uuid(Uuid::from_u128(0xac3)),
                TurnId::from_uuid(Uuid::from_u128(0xac4)),
            ],
        ),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
    );
    let first_start = start_input(
        0x4c2,
        0x8c1,
        "first composed start",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(first_origin),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            first_start.command_id(),
            first_start.session(),
            first_start.content().clone(),
            first_start.delivery(),
        )?)
        .await?
    else {
        panic!("the first composed start must apply");
    };
    assert_eq!(first_origin.turn(), first_turn);
    let second_start = start_input(
        0x4c3,
        0x8c1,
        "second composed start",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(second_origin),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            second_start.command_id(),
            second_start.session(),
            second_start.content().clone(),
            second_start.delivery(),
        )?)
        .await?
    else {
        panic!("the second composed start must queue behind the first");
    };
    assert_eq!(second_origin.turn(), second_turn);
    assert_eq!(second_origin.acceptance_position().as_u64(), 2);

    let first_origin_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdc1));
    let first_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xbc1));
    let mut first_activation = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [first_origin_entry],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xec1))],
            [first_attempt],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(first_activated) =
        first_activation.execute(session).await?
    else {
        panic!("the first composed queued turn must activate");
    };
    assert_eq!(first_activated.turn(), first_turn);

    // Raw terminal seam: no production terminalization adapter exists yet, so
    // the predecessor's failure facts commit exactly as in the S09
    // predecessor-prefix test.
    let failure_entry = Uuid::from_u128(0xdc2);
    let terminal_frontier = Uuid::from_u128(0xec2);
    let mut terminalize = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session.into_uuid())
    .bind(failure_entry)
    .bind(first_turn.into_uuid())
    .execute(&mut *terminalize)
    .await?;
    insert_frontier(
        &mut terminalize,
        session.into_uuid(),
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (
                Decimal::ONE,
                session.into_uuid(),
                first_origin_entry.into_uuid(),
            ),
            (Decimal::from(2_u64), session.into_uuid(), failure_entry),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = 'without_stop',
                end_disposition = 'known_failure'
          WHERE turn_attempt_id = $1",
    )
    .bind(first_attempt.into_uuid())
    .execute(&mut *terminalize)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                terminal_frontier_id = $1,
                active_phase_kind = NULL,
                terminal_attempt_id = current_attempt_id,
                current_attempt_id = NULL,
                terminal_disposition_kind = 'failed'
          WHERE turn_id = $2",
    )
    .bind(terminal_frontier)
    .bind(first_turn.into_uuid())
    .execute(&mut *terminalize)
    .await?;
    terminalize.commit().await?;

    let mut second_activation = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdc3))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xec3))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xbc3))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(second_activated) =
        second_activation.execute(session).await?
    else {
        panic!("the successor must activate after its failed predecessor");
    };
    assert_eq!(second_activated.turn(), second_turn);
    assert_eq!(
        second_activated.start().lineage(),
        AcceptedInputStartingLineage::After {
            immediate_predecessor: first_turn,
        }
    );

    let after = input_with_delivery(
        0x4c4,
        0x8c1,
        "after the After-lineage turn",
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: second_activated.turn(),
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(after_applied),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            after.command_id(),
            after.session(),
            after.content().clone(),
            after.delivery(),
        )?)
        .await?
    else {
        panic!("matching AfterCurrentTurn must queue against the After-lineage turn");
    };
    assert_eq!(after_applied.acceptance_position().as_u64(), 3);

    let safe_point = input_with_delivery(
        0x4c5,
        0x8c1,
        "steer the After-lineage turn",
        DeliveryRequest::NextSafePoint {
            expected_active_turn: second_activated.turn(),
        },
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::PendingSteering(steering),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            safe_point.command_id(),
            safe_point.session(),
            safe_point.content().clone(),
            safe_point.delivery(),
        )?)
        .await?
    else {
        panic!("matching NextSafePoint must bind against the After-lineage turn");
    };
    assert_eq!(steering.acceptance_position().as_u64(), 4);
    assert_eq!(steering.binding().source_turn(), second_activated.turn());

    let blocked_start = start_input(
        0x4c6,
        0x8c1,
        "blocked start behind the After-lineage turn",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let blocked = submit_service
        .execute(SubmitInputRequest::try_new(
            blocked_start.command_id(),
            blocked_start.session(),
            blocked_start.content().clone(),
            blocked_start.delivery(),
        )?)
        .await?;
    assert!(
        matches!(
            blocked,
            SubmitInputOutcome::Recorded(SubmitInputResult::Rejected(
                SubmitInputRejectedResult::ActiveTurnPresent {
                    session: rejected_session,
                    active_turn,
                }
            )) if rejected_session == session && active_turn == second_activated.turn()
        ),
        "a start against the After-lineage slot must name it: {blocked:?}"
    );

    let successor_shape: (i64, String, Uuid, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE session_id = $1 AND state_kind = 'active'),
            turn.start_lineage_kind,
            turn.immediate_predecessor_turn_id,
            frontier.member_count::bigint
         FROM turn_lifecycle AS turn
         JOIN context_frontier AS frontier
           ON frontier.owning_session_id = turn.session_id
          AND frontier.context_frontier_id = turn.starting_frontier_id
        WHERE turn.turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(second_activated.turn().into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        successor_shape,
        (1, "after".into(), first_turn.into_uuid(), 3)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-007 / INV-008 / INV-012: the session-before-scheduler lock order
/// serializes mixed occupied-slot acceptances into one gap-free order while
/// preserving each delivery's distinct atomic effect shape.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn occupied_slot_mixed_acceptances_serialize_positions_and_effects()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x451, 0x851, direct(0xc51)))
        .await?;
    let active_origin_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x951));
    let active_origin_turn = TurnId::from_uuid(Uuid::from_u128(0xa51));
    let repository = SubmitInputRepository::new(pool.clone());
    repository
        .handle(
            start_input(
                0x452,
                0x851,
                "active origin",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            active_origin_input,
            Some(active_origin_turn),
        )
        .await?;
    let activated = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: Uuid::from_u128(0x851),
            origin_entry: Uuid::from_u128(0xd51),
            starting_frontier: Uuid::from_u128(0xe51),
            initial_attempt: Uuid::from_u128(0xb51),
        },
    )
    .await?;
    assert_eq!(activated.accepted_input().id(), active_origin_input);
    assert_eq!(activated.turn(), active_origin_turn);

    let (positions, turn_origins, pending_steering) =
        run_mixed_occupied_acceptances(repository).await?;
    assert_eq!(positions, vec![2, 3, 4, 5, 6, 7]);
    assert_eq!((turn_origins, pending_steering), (3, 3));

    let effects: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            count(*) FILTER (WHERE delivery_kind = 'after_current_turn'),
            count(*) FILTER (WHERE delivery_kind = 'next_safe_point'),
            (SELECT count(*)
               FROM queued_input_origin
              WHERE session_id = $1
                AND acceptance_position > 1),
            (SELECT count(*)
               FROM accepted_input
              WHERE session_id = $1
                AND disposition_kind = 'pending_steering'
                AND origin_turn_id IS NULL
                AND expected_defaults_version IS NULL)
          FROM accepted_input
         WHERE session_id = $1
           AND acceptance_position > 1",
    )
    .bind(Uuid::from_u128(0x851))
    .fetch_one(&pool)
    .await?;
    assert_eq!(effects, (3, 3, 3, 3));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-002 / INV-005 / INV-008 / INV-012 / INV-016: occupied-slot result
/// shapes and correlations are database-enforced, pending steering keeps its
/// source active and cannot become semantic origin, and its immutable receipt
/// survives a later current-disposition change.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn occupied_slot_schema_constraints_and_checked_decode_fail_closed()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x461, 0x861, direct(0xc61)))
        .await?;
    let active_origin_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x961));
    let active_origin_turn = TurnId::from_uuid(Uuid::from_u128(0xa61));
    let repository = SubmitInputRepository::new(pool.clone());
    repository
        .handle(
            start_input(
                0x462,
                0x861,
                "active origin",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            active_origin_input,
            Some(active_origin_turn),
        )
        .await?;
    let activated = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: Uuid::from_u128(0x861),
            origin_entry: Uuid::from_u128(0xd61),
            starting_frontier: Uuid::from_u128(0xe61),
            initial_attempt: Uuid::from_u128(0xb61),
        },
    )
    .await?;
    assert_eq!(activated.accepted_input().id(), active_origin_input);
    assert_eq!(activated.turn(), active_origin_turn);
    let safe_source = input_with_delivery(
        0x463,
        0x861,
        "safe-point representation",
        DeliveryRequest::NextSafePoint {
            expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa61)),
        },
    );
    let SubmitInputHandlingOutcome::Recorded(safe_result) = repository
        .handle(
            safe_source.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x962)),
            None,
        )
        .await?
    else {
        panic!("safe-point input must be recorded");
    };

    let semantic_pending_error = sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'origin_accepted_input', $3, NULL)",
    )
    .bind(Uuid::from_u128(0x861))
    .bind(Uuid::from_u128(0xd62))
    .bind(Uuid::from_u128(0x962))
    .execute(&pool)
    .await
    .expect_err("pending steering cannot establish a semantic turn origin");
    let semantic_pending_database_error = semantic_pending_error
        .as_database_error()
        .expect("deferred semantic-origin validation must return a database error");
    assert_eq!(semantic_pending_database_error.code(), Some("23514".into()));
    assert_eq!(
        semantic_pending_database_error.constraint(),
        Some("semantic_transcript_entry_origin_disposition")
    );

    let mut terminalize_source = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(Uuid::from_u128(0x861))
    .bind(Uuid::from_u128(0xd63))
    .bind(Uuid::from_u128(0xa61))
    .execute(&mut *terminalize_source)
    .await?;
    insert_frontier(
        &mut terminalize_source,
        Uuid::from_u128(0x861),
        Uuid::from_u128(0xe63),
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, Uuid::from_u128(0x861), Uuid::from_u128(0xd61)),
            (
                Decimal::from(2_u64),
                Uuid::from_u128(0x861),
                Uuid::from_u128(0xd63),
            ),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = 'without_stop',
                end_disposition = 'known_failure'
          WHERE turn_attempt_id = $1",
    )
    .bind(Uuid::from_u128(0xb61))
    .execute(&mut *terminalize_source)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                active_phase_kind = NULL,
                terminal_attempt_id = current_attempt_id,
                current_attempt_id = NULL,
                terminal_frontier_id = $1,
                terminal_disposition_kind = 'failed'
          WHERE turn_id = $2",
    )
    .bind(Uuid::from_u128(0xe63))
    .bind(Uuid::from_u128(0xa61))
    .execute(&mut *terminalize_source)
    .await?;
    let terminalize_source_error = terminalize_source
        .commit()
        .await
        .expect_err("pending steering must keep its source turn active");
    let terminalize_source_database_error = terminalize_source_error
        .as_database_error()
        .expect("deferred pending-source validation must return a database error");
    assert_eq!(
        terminalize_source_database_error.code(),
        Some("23514".into())
    );
    assert_eq!(
        terminalize_source_database_error.constraint(),
        Some("turn_lifecycle_pending_steering_closed")
    );

    repository
        .handle(
            input_with_delivery(
                0x464,
                0x861,
                "alternate lifecycle",
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa61)),
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x963)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa62))),
        )
        .await?;

    repository
        .handle(
            input_with_delivery(
                0x46a,
                0x861,
                "unknown alias rejection",
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa61)),
                    configuration: input_choices(
                        1,
                        ModelSelectionOverride::ReplaceWith(alias(0xc69)),
                    ),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x96a)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa6a))),
        )
        .await?;

    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x46b, 0x86b, direct(0xc6b)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x46c,
                0x86b,
                "other-session origin",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x96b)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa6b))),
        )
        .await?;

    for (command_id, source_turn, description) in [
        (
            Uuid::from_u128(0x46d),
            Uuid::from_u128(0xa6f),
            "missing source turn",
        ),
        (
            Uuid::from_u128(0x46e),
            Uuid::from_u128(0xa6b),
            "cross-session source turn",
        ),
    ] {
        let error = insert_cross_wired_occupied_rejection(
            &pool,
            command_id,
            Uuid::from_u128(0x46a),
            source_turn,
        )
        .await
        .expect_err(description);
        let database_error = error
            .as_database_error()
            .expect("deferred source-origin validation must return a database error");
        assert_eq!(database_error.code(), Some("23503".into()));
        assert_eq!(
            database_error.constraint(),
            Some("submit_input_command_rejected_source_origin")
        );
    }

    let new_constraints: Vec<String> = sqlx::query_scalar(
        "SELECT conname
           FROM pg_constraint
          WHERE conname IN (
                'accepted_input_pending_result_key',
                'accepted_input_expected_active_turn_fk',
                'accepted_input_general_command_result_fk',
                'submit_input_command_actual_active_turn_fk',
                'submit_input_command_pending_effect_fk',
                'submit_input_command_general_applied_effect_fk'
          )
          ORDER BY conname",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(new_constraints.len(), 6);

    let scheduling_support_indexes: Vec<(String, bool)> = sqlx::query_as(
        "SELECT
            indexname,
            indexdef LIKE
                CASE indexname
                    WHEN 'accepted_input_pending_by_source_turn'
                        THEN '%(session_id, expected_active_turn_id) WHERE (disposition_kind = ''pending_steering''::text)'
                    WHEN 'queued_input_origin_by_session_position'
                        THEN '%(session_id, acceptance_position)'
                END
           FROM pg_indexes
          WHERE schemaname = current_schema()
            AND indexname IN (
                'accepted_input_pending_by_source_turn',
                'queued_input_origin_by_session_position'
            )
          ORDER BY indexname",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        scheduling_support_indexes,
        vec![
            ("accepted_input_pending_by_source_turn".to_owned(), true),
            ("queued_input_origin_by_session_position".to_owned(), true),
        ]
    );

    let forbidden_configuration = sqlx::query(
        "INSERT INTO accepted_input
            (accepted_input_id, accepting_command_id, session_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             acceptance_position, disposition_kind, origin_turn_id)
         VALUES
            ($1, $2, $3, 'text', 'forbidden configuration',
             'next_safe_point', $4, 1, 'use_session_default',
             NULL, NULL, NULL, 4, 'pending_steering', NULL)",
    )
    .bind(Uuid::from_u128(0x969))
    .bind(Uuid::from_u128(0x469))
    .bind(Uuid::from_u128(0x861))
    .bind(Uuid::from_u128(0xa61))
    .execute(&pool)
    .await
    .expect_err("pending steering cannot persist origin configuration");
    assert_eq!(
        forbidden_configuration
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let extra_queue = sqlx::query(
        "INSERT INTO queued_input_origin
            (turn_id, accepted_input_id, session_id, acceptance_position,
             priority_kind, defaults_version,
             requested_model_kind, requested_direct_model_selection_id,
             requested_model_alias_id, frozen_model_kind,
             frozen_direct_model_selection_id, frozen_model_alias_id,
             frozen_alias_selected_direct_id, model_parameters,
             known_provider_failure_retry, model_fallback)
         VALUES
            ($1, $2, $3, 2, 'ordinary', 1,
             'direct', $4, NULL, 'direct', $4, NULL, NULL,
             'provider_defaults', 'disabled', 'disabled')",
    )
    .bind(Uuid::from_u128(0xf61))
    .bind(Uuid::from_u128(0x962))
    .bind(Uuid::from_u128(0x861))
    .bind(Uuid::from_u128(0xc61))
    .execute(&pool)
    .await
    .expect_err("pending steering cannot acquire a queued turn");
    assert_eq!(
        extra_queue
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23503".into())
    );

    let mut cross_wired = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'submit_input', 1, transaction_timestamp())",
    )
    .bind(Uuid::from_u128(0x466))
    .execute(&mut *cross_wired)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_actual_active_turn_id, result_expected_active_turn_id,
             result_expected_defaults_version, result_current_defaults_version,
             result_unknown_alias_id, result_selected_defaults_version,
             result_last_position)
         VALUES
            ($1, 'submit_input', 1, $2,
             'owner', NULL, NULL, 'text', 'cross-wired steering',
             'next_safe_point', $3, NULL, NULL, NULL, NULL, NULL,
             'applied', NULL, $2, $4, NULL, $3,
             NULL, NULL, NULL, NULL, NULL, NULL)",
    )
    .bind(Uuid::from_u128(0x466))
    .bind(Uuid::from_u128(0x861))
    .bind(Uuid::from_u128(0xa62))
    .bind(Uuid::from_u128(0x966))
    .execute(&mut *cross_wired)
    .await?;
    sqlx::query(
        "INSERT INTO accepted_input
            (accepted_input_id, accepting_command_id, session_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             acceptance_position, disposition_kind, origin_turn_id)
         VALUES
            ($1, $2, $3, 'text', 'cross-wired steering',
             'next_safe_point', $4, NULL, NULL, NULL, NULL, NULL,
             4, 'pending_steering', NULL)",
    )
    .bind(Uuid::from_u128(0x966))
    .bind(Uuid::from_u128(0x466))
    .bind(Uuid::from_u128(0x861))
    .bind(Uuid::from_u128(0xa61))
    .execute(&mut *cross_wired)
    .await?;
    let cross_wired_error = cross_wired
        .commit()
        .await
        .expect_err("command and pending acceptance must bind the same source turn");
    assert_eq!(
        cross_wired_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23503".into())
    );

    sqlx::query(
        "ALTER TABLE accepted_input
            DISABLE TRIGGER accepted_input_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE accepted_input
            DROP CONSTRAINT accepted_input_delivery_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE accepted_input
            SET disposition_kind = 'origin_of'
          WHERE accepted_input_id = $1",
    )
    .bind(Uuid::from_u128(0x962))
    .execute(&pool)
    .await?;
    let replayed = repository
        .load(safe_source.command_id())
        .await?
        .expect("mutable disposition cannot erase the immutable receipt");
    assert_eq!(replayed.result(), &safe_result);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S08 / INV-016: pending-steering acceptance and source terminalization
/// serialize on the source lifecycle row, so racing commits cannot both
/// succeed from snapshots in which the reciprocal effect is not yet visible.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv016_pending_steering_and_source_terminalization_serialize() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x471, 0x871, direct(0xc71)))
        .await?;
    let active_origin_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x971));
    let active_origin_turn = TurnId::from_uuid(Uuid::from_u128(0xa71));
    let repository = SubmitInputRepository::new(pool.clone());
    repository
        .handle(
            start_input(
                0x472,
                0x871,
                "active source",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            active_origin_input,
            Some(active_origin_turn),
        )
        .await?;
    let activated = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: Uuid::from_u128(0x871),
            origin_entry: Uuid::from_u128(0xd71),
            starting_frontier: Uuid::from_u128(0xe71),
            initial_attempt: Uuid::from_u128(0xb71),
        },
    )
    .await?;
    assert_eq!(activated.accepted_input().id(), active_origin_input);
    assert_eq!(activated.turn(), active_origin_turn);

    let mut terminalize_source = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(Uuid::from_u128(0x871))
    .bind(Uuid::from_u128(0xd72))
    .bind(Uuid::from_u128(0xa71))
    .execute(&mut *terminalize_source)
    .await?;
    insert_frontier(
        &mut terminalize_source,
        Uuid::from_u128(0x871),
        Uuid::from_u128(0xe72),
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, Uuid::from_u128(0x871), Uuid::from_u128(0xd71)),
            (
                Decimal::from(2_u64),
                Uuid::from_u128(0x871),
                Uuid::from_u128(0xd72),
            ),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = 'without_stop',
                end_disposition = 'known_failure'
          WHERE turn_attempt_id = $1",
    )
    .bind(Uuid::from_u128(0xb71))
    .execute(&mut *terminalize_source)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                active_phase_kind = NULL,
                terminal_attempt_id = current_attempt_id,
                current_attempt_id = NULL,
                terminal_frontier_id = $1,
                terminal_disposition_kind = 'failed'
          WHERE turn_id = $2",
    )
    .bind(Uuid::from_u128(0xe72))
    .bind(Uuid::from_u128(0xa71))
    .execute(&mut *terminalize_source)
    .await?;

    let pending_acceptance = tokio::spawn(async move {
        repository
            .handle(
                input_with_delivery(
                    0x473,
                    0x871,
                    "racing steering",
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa71)),
                    },
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x972)),
                None,
            )
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !pending_acceptance.is_finished(),
        "pending acceptance must wait for the source lifecycle row"
    );

    terminalize_source.commit().await?;
    let pending_error = pending_acceptance
        .await?
        .expect_err("steering must fail after racing source terminalization commits");
    let SubmitInputRepositoryError::Database(pending_database_error) = pending_error else {
        panic!("the rejected racing commit must report its database constraint");
    };
    assert_eq!(
        pending_database_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    assert_eq!(
        pending_database_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("accepted_input_pending_source_active")
    );

    let durable_effects: (i64, i64, String) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM durable_command WHERE command_id = $1),
            (SELECT count(*) FROM accepted_input WHERE accepted_input_id = $2),
            (SELECT state_kind FROM turn_lifecycle WHERE turn_id = $3)",
    )
    .bind(Uuid::from_u128(0x473))
    .bind(Uuid::from_u128(0x972))
    .bind(Uuid::from_u128(0xa71))
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_effects, (0, 0, "terminal".to_owned()));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S03 / S04 / INV-006 / INV-034: after a real pool restart, startup atomically
/// ends the prior-process attempt as Lost, retains it as attempt-only terminal
/// provenance, appends `TurnFailed`, terminalizes Failed, remains idempotent on
/// replay, and exposes the queued successor to the ordinary scheduler path.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s03_s04_inv006_inv034_restart_scan_recovers_lost_attempt_once_and_unblocks_successor()
-> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let session_uuid = Uuid::from_u128(0x7b1);
    let first_turn_uuid = Uuid::from_u128(0xab1);
    let second_turn_uuid = Uuid::from_u128(0xab2);
    let attempt_uuid = Uuid::from_u128(0xbb1);
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x3b0, 0x7b1, direct(0x8b1)))
        .await?;
    let inputs = SubmitInputRepository::new(pool.clone());
    inputs
        .handle(
            start_input(
                0x3b1,
                0x7b1,
                "prior process",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9b1)),
            Some(TurnId::from_uuid(first_turn_uuid)),
        )
        .await?;
    inputs
        .handle(
            start_input(
                0x3b2,
                0x7b1,
                "queued successor",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9b2)),
            Some(TurnId::from_uuid(second_turn_uuid)),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session_uuid,
            origin_entry: Uuid::from_u128(0xcb1),
            starting_frontier: Uuid::from_u128(0xdb1),
            initial_attempt: attempt_uuid,
        },
    )
    .await?;

    // Restart boundary: the active attempt exists durably, but its creating
    // process and every connection it owned are gone.
    drop(inputs);
    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let failure_entry_uuid = Uuid::from_u128(0xeb1);
    let terminal_frontier_uuid = Uuid::from_u128(0xfb1);
    let mut scan = StartupScanService::new(
        FixedStartupScanIds::new(
            [SemanticTranscriptEntryId::from_uuid(failure_entry_uuid)],
            [ContextFrontierId::from_uuid(terminal_frontier_uuid)],
        ),
        PostgresStartupScanRepository::new(restarted_pool.clone()),
    );

    let first = scan.execute().await?;
    assert!(first.is_complete());
    assert_eq!(first.recovered_turn_count(), 1);
    assert!(first.pending_steering_sessions().is_empty());

    let recovered: (
        String,
        String,
        String,
        String,
        String,
        Option<Uuid>,
        Uuid,
        Option<Uuid>,
    ) = sqlx::query_as(
        "SELECT attempt.state_kind,
                attempt.end_variant,
                attempt.end_disposition,
                turn.state_kind,
                turn.terminal_disposition_kind,
                turn.current_attempt_id,
                turn.terminal_attempt_id,
                turn.terminal_model_call_id
           FROM turn_attempt AS attempt
           JOIN turn_lifecycle AS turn
             ON turn.turn_id = attempt.turn_id
            AND turn.session_id = attempt.session_id
          WHERE attempt.turn_attempt_id = $1",
    )
    .bind(attempt_uuid)
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        recovered,
        (
            "ended".into(),
            "without_stop".into(),
            "lost".into(),
            "terminal".into(),
            "failed".into(),
            None,
            attempt_uuid,
            None,
        )
    );
    let terminal_entries = sqlx::query_scalar::<_, String>(
        "SELECT entry.payload_kind
           FROM context_frontier_member AS member
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
          WHERE member.owning_session_id = $1
            AND member.context_frontier_id = $2
          ORDER BY member.member_position",
    )
    .bind(session_uuid)
    .bind(terminal_frontier_uuid)
    .fetch_all(&restarted_pool)
    .await?;
    assert_eq!(terminal_entries, ["origin_accepted_input", "turn_failed"]);
    let recovery_events: Vec<(String, i16, Uuid, Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT header.event_kind,
                header.storage_version,
                header.session_id,
                failed.turn_id,
                failed.failure_entry_id,
                failed.terminal_frontier_id
           FROM outbox_event AS header
           JOIN turn_failed_outbox_event AS failed
             ON failed.event_sequence = header.event_sequence
          ORDER BY header.event_sequence",
    )
    .fetch_all(&restarted_pool)
    .await?;
    assert_eq!(
        recovery_events,
        vec![(
            "turn_failed".into(),
            1,
            session_uuid,
            first_turn_uuid,
            failure_entry_uuid,
            terminal_frontier_uuid,
        )]
    );
    let committed_counts_before_replay: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE payload_kind = 'turn_failed' AND failed_turn_id = $1),
            (SELECT count(*) FROM context_frontier
              WHERE owning_session_id = $2),
            (SELECT count(*) FROM turn_attempt
              WHERE turn_id = $1),
            (SELECT count(*) FROM outbox_event
              WHERE event_kind = 'turn_failed' AND session_id = $2),
            (SELECT count(*) FROM turn_failed_outbox_event
              WHERE turn_id = $1)",
    )
    .bind(first_turn_uuid)
    .bind(session_uuid)
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(committed_counts_before_replay, (1, 2, 1, 1, 1));

    let replay = scan.execute().await?;
    assert!(replay.is_complete());
    assert_eq!(replay.recovered_turn_count(), 0);
    let committed_counts_after_replay: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE payload_kind = 'turn_failed' AND failed_turn_id = $1),
            (SELECT count(*) FROM context_frontier
              WHERE owning_session_id = $2),
            (SELECT count(*) FROM turn_attempt
              WHERE turn_id = $1),
            (SELECT count(*) FROM outbox_event
              WHERE event_kind = 'turn_failed' AND session_id = $2),
            (SELECT count(*) FROM turn_failed_outbox_event
              WHERE turn_id = $1)",
    )
    .bind(first_turn_uuid)
    .bind(session_uuid)
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        committed_counts_after_replay,
        committed_counts_before_replay
    );

    let (eligible_sessions, continuation) = PostgresEligibilitySweep::new(restarted_pool.clone())
        .find_sessions()
        .await?
        .into_parts();
    assert!(!continuation);
    assert_eq!(eligible_sessions, vec![SessionId::from_uuid(session_uuid)]);
    let activated = activate_earliest_queued_turn(
        &restarted_pool,
        EarliestQueuedTurnActivation {
            session: session_uuid,
            origin_entry: Uuid::from_u128(0xcb2),
            starting_frontier: Uuid::from_u128(0xdb2),
            initial_attempt: Uuid::from_u128(0xbb2),
        },
    )
    .await?;
    assert_eq!(activated.turn(), TurnId::from_uuid(second_turn_uuid));
    assert_eq!(
        activated.start().lineage(),
        AcceptedInputStartingLineage::After {
            immediate_predecessor: TurnId::from_uuid(first_turn_uuid),
        }
    );

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S03 / INV-032 / INV-034: failure after the typed outbox append rolls the
/// complete Lost recovery back; retry then commits the state and event once.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s03_inv032_inv034_startup_recovery_and_outbox_commit_or_roll_back_together()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session_uuid = Uuid::from_u128(0x7d1);
    let turn_uuid = Uuid::from_u128(0xad1);
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x3d0, 0x7d1, direct(0x8d1)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x3d1,
                0x7d1,
                "active before failed recovery",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9d1)),
            Some(TurnId::from_uuid(turn_uuid)),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session_uuid,
            origin_entry: Uuid::from_u128(0xcd1),
            starting_frontier: Uuid::from_u128(0xdd1),
            initial_attempt: Uuid::from_u128(0xbd1),
        },
    )
    .await?;
    sqlx::query(
        "CREATE FUNCTION fail_test_turn_failed_outbox_commit()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $$
         BEGIN
             RAISE EXCEPTION 'injected failure after recovery outbox append'
                 USING ERRCODE = '40001';
         END;
         $$",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE CONSTRAINT TRIGGER zz_test_fail_turn_failed_outbox_commit
         AFTER INSERT ON turn_failed_outbox_event
         DEFERRABLE INITIALLY DEFERRED
         FOR EACH ROW
         EXECUTE FUNCTION fail_test_turn_failed_outbox_commit()",
    )
    .execute(&pool)
    .await?;

    let failure_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xed1));
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xfd1));
    let mut failing_scan = StartupScanService::new(
        FixedStartupScanIds::new([failure_entry], [terminal_frontier]),
        PostgresStartupScanRepository::new(pool.clone()),
    );
    failing_scan
        .execute()
        .await
        .expect_err("the deferred outbox fixture must abort recovery commit");

    let rolled_back: (String, String, i64, i64, Decimal) = sqlx::query_as(
        "SELECT turn.state_kind,
                attempt.state_kind,
                (SELECT count(*) FROM semantic_transcript_entry
                  WHERE failed_turn_id = $1),
                (SELECT count(*) FROM turn_failed_outbox_event
                  WHERE turn_id = $1),
                (SELECT last_sequence FROM outbox_sequence_state
                  WHERE singleton)
           FROM turn_lifecycle AS turn
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = turn.current_attempt_id
          WHERE turn.turn_id = $1",
    )
    .bind(turn_uuid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        rolled_back,
        ("active".into(), "prepared".into(), 0, 0, Decimal::ONE)
    );

    sqlx::query(
        "DROP TRIGGER zz_test_fail_turn_failed_outbox_commit
            ON turn_failed_outbox_event",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DROP FUNCTION fail_test_turn_failed_outbox_commit()")
        .execute(&pool)
        .await?;

    let mut retry_scan = StartupScanService::new(
        FixedStartupScanIds::new([failure_entry], [terminal_frontier]),
        PostgresStartupScanRepository::new(pool.clone()),
    );
    assert_eq!(retry_scan.execute().await?.recovered_turn_count(), 1);
    let committed: (String, String, i64, i64, Decimal) = sqlx::query_as(
        "SELECT turn.state_kind,
                attempt.state_kind,
                (SELECT count(*) FROM semantic_transcript_entry
                  WHERE failed_turn_id = $1),
                (SELECT count(*) FROM turn_failed_outbox_event
                  WHERE turn_id = $1),
                (SELECT last_sequence FROM outbox_sequence_state
                  WHERE singleton)
           FROM turn_lifecycle AS turn
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = $2
          WHERE turn.turn_id = $1",
    )
    .bind(turn_uuid)
    .bind(Uuid::from_u128(0xbd1))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        committed,
        ("terminal".into(), "ended".into(), 1, 1, Decimal::from(2))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[track_caller]
fn assert_restart_scan_visibly_defers_pending_steering(
    scan: &mut StartupScanService<FixedStartupScanIds, PostgresStartupScanRepository>,
    session: SessionId,
) -> impl Future<Output = Result<(), Box<dyn Error>>> + '_ {
    let caller = std::panic::Location::caller();
    async move {
        let outcome = scan.execute().await?;
        assert_eq!(
            outcome.recovered_turn_count(),
            0,
            "restart scan asserted at {caller} must not recover pending steering"
        );
        assert_eq!(
            outcome.pending_steering_sessions(),
            &[session],
            "restart scan asserted at {caller} must report its pending-steering blocker"
        );
        assert!(
            !outcome.is_complete(),
            "restart scan asserted at {caller} must remain incomplete"
        );
        Ok(())
    }
}

/// S08 / INV-016 / INV-034: a restart scan never treats pending steering as a
/// stop cause or silently drops it; the source remains active and each scan
/// returns the same visible session blocker without adding terminal facts.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s08_inv016_inv034_restart_scan_visibly_defers_pending_steering_unchanged()
-> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let session_uuid = Uuid::from_u128(0x7c1);
    let turn_uuid = Uuid::from_u128(0xac1);
    let attempt_uuid = Uuid::from_u128(0xbc1);
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x3c0, 0x7c1, direct(0x8c1)))
        .await?;
    let inputs = SubmitInputRepository::new(pool.clone());
    inputs
        .handle(
            start_input(
                0x3c1,
                0x7c1,
                "active source",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9c1)),
            Some(TurnId::from_uuid(turn_uuid)),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session_uuid,
            origin_entry: Uuid::from_u128(0xcc1),
            starting_frontier: Uuid::from_u128(0xdc1),
            initial_attempt: attempt_uuid,
        },
    )
    .await?;
    let pending_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9c2));
    let pending = inputs
        .handle(
            input_with_delivery(
                0x3c2,
                0x7c1,
                "steer later",
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: TurnId::from_uuid(turn_uuid),
                },
            ),
            pending_input,
            None,
        )
        .await?;
    assert!(matches!(
        pending,
        signalbox_persistence::submit_input::SubmitInputHandlingOutcome::Recorded(
            SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
        )
    ));

    drop(inputs);
    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let mut scan = StartupScanService::new(
        FixedStartupScanIds::new(
            [
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xec1)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xec2)),
            ],
            [
                ContextFrontierId::from_uuid(Uuid::from_u128(0xfc1)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xfc2)),
            ],
        ),
        PostgresStartupScanRepository::new(restarted_pool.clone()),
    );

    let session = SessionId::from_uuid(session_uuid);
    assert_restart_scan_visibly_defers_pending_steering(&mut scan, session).await?;
    assert_restart_scan_visibly_defers_pending_steering(&mut scan, session).await?;

    let recovery_events: (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM outbox_event
              WHERE event_kind = 'turn_failed' AND session_id = $1),
            (SELECT count(*) FROM turn_failed_outbox_event
              WHERE session_id = $1)",
    )
    .bind(session_uuid)
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(recovery_events, (0, 0));

    let unchanged: (String, String, i64, i64, i64) = sqlx::query_as(
        "SELECT turn.state_kind,
                attempt.state_kind,
                (SELECT count(*) FROM semantic_transcript_entry
                  WHERE payload_kind = 'turn_failed' AND failed_turn_id = $1),
                (SELECT count(*) FROM context_frontier
                  WHERE owning_session_id = $2),
                (SELECT count(*) FROM accepted_input
                  WHERE accepted_input_id = $3
                    AND disposition_kind = 'pending_steering')
           FROM turn_lifecycle AS turn
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = turn.current_attempt_id
          WHERE turn.turn_id = $1",
    )
    .bind(turn_uuid)
    .bind(session_uuid)
    .bind(pending_input.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(unchanged, ("active".into(), "prepared".into(), 0, 1, 1));
    assert_eq!(
        PostgresStartupScanRepository::new(restarted_pool.clone())
            .recover(
                SessionId::from_uuid(session_uuid),
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xec3)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(0xfc3)),
                ),
                |_| panic!("the no-call recovery must leave pending steering deferred"),
            )
            .await?,
        StartupScanSessionOutcome::DeferredPendingSteering {
            accepted_input: pending_input,
        }
    );

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / S03 / S08 / S09 / INV-001 / INV-008 / INV-012: occupied-slot
/// rejection evidence is recorded exactly, generated identities cannot reuse
/// the active origin, and the not-yet-supported matching interrupt path rolls
/// back its command claim and consumes no acceptance position.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn occupied_slot_rejections_and_matching_interrupt_rollback_are_exact()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x441, 0x841, direct(0xc41)))
        .await?;
    let active_origin_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x941));
    let active_origin_turn = TurnId::from_uuid(Uuid::from_u128(0xa41));
    let repository = SubmitInputRepository::new(pool.clone());
    repository
        .handle(
            start_input(
                0x442,
                0x841,
                "active origin",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            active_origin_input,
            Some(active_origin_turn),
        )
        .await?;
    let activated = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: Uuid::from_u128(0x841),
            origin_entry: Uuid::from_u128(0xd41),
            starting_frontier: Uuid::from_u128(0xe41),
            initial_attempt: Uuid::from_u128(0xb41),
        },
    )
    .await?;
    assert_eq!(activated.accepted_input().id(), active_origin_input);
    assert_eq!(activated.turn(), active_origin_turn);

    let active_start = start_input(
        0x443,
        0x841,
        "cannot start",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let active_start_outcome = repository
        .handle(
            active_start.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x942)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa42))),
        )
        .await?;
    assert!(matches!(
        active_start_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::ActiveTurnPresent {
                session,
                active_turn,
            }
        )) if session == SessionId::from_uuid(Uuid::from_u128(0x841))
            && active_turn == TurnId::from_uuid(Uuid::from_u128(0xa41))
    ));

    let stale_after = record_stale_active_input(
        &repository,
        0x444,
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xaff)),
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
        0x943,
        Some(0xa43),
    )
    .await?;
    let stale_safe_point = record_stale_active_input(
        &repository,
        0x445,
        DeliveryRequest::NextSafePoint {
            expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xaff)),
        },
        0x944,
        None,
    )
    .await?;
    let stale_interrupt = record_stale_active_input(
        &repository,
        0x446,
        DeliveryRequest::Interrupt {
            expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xaff)),
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
        0x945,
        Some(0xa45),
    )
    .await?;
    assert!(matches!(
        stale_after.1.clone(),
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::ActiveTurnMismatch {
                expected_active_turn,
                actual_active_turn,
                ..
            }
        )) if expected_active_turn == TurnId::from_uuid(Uuid::from_u128(0xaff))
            && actual_active_turn == TurnId::from_uuid(Uuid::from_u128(0xa41))
    ));
    assert!(matches!(
        stale_safe_point.1.clone(),
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::ActiveTurnMismatch {
                expected_active_turn,
                actual_active_turn,
                ..
            }
        )) if expected_active_turn == TurnId::from_uuid(Uuid::from_u128(0xaff))
            && actual_active_turn == TurnId::from_uuid(Uuid::from_u128(0xa41))
    ));
    assert!(matches!(
        stale_interrupt.1.clone(),
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::ActiveTurnMismatch {
                expected_active_turn,
                actual_active_turn,
                ..
            }
        )) if expected_active_turn == TurnId::from_uuid(Uuid::from_u128(0xaff))
            && actual_active_turn == TurnId::from_uuid(Uuid::from_u128(0xa41))
    ));

    let after_collision = active_origin_collision(
        &repository,
        &pool,
        0x449,
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa41)),
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
        Some(0xa49),
    )
    .await?;
    let safe_point_collision = active_origin_collision(
        &repository,
        &pool,
        0x44a,
        DeliveryRequest::NextSafePoint {
            expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa41)),
        },
        None,
    )
    .await?;
    assert!(matches!(
        after_collision.0,
        SubmitInputRepositoryError::AcceptedInputIdentityCollision {
            command_id,
            active_turn,
            accepted_input,
        } if command_id == DurableCommandId::from_uuid(Uuid::from_u128(0x449))
            && active_turn == TurnId::from_uuid(Uuid::from_u128(0xa41))
            && accepted_input == AcceptedInputId::from_uuid(Uuid::from_u128(0x941))
    ));
    assert_eq!(after_collision.1, 0);
    assert!(matches!(
        safe_point_collision.0,
        SubmitInputRepositoryError::AcceptedInputIdentityCollision {
            command_id,
            active_turn,
            accepted_input,
        } if command_id == DurableCommandId::from_uuid(Uuid::from_u128(0x44a))
            && active_turn == TurnId::from_uuid(Uuid::from_u128(0xa41))
            && accepted_input == AcceptedInputId::from_uuid(Uuid::from_u128(0x941))
    ));
    assert_eq!(safe_point_collision.1, 0);

    let matching_interrupt = input_with_delivery(
        0x447,
        0x841,
        "matching interrupt",
        DeliveryRequest::Interrupt {
            expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa41)),
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let error = repository
        .handle(
            matching_interrupt,
            AcceptedInputId::from_uuid(Uuid::from_u128(0x946)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa46))),
        )
        .await
        .expect_err("matching interrupt application is explicitly unavailable");
    assert!(matches!(
        error,
        SubmitInputRepositoryError::InterruptApplicationUnavailable {
            command_id,
            active_turn,
        } if command_id == DurableCommandId::from_uuid(Uuid::from_u128(0x447))
            && active_turn == TurnId::from_uuid(Uuid::from_u128(0xa41))
    ));
    let unclaimed: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM durable_command WHERE command_id = $1),
            (SELECT count(*) FROM submit_input_command WHERE command_id = $1),
            (SELECT count(*) FROM accepted_input WHERE accepting_command_id = $1),
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE origin_accepted_input_id = $2)",
    )
    .bind(Uuid::from_u128(0x447))
    .bind(Uuid::from_u128(0x946))
    .fetch_one(&pool)
    .await?;
    assert_eq!(unclaimed, (0, 0, 0, 0));

    let next = input_with_delivery(
        0x448,
        0x841,
        "position after interrupt rollback",
        DeliveryRequest::NextSafePoint {
            expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa41)),
        },
    );
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::PendingSteering(next),
    )) = repository
        .handle(
            next,
            AcceptedInputId::from_uuid(Uuid::from_u128(0x947)),
            None,
        )
        .await?
    else {
        panic!("a matching safe-point request must apply after interrupt rollback");
    };
    assert_eq!(next.acceptance_position().as_u64(), 2);

    let evidence: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            count(*) FILTER (
                WHERE rejection_kind = 'active_turn_present'
                  AND result_actual_active_turn_id = $1
            ),
            count(*) FILTER (
                WHERE rejection_kind = 'active_turn_mismatch'
                  AND result_expected_active_turn_id = $2
                  AND result_actual_active_turn_id = $1
            ),
            count(*) FILTER (
                WHERE rejection_kind IN (
                    'active_turn_present',
                    'active_turn_mismatch'
                )
                  AND result_accepted_input_id IS NULL
                  AND result_turn_id IS NULL
            )
          FROM submit_input_command
         WHERE command_id BETWEEN $3 AND $4",
    )
    .bind(Uuid::from_u128(0xa41))
    .bind(Uuid::from_u128(0xaff))
    .bind(Uuid::from_u128(0x443))
    .bind(Uuid::from_u128(0x446))
    .fetch_one(&pool)
    .await?;
    assert_eq!(evidence, (1, 3, 4));

    assert_eq!(
        repository
            .handle(
                active_start,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9ff)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xaff))),
            )
            .await?,
        active_start_outcome
    );
    assert_eq!(
        repository
            .handle(
                stale_after.0,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fe)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xafe))),
            )
            .await?,
        stale_after.1
    );
    assert_eq!(
        repository
            .handle(
                stale_safe_point.0,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fe)),
                None,
            )
            .await?,
        stale_safe_point.1
    );
    assert_eq!(
        repository
            .handle(
                stale_interrupt.0,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fe)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xafe))),
            )
            .await?,
        stale_interrupt.1
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-007 / INV-009 / INV-015: an incomplete frontier cannot expose any
/// semantic entry, start binding, slot owner, or attempt after rollback.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv007_inv009_inv015_malformed_atomic_start_rolls_back_every_fact()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x411, 0x811, direct(0xc11)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x412,
                0x811,
                "malformed future start",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x911)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa11))),
        )
        .await?;

    let session = Uuid::from_u128(0x811);
    let turn = Uuid::from_u128(0xa11);
    let mut malformed = pool.begin().await?;
    insert_origin_frontier(
        &mut malformed,
        session,
        Uuid::from_u128(0x911),
        Uuid::from_u128(0xd11),
        Uuid::from_u128(0xe11),
        Decimal::from(2_u64),
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xb11))
    .bind(turn)
    .bind(session)
    .execute(&mut *malformed)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'active',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                active_phase_kind = 'running',
                current_attempt_id = $2
          WHERE turn_id = $3",
    )
    .bind(Uuid::from_u128(0xe11))
    .bind(Uuid::from_u128(0xb11))
    .bind(turn)
    .execute(&mut *malformed)
    .await?;
    let incomplete = malformed
        .commit()
        .await
        .expect_err("a gapped one-member frontier must not commit");
    assert_eq!(
        incomplete
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let unchanged: (String, i64, i64, i64) = sqlx::query_as(
        "SELECT
            state_kind,
            (SELECT count(*) FROM semantic_transcript_entry),
            (SELECT count(*) FROM context_frontier),
            (SELECT count(*) FROM turn_attempt)
         FROM turn_lifecycle
         WHERE turn_id = $1",
    )
    .bind(turn)
    .fetch_one(&pool)
    .await?;
    assert_eq!(unchanged, ("queued".to_owned(), 0, 0, 0));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-001 / INV-005 / INV-006 / INV-009 / INV-015: the initial semantic variants
/// preserve globally unique identities and exact source correlations; eligible
/// failure records origin then failure without putting the later failure
/// marker in the starting frontier.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv005_inv006_inv009_inv015_initial_semantic_entries_are_turn_correlated()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x421, 0x821, direct(0xc21)))
        .await?;
    let submit = SubmitInputRepository::new(pool.clone());
    submit
        .handle(
            start_input(
                0x422,
                0x821,
                "will fail eligibility",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x921)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa21))),
        )
        .await?;

    let session = Uuid::from_u128(0x821);
    let turn = Uuid::from_u128(0xa21);
    let origin_entry = Uuid::from_u128(0xd21);
    let failure_entry = Uuid::from_u128(0xd22);
    let starting_frontier = Uuid::from_u128(0xe21);
    let terminal_frontier = Uuid::from_u128(0xe22);

    let mut missing_terminal_frontier = pool.begin().await?;
    insert_origin_frontier(
        &mut missing_terminal_frontier,
        session,
        Uuid::from_u128(0x921),
        origin_entry,
        starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(turn)
    .execute(&mut *missing_terminal_frontier)
    .await?;
    let missing_terminal_frontier_error = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                terminal_disposition_kind = 'failed'
          WHERE turn_id = $2",
    )
    .bind(starting_frontier)
    .bind(turn)
    .execute(&mut *missing_terminal_frontier)
    .await
    .expect_err("a failed terminal turn must name its terminal frontier");
    assert_eq!(
        missing_terminal_frontier_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_lifecycle_state_payload_shape")
    );
    missing_terminal_frontier.rollback().await?;

    let mut gapped_terminal_frontier = pool.begin().await?;
    insert_origin_frontier(
        &mut gapped_terminal_frontier,
        session,
        Uuid::from_u128(0x921),
        origin_entry,
        starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(turn)
    .execute(&mut *gapped_terminal_frontier)
    .await?;
    insert_frontier(
        &mut gapped_terminal_frontier,
        session,
        terminal_frontier,
        Decimal::from(3_u64),
        &[
            (Decimal::ONE, session, origin_entry),
            (Decimal::from(3_u64), session, failure_entry),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                terminal_frontier_id = $2,
                terminal_disposition_kind = 'failed'
          WHERE turn_id = $3",
    )
    .bind(starting_frontier)
    .bind(terminal_frontier)
    .bind(turn)
    .execute(&mut *gapped_terminal_frontier)
    .await?;
    let gapped = gapped_terminal_frontier
        .commit()
        .await
        .expect_err("a terminal frontier with a membership gap must not commit");
    assert_eq!(
        gapped.as_database_error().and_then(|error| error.code()),
        Some("23514".into())
    );

    let mut cross_wired_terminal_frontier = pool.begin().await?;
    insert_origin_frontier(
        &mut cross_wired_terminal_frontier,
        session,
        Uuid::from_u128(0x921),
        origin_entry,
        starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(turn)
    .execute(&mut *cross_wired_terminal_frontier)
    .await?;
    insert_frontier(
        &mut cross_wired_terminal_frontier,
        session,
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, session, failure_entry),
            (Decimal::from(2_u64), session, origin_entry),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                terminal_frontier_id = $2,
                terminal_disposition_kind = 'failed'
          WHERE turn_id = $3",
    )
    .bind(starting_frontier)
    .bind(terminal_frontier)
    .bind(turn)
    .execute(&mut *cross_wired_terminal_frontier)
    .await?;
    let cross_wired = cross_wired_terminal_frontier
        .commit()
        .await
        .expect_err("a reordered terminal frontier must not commit");
    assert_eq!(
        cross_wired
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let mut attempted_failure = pool.begin().await?;
    insert_origin_frontier(
        &mut attempted_failure,
        session,
        Uuid::from_u128(0x921),
        origin_entry,
        starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(turn)
    .execute(&mut *attempted_failure)
    .await?;
    insert_frontier(
        &mut attempted_failure,
        session,
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, session, origin_entry),
            (Decimal::from(2_u64), session, failure_entry),
        ],
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xb21))
    .bind(turn)
    .bind(session)
    .execute(&mut *attempted_failure)
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = 'without_stop',
                end_disposition = 'known_failure'
          WHERE turn_attempt_id = $1",
    )
    .bind(Uuid::from_u128(0xb21))
    .execute(&mut *attempted_failure)
    .await?;
    let attempted_failure_error = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                terminal_frontier_id = $2,
                terminal_disposition_kind = 'failed'
          WHERE turn_id = $3",
    )
    .bind(starting_frontier)
    .bind(terminal_frontier)
    .bind(turn)
    .execute(&mut *attempted_failure)
    .await
    .expect_err("a direct queued failure cannot carry an ended attempt");
    assert_eq!(
        attempted_failure_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_lifecycle_queued_failure_without_attempt")
    );
    attempted_failure.rollback().await?;

    let mut failure = pool.begin().await?;
    insert_origin_frontier(
        &mut failure,
        session,
        Uuid::from_u128(0x921),
        origin_entry,
        starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(turn)
    .execute(&mut *failure)
    .await?;
    insert_frontier(
        &mut failure,
        session,
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, session, origin_entry),
            (Decimal::from(2_u64), session, failure_entry),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                terminal_frontier_id = $2,
                terminal_disposition_kind = 'failed'
          WHERE turn_id = $3",
    )
    .bind(starting_frontier)
    .bind(terminal_frontier)
    .bind(turn)
    .execute(&mut *failure)
    .await?;
    failure.commit().await?;

    let semantic_shape: (String, i64, i64, i64, i64, i64, Option<Uuid>, Option<Uuid>) =
        sqlx::query_as(
            "SELECT
            turn.state_kind,
            (SELECT count(*)
               FROM semantic_transcript_entry
              WHERE source_session_id = $1),
            (SELECT count(*)
               FROM turn_attempt
              WHERE turn_id = $3),
            starting.member_count::bigint,
            terminal.member_count::bigint,
            (SELECT count(*)
               FROM context_frontier_member AS member
               JOIN semantic_transcript_entry AS entry
                 ON entry.source_session_id = member.source_session_id
                AND entry.semantic_entry_id = member.semantic_entry_id
              WHERE member.owning_session_id = $1
                AND member.context_frontier_id = $2
                AND entry.payload_kind = 'turn_failed'),
            turn.terminal_attempt_id,
            turn.terminal_model_call_id
         FROM turn_lifecycle AS turn
         JOIN context_frontier AS starting
           ON starting.owning_session_id = turn.session_id
          AND starting.context_frontier_id = turn.starting_frontier_id
         JOIN context_frontier AS terminal
           ON terminal.owning_session_id = turn.session_id
          AND terminal.context_frontier_id = turn.terminal_frontier_id
         WHERE turn.turn_id = $3",
        )
        .bind(session)
        .bind(starting_frontier)
        .bind(turn)
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        semantic_shape,
        ("terminal".to_owned(), 2, 0, 1, 2, 0, None, None)
    );

    let late_attempt = sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xb22))
    .bind(turn)
    .bind(session)
    .execute(&pool)
    .await
    .expect_err("an attempt cannot be inserted after direct terminalization");
    assert_eq!(
        late_attempt
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let overrun = sqlx::query(
        "INSERT INTO context_frontier_member
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES ($1, $2, 3, $1, $3)",
    )
    .bind(session)
    .bind(terminal_frontier)
    .bind(failure_entry)
    .execute(&pool)
    .await
    .expect_err("a committed frontier cannot grow beyond its declared count");
    assert_eq!(
        overrun
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("context_frontier_member_within_declared_count")
    );

    let trigger_inventory: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            count(*) FILTER (
                WHERE relation.relname = 'context_frontier'
                  AND candidate.tgname = 'context_frontier_requires_complete_membership'
                  AND candidate.tgdeferrable
            ),
            count(*) FILTER (
                WHERE relation.relname = 'context_frontier_member'
                  AND candidate.tgname = 'context_frontier_member_requires_complete_membership'
            ),
            count(*) FILTER (
                WHERE relation.relname = 'context_frontier_member'
                  AND candidate.tgname = 'context_frontier_member_stays_within_declared_count'
                  AND NOT candidate.tgdeferrable
            ),
            count(*) FILTER (
                WHERE relation.relname = 'context_frontier_member'
                  AND candidate.tgname = 'context_frontier_member_rechecks_declared_count'
                  AND candidate.tgdeferrable
            )
         FROM pg_trigger AS candidate
         JOIN pg_class AS relation
           ON relation.oid = candidate.tgrelid
         WHERE NOT candidate.tgisinternal",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(trigger_inventory, (1, 0, 1, 1));

    let index_inventory: (i64, i64) = sqlx::query_as(
        "SELECT
            count(*) FILTER (
                WHERE indexname = 'turn_attempt_by_turn_session'
                  AND indexdef LIKE '%(turn_id, session_id)%'
            ),
            count(*) FILTER (
                WHERE indexname = 'turn_lifecycle_by_session_position'
                  AND indexdef LIKE '%(session_id, acceptance_position)%'
            )
         FROM pg_indexes
         WHERE schemaname = current_schema()",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(index_inventory, (1, 1));

    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x424, 0x822, direct(0xc24)))
        .await?;
    submit
        .handle(
            start_input(
                0x425,
                0x822,
                "cross-session identity probe",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x924)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa24))),
        )
        .await?;
    let semantic_id_reuse = sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'origin_accepted_input', $3, NULL)",
    )
    .bind(Uuid::from_u128(0x822))
    .bind(origin_entry)
    .bind(Uuid::from_u128(0x924))
    .execute(&pool)
    .await
    .expect_err("a semantic entry identifier cannot be reused by another session");
    assert_eq!(
        semantic_id_reuse
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("semantic_transcript_entry_id_global")
    );

    let frontier_id_reuse = sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(Uuid::from_u128(0x822))
    .bind(starting_frontier)
    .execute(&pool)
    .await
    .expect_err("a context frontier identifier cannot be reused by another session");
    assert_eq!(
        frontier_id_reuse
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("context_frontier_id_global")
    );

    submit
        .handle(
            start_input(
                0x423,
                0x821,
                "still queued",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x922)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa22))),
        )
        .await?;

    let second_turn = Uuid::from_u128(0xa22);
    let second_origin = Uuid::from_u128(0xd23);
    let second_starting_frontier = Uuid::from_u128(0xe23);
    let second_attempt = Uuid::from_u128(0xb23);
    let mut omitted_predecessor_frontier = pool.begin().await?;
    insert_origin_frontier(
        &mut omitted_predecessor_frontier,
        session,
        Uuid::from_u128(0x922),
        second_origin,
        second_starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(second_attempt)
    .bind(second_turn)
    .bind(session)
    .execute(&mut *omitted_predecessor_frontier)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'active',
                start_lineage_kind = 'after',
                immediate_predecessor_turn_id = $1,
                starting_frontier_id = $2,
                active_phase_kind = 'running',
                current_attempt_id = $3
          WHERE turn_id = $4",
    )
    .bind(turn)
    .bind(second_starting_frontier)
    .bind(second_attempt)
    .bind(second_turn)
    .execute(&mut *omitted_predecessor_frontier)
    .await?;
    let omitted = omitted_predecessor_frontier
        .commit()
        .await
        .expect_err("a successor start cannot omit its predecessor terminal frontier");
    assert_eq!(
        omitted.as_database_error().and_then(|error| error.code()),
        Some("23514".into())
    );

    let mut reordered_predecessor_frontier = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'origin_accepted_input', $3, NULL)",
    )
    .bind(session)
    .bind(second_origin)
    .bind(Uuid::from_u128(0x922))
    .execute(&mut *reordered_predecessor_frontier)
    .await?;
    insert_frontier(
        &mut reordered_predecessor_frontier,
        session,
        second_starting_frontier,
        Decimal::from(3_u64),
        &[
            (Decimal::ONE, session, failure_entry),
            (Decimal::from(2_u64), session, origin_entry),
            (Decimal::from(3_u64), session, second_origin),
        ],
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(second_attempt)
    .bind(second_turn)
    .bind(session)
    .execute(&mut *reordered_predecessor_frontier)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'active',
                start_lineage_kind = 'after',
                immediate_predecessor_turn_id = $1,
                starting_frontier_id = $2,
                active_phase_kind = 'running',
                current_attempt_id = $3
          WHERE turn_id = $4",
    )
    .bind(turn)
    .bind(second_starting_frontier)
    .bind(second_attempt)
    .bind(second_turn)
    .execute(&mut *reordered_predecessor_frontier)
    .await?;
    let reordered = reordered_predecessor_frontier
        .commit()
        .await
        .expect_err("a successor start cannot reorder predecessor membership");
    assert_eq!(
        reordered.as_database_error().and_then(|error| error.code()),
        Some("23514".into())
    );

    let mut invalid_failure = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(Uuid::from_u128(0xd23))
    .bind(second_turn)
    .execute(&mut *invalid_failure)
    .await?;
    let queued_failure = invalid_failure
        .commit()
        .await
        .expect_err("a queued turn cannot acquire a failure entry");
    assert_eq!(
        queued_failure
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-009 / INV-015: direct queued failure and immutable frontier membership
/// remain closed under transactions that begin from stale concurrent snapshots.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv009_inv015_concurrent_attempt_and_frontier_inserts_fail_closed()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x451, 0x851, direct(0xc51)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x452,
                0x851,
                "concurrent static failure",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x951)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa51))),
        )
        .await?;

    let session = Uuid::from_u128(0x851);
    let turn = Uuid::from_u128(0xa51);
    let origin_entry = Uuid::from_u128(0xd51);
    let failure_entry = Uuid::from_u128(0xd52);
    let starting_frontier = Uuid::from_u128(0xe51);
    let terminal_frontier = Uuid::from_u128(0xe52);

    let mut terminalize = pool.begin().await?;
    insert_origin_frontier(
        &mut terminalize,
        session,
        Uuid::from_u128(0x951),
        origin_entry,
        starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(turn)
    .execute(&mut *terminalize)
    .await?;
    insert_frontier(
        &mut terminalize,
        session,
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, session, origin_entry),
            (Decimal::from(2_u64), session, failure_entry),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                terminal_frontier_id = $2,
                terminal_disposition_kind = 'failed'
          WHERE turn_id = $3",
    )
    .bind(starting_frontier)
    .bind(terminal_frontier)
    .bind(turn)
    .execute(&mut *terminalize)
    .await?;

    let concurrent_attempt = tokio::spawn({
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO turn_attempt
                    (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
                     state_kind, end_variant, end_disposition)
                 VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
            )
            .bind(Uuid::from_u128(0xb51))
            .bind(turn)
            .bind(session)
            .execute(&pool)
            .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !concurrent_attempt.is_finished(),
        "attempt insertion must serialize on the lifecycle row"
    );
    terminalize.commit().await?;
    let attempt_error = concurrent_attempt
        .await?
        .expect_err("an attempt racing direct terminalization must fail");
    assert_eq!(
        attempt_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    let attempt_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM turn_attempt WHERE turn_id = $1")
            .bind(turn)
            .fetch_one(&pool)
            .await?;
    assert_eq!(attempt_count, 0);

    let racing_frontier = Uuid::from_u128(0xe53);
    let mut header = pool.begin().await?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(session)
    .bind(racing_frontier)
    .execute(&mut *header)
    .await?;

    let mut member = pool.begin().await?;
    sqlx::query(
        "INSERT INTO context_frontier_member
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES ($1, $2, 1, $1, $3)",
    )
    .bind(session)
    .bind(racing_frontier)
    .bind(failure_entry)
    .execute(&mut *member)
    .await?;
    let concurrent_member = tokio::spawn(async move { member.commit().await });
    header.commit().await?;
    let member_error = concurrent_member
        .await?
        .expect_err("a member racing an uncommitted header must fail closed");
    assert!(matches!(
        member_error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503" | "23514")
    ));
    let member_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM context_frontier_member
          WHERE owning_session_id = $1
            AND context_frontier_id = $2",
    )
    .bind(session)
    .bind(racing_frontier)
    .fetch_one(&pool)
    .await?;
    assert_eq!(member_count, 0);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-008 / INV-012: all baseline authoritative rejections are typed
/// terminal records. Active-work delivery modes reject `NoActiveTurn`, stale
/// defaults and unresolved aliases retain their exact evidence, and missing
/// sessions create no aggregate or queued-work effects.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv008_inv012_submit_records_authoritative_rejections() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create = CreateSessionRepository::new(pool.clone());
    create.handle(prepared(0x311, 0x711, direct(0x811))).await?;
    create.handle(prepared(0x312, 0x712, alias(0x812))).await?;
    let repository = SubmitInputRepository::new(pool.clone());

    let missing = start_input(
        0x313,
        0x7ff,
        "missing",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let missing_recorded = repository
        .handle(
            missing.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x913)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa13))),
        )
        .await?;
    assert!(matches!(
        missing_recorded,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionNotFound { .. }
        ))
    ));
    create.handle(prepared(0x31a, 0x7ff, direct(0x81a))).await?;
    assert_eq!(
        repository
            .handle(
                missing,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x91a)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa1a))),
            )
            .await?,
        missing_recorded
    );

    let expected_turn = TurnId::from_uuid(Uuid::from_u128(0xb11));
    let active_modes = [
        DeliveryRequest::Interrupt {
            expected_active_turn: expected_turn,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
        DeliveryRequest::NextSafePoint {
            expected_active_turn: expected_turn,
        },
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: expected_turn,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    ];
    for (offset, delivery) in active_modes.into_iter().enumerate() {
        let turn = match delivery {
            DeliveryRequest::NextSafePoint { .. } => None,
            DeliveryRequest::Interrupt { .. } | DeliveryRequest::AfterCurrentTurn { .. } => {
                Some(TurnId::from_uuid(Uuid::from_u128(0xa14 + offset as u128)))
            }
            DeliveryRequest::StartWhenNoActiveTurn { .. } => {
                unreachable!("the table contains only active-work delivery modes")
            }
        };
        let command = input_with_delivery(0x314 + offset as u128, 0x711, "active", delivery);
        assert!(matches!(
            repository
                .handle(
                    command,
                    AcceptedInputId::from_uuid(Uuid::from_u128(0x914 + offset as u128)),
                    turn,
                )
                .await?,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
                SubmitInputRejectedResult::NoActiveTurn {
                    expected_active_turn: recorded,
                    ..
                }
            )) if recorded == expected_turn
        ));
    }

    let stale = start_input(
        0x318,
        0x711,
        "stale",
        2,
        ModelSelectionOverride::UseSessionDefault,
    );
    let stale_recorded = repository
        .handle(
            stale.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x918)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa18))),
        )
        .await?;
    assert!(matches!(
        stale_recorded,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                expected,
                current,
                ..
            }
        )) if expected.as_u64() == 2 && current.as_u64() == 1
    ));
    ReplaceSessionDefaultsRepository::new(pool.clone())
        .handle(replacement(0x31b, 0x711, 1, direct(0x81b)))
        .await?;
    assert_eq!(
        repository
            .handle(
                stale,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x91b)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa1b))),
            )
            .await?,
        stale_recorded
    );

    let unknown = start_input(
        0x319,
        0x712,
        "alias",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    assert!(matches!(
        repository
            .handle(
                unknown,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x919)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa19))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::UnknownModelAlias { alias, .. }
        )) if alias == ModelAlias::from_uuid(Uuid::from_u128(0x812))
    ));

    let explicit_unknown = start_input(
        0x31c,
        0x711,
        "explicit alias",
        2,
        ModelSelectionOverride::ReplaceWith(alias(0x81c)),
    );
    assert!(matches!(
        repository
            .handle(
                explicit_unknown,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x91c)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa1c))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::UnknownModelAlias { alias, .. }
        )) if alias == ModelAlias::from_uuid(Uuid::from_u128(0x81c))
    ));

    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM submit_input_command),
            (SELECT count(*) FROM accepted_input),
            (SELECT count(*) FROM queued_input_origin)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (7, 0, 0));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-007 / INV-008 / INV-012: the locked session row serializes concurrent
/// assignments into one gap-free position order, and a post-claim database
/// failure explicitly rolls back the claim and does not consume a position.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv007_inv008_inv012_submit_serializes_positions_and_rolls_back_failures()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x321, 0x721, direct(0x821)))
        .await?;
    let repository = SubmitInputRepository::new(pool.clone());
    let mut tasks = Vec::new();
    for offset in 0..6_u128 {
        let repository = repository.clone();
        tasks.push(tokio::spawn(async move {
            repository
                .handle(
                    start_input(
                        0x322 + offset,
                        0x721,
                        &format!("concurrent {offset}"),
                        1,
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                    AcceptedInputId::from_uuid(Uuid::from_u128(0x922 + offset)),
                    Some(TurnId::from_uuid(Uuid::from_u128(0xa22 + offset))),
                )
                .await
        }));
    }
    let mut positions = Vec::new();
    for task in tasks {
        let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(applied)) =
            task.await??
        else {
            panic!("each distinct concurrent command must apply");
        };
        positions.push(applied.acceptance_position().as_u64());
    }
    positions.sort_unstable();
    assert_eq!(positions, vec![1, 2, 3, 4, 5, 6]);

    let colliding = start_input(
        0x328,
        0x721,
        "collision",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let error = repository
        .handle(
            colliding.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x922)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa28))),
        )
        .await
        .expect_err("an accepted-input identity collision must abort the transaction");
    assert!(matches!(error, SubmitInputRepositoryError::Database(_)));
    assert!(repository.load(colliding.command_id()).await?.is_none());

    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(retried)) = repository
        .handle(
            colliding,
            AcceptedInputId::from_uuid(Uuid::from_u128(0x928)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa28))),
        )
        .await?
    else {
        panic!("retry after rollback must apply");
    };
    assert_eq!(retried.acceptance_position().as_u64(), 7);

    let equal = start_input(
        0x329,
        0x721,
        "equal concurrent replay",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (left, right) = tokio::join!(
        {
            let repository = repository.clone();
            let command = equal.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                repository
                    .handle(
                        command,
                        AcceptedInputId::from_uuid(Uuid::from_u128(0x929)),
                        Some(TurnId::from_uuid(Uuid::from_u128(0xa29))),
                    )
                    .await
            }
        },
        {
            let repository = repository.clone();
            let command = equal.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                repository
                    .handle(
                        command,
                        AcceptedInputId::from_uuid(Uuid::from_u128(0x92a)),
                        Some(TurnId::from_uuid(Uuid::from_u128(0xa2a))),
                    )
                    .await
            }
        }
    );
    let left = left?;
    let right = right?;
    assert_eq!(left, right);
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(equal_applied)) = left
    else {
        panic!("equal concurrent first handling must converge on an application");
    };
    assert_eq!(equal_applied.acceptance_position().as_u64(), 8);
    let equal_counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM submit_input_command WHERE command_id = $1),
            (SELECT count(*) FROM accepted_input WHERE accepting_command_id = $1),
            (SELECT count(*)
               FROM queued_input_origin AS queued
               JOIN accepted_input AS accepted
                 ON accepted.accepted_input_id = queued.accepted_input_id
              WHERE accepted.accepting_command_id = $1),
            (SELECT count(*)
               FROM turn_lifecycle AS turn
               JOIN accepted_input AS accepted
                 ON accepted.origin_turn_id = turn.turn_id
              WHERE accepted.accepting_command_id = $1)",
    )
    .bind(Uuid::from_u128(0x329))
    .fetch_one(&pool)
    .await?;
    assert_eq!(equal_counts, (1, 1, 1, 1));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-007 / INV-008 / INV-012: a defaults replacement holds the pointer-row
/// lock when its version-row insert requests `FOR KEY SHARE` on the session
/// row through the non-deferrable session foreign key, while submit orders
/// the session row before the pointer row. The forced interleaving completes
/// with typed outcomes because submit's session-row lock is
/// `FOR NO KEY UPDATE`; `FOR UPDATE` deadlocks here (Postgres 40P01).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv007_inv008_inv012_submit_and_defaults_replacement_interleave_without_deadlock()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x341, 0x751, direct(0x851)))
        .await?;

    // Replacement side, first half: hold the pointer-row lock exactly as the
    // defaults-replacement compare-and-set does before its version insert.
    // The pointer's version foreign key is deferred, so the successor row may
    // follow the pointer change inside the same transaction.
    let mut replacement_side = pool.begin().await?;
    let cas = sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 2
         WHERE session_id = $1
           AND current_version = 1",
    )
    .bind(Uuid::from_u128(0x751))
    .execute(&mut *replacement_side)
    .await?;
    assert_eq!(cas.rows_affected(), 1);

    // Submit side: locks the session row, then blocks on the held pointer.
    let submit = tokio::spawn({
        let repository = SubmitInputRepository::new(pool.clone());
        async move {
            repository
                .handle(
                    start_input(
                        0x342,
                        0x751,
                        "interleaved",
                        1,
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                    AcceptedInputId::from_uuid(Uuid::from_u128(0x942)),
                    Some(TurnId::from_uuid(Uuid::from_u128(0xa42))),
                )
                .await
        }
    });

    // Force the interleaving: proceed only once the submit transaction holds
    // its session-row lock and waits on the pointer row.
    let mut submit_blocked_on_pointer = false;
    for _ in 0..400 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*)
             FROM pg_stat_activity
             WHERE wait_event_type = 'Lock'
               AND query LIKE '%FROM session_current_defaults%'",
        )
        .fetch_one(&pool)
        .await?;
        if waiting > 0 {
            submit_blocked_on_pointer = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        submit_blocked_on_pointer,
        "the submit transaction must block on the held pointer row"
    );

    // Replacement side, second half: the insert's session foreign key takes
    // `FOR KEY SHARE` on the session row the submit transaction has locked.
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, 2, 'direct', $2, NULL)",
    )
    .bind(Uuid::from_u128(0x751))
    .bind(Uuid::from_u128(0x852))
    .execute(&mut *replacement_side)
    .await?;
    replacement_side.commit().await?;

    // The unblocked submit records the advanced pointer as a typed stale
    // rejection rather than failing on infrastructure.
    assert!(matches!(
        submit.await??,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                expected,
                current,
                ..
            }
        )) if expected.as_u64() == 1 && current.as_u64() == 2
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-002 / INV-008 / INV-012: checked loads reject cross-wired immutable
/// effects even when database protections are deliberately disabled, and the
/// maximum stored position produces a durable exhaustion rejection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv002_inv008_inv012_submit_corruption_and_position_exhaustion_fail_closed()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x331, 0x731, direct(0x831)))
        .await?;
    let repository = SubmitInputRepository::new(pool.clone());
    let first = start_input(
        0x332,
        0x731,
        "uncorrupted",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    repository
        .handle(
            first.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x932)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa32))),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE submit_input_command
            DISABLE TRIGGER submit_input_command_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE submit_input_command
            SET actor_kind = 'recovery'
          WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x332))
    .execute(&pool)
    .await?;
    let non_owner = repository
        .load(first.command_id())
        .await
        .expect_err("domain reconstitution rejects a stored non-owner actor");
    assert!(matches!(
        non_owner,
        SubmitInputRepositoryError::Corruption(SubmitInputCorruption::Domain(
            SubmitInputReconstitutionFailure::StoredActorMismatch
        ))
    ));
    sqlx::query(
        "UPDATE submit_input_command
            SET actor_kind = 'owner'
          WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x332))
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE accepted_input
            DISABLE TRIGGER accepted_input_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE queued_input_origin
            DISABLE TRIGGER queued_input_origin_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE queued_input_origin
            DROP CONSTRAINT queued_input_origin_accepted_input_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE accepted_input
            DROP CONSTRAINT accepted_input_queued_origin_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE turn_lifecycle
            DROP CONSTRAINT turn_lifecycle_queued_origin_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE queued_input_origin
            DROP CONSTRAINT queued_input_origin_turn_lifecycle_fk",
    )
    .execute(&pool)
    .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE accepted_input
            SET acceptance_position = 18446744073709551615
          WHERE accepting_command_id = $1",
    )
    .bind(Uuid::from_u128(0x332))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE queued_input_origin
            SET acceptance_position = 18446744073709551615
          WHERE accepted_input_id = $1",
    )
    .bind(Uuid::from_u128(0x932))
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    let exhausted = start_input(
        0x333,
        0x731,
        "exhausted",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    assert!(matches!(
        repository
            .handle(
                exhausted,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x933)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa33))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::AcceptancePositionExhausted { last, .. }
        )) if last.as_u64() == u64::MAX
    ));

    sqlx::query(
        "UPDATE accepted_input
            SET content_text = 'cross-wired'
          WHERE accepting_command_id = $1",
    )
    .bind(Uuid::from_u128(0x332))
    .execute(&pool)
    .await?;
    let corrupt = repository
        .load(first.command_id())
        .await
        .expect_err("domain correlation rejects altered accepted content");
    assert!(matches!(
        corrupt,
        SubmitInputRepositoryError::Corruption(SubmitInputCorruption::Domain(
            SubmitInputReconstitutionFailure::AcceptedContentMismatch
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24 / INV-032: the transactional allocator holds its singleton row through
/// commit, so a concurrent event cannot obtain the next sequence and commit
/// ahead of the lower event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_inv032_outbox_sequences_follow_concurrent_commit_order() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first_session = insert_outbox_session_fixture(&pool, 0xe11).await?;
    let second_session = insert_outbox_session_fixture(&pool, 0xe12).await?;

    let mut first_transaction = pool.begin().await?;
    let first_sequence =
        append_session_created_test_event(&mut first_transaction, first_session).await?;
    let second = tokio::spawn({
        let pool = pool.clone();
        async move {
            let mut transaction = pool.begin().await?;
            let sequence =
                append_session_created_test_event(&mut transaction, second_session).await?;
            transaction.commit().await?;
            Ok::<_, sqlx::Error>(sequence)
        }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the higher-sequence allocator must wait for the lower transaction"
    );

    first_transaction.commit().await?;
    let second_sequence = second.await??;
    assert_eq!(first_sequence, Decimal::ONE);
    assert_eq!(second_sequence, Decimal::from(2));

    let committed: Vec<(Decimal, Uuid)> = sqlx::query_as(
        "SELECT event_sequence, session_id
           FROM outbox_event
          ORDER BY event_sequence",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        committed,
        vec![
            (first_sequence, first_session),
            (second_sequence, second_session),
        ]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24 / INV-032: delivery cannot advance to an uncommitted allocation, and a
/// later concurrent allocation remains a suffix after the committed prefix is
/// marked delivered.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_inv032_outbox_delivery_prefix_is_stable() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first_session = insert_outbox_session_fixture(&pool, 0xe13).await?;
    let second_session = insert_outbox_session_fixture(&pool, 0xe14).await?;

    let mut first_transaction = pool.begin().await?;
    let first_sequence =
        append_session_created_test_event(&mut first_transaction, first_session).await?;
    let (allocated_sender, allocated_receiver) = tokio::sync::oneshot::channel();
    let (commit_sender, commit_receiver) = tokio::sync::oneshot::channel();
    let second = tokio::spawn({
        let pool = pool.clone();
        async move {
            let mut transaction = pool.begin().await?;
            let sequence =
                append_session_created_test_event(&mut transaction, second_session).await?;
            allocated_sender
                .send(sequence)
                .expect("the prefix test receives the second allocation");
            commit_receiver
                .await
                .expect("the prefix test releases the second commit");
            transaction.commit().await?;
            Ok::<_, sqlx::Error>(sequence)
        }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the second allocation must wait while the first is uncommitted"
    );

    let invisible_events: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox_event")
        .fetch_one(&pool)
        .await?;
    assert_eq!(invisible_events, 0);
    let uncommitted_delivery = sqlx::query(
        "UPDATE outbox_delivery_state
            SET delivered_through = $1
          WHERE singleton",
    )
    .bind(first_sequence)
    .execute(&pool)
    .await
    .expect_err("an uncommitted sequence is not a deliverable prefix");
    assert_eq!(
        uncommitted_delivery
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    first_transaction.commit().await?;
    let second_sequence = allocated_receiver.await?;
    let visible_sequences: Vec<Decimal> = sqlx::query_scalar(
        "SELECT event_sequence
           FROM outbox_event
          ORDER BY event_sequence",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(visible_sequences, vec![first_sequence]);

    sqlx::query(
        "UPDATE outbox_delivery_state
            SET delivered_through = $1
          WHERE singleton",
    )
    .bind(first_sequence)
    .execute(&pool)
    .await?;
    commit_sender
        .send(())
        .expect("the prefix test still awaits the second commit");
    assert_eq!(second.await??, second_sequence);

    let undelivered_suffix: Vec<Decimal> = sqlx::query_scalar(
        "SELECT event.event_sequence
           FROM outbox_event AS event
           CROSS JOIN outbox_delivery_state AS delivery
          WHERE delivery.singleton
            AND event.event_sequence > delivery.delivered_through
          ORDER BY event.event_sequence",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(first_sequence, Decimal::ONE);
    assert_eq!(second_sequence, Decimal::from(2));
    assert_eq!(undelivered_suffix, vec![second_sequence]);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24 / INV-032: an event-producing transaction cannot mark its own
/// uncommitted event delivered and thereby make restart recovery skip it.
/// Both append-before-delivery and delivery-before-append orderings are covered.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_inv032_outbox_delivery_rejects_event_producing_transaction()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    insert_outbox_session_fixture(&pool, 0xe15).await?;
    insert_outbox_session_fixture(&pool, 0xe16).await?;

    let mut event_transaction = pool.begin().await?;
    let sequence =
        append_session_created_test_event(&mut event_transaction, Uuid::from_u128(0xe15)).await?;
    let same_transaction_delivery = sqlx::query(
        "UPDATE outbox_delivery_state
            SET delivered_through = $1
          WHERE singleton",
    )
    .bind(sequence)
    .execute(&mut *event_transaction)
    .await
    .expect_err("an event-producing transaction cannot deliver its own event");
    assert_eq!(
        same_transaction_delivery
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    event_transaction.rollback().await?;

    let rolled_back: (Decimal, i64) = sqlx::query_as(
        "SELECT
            (SELECT delivered_through
               FROM outbox_delivery_state
              WHERE singleton),
            (SELECT count(*)
               FROM outbox_event)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(rolled_back, (Decimal::ZERO, 0));

    let mut committed_event = pool.begin().await?;
    let sequence =
        append_session_created_test_event(&mut committed_event, Uuid::from_u128(0xe15)).await?;
    committed_event.commit().await?;

    let mut delivery_then_event = pool.begin().await?;
    sqlx::query(
        "UPDATE outbox_delivery_state
            SET delivered_through = $1
          WHERE singleton",
    )
    .bind(sequence)
    .execute(&mut *delivery_then_event)
    .await?;
    let delivery_first_append =
        append_session_created_test_event(&mut delivery_then_event, Uuid::from_u128(0xe16))
            .await
            .expect_err("delivery and later event append cannot share one transaction");
    assert_eq!(
        delivery_first_append
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    delivery_then_event.rollback().await?;

    let after_delivery_first_rollback: (Decimal, i64) = sqlx::query_as(
        "SELECT
            (SELECT delivered_through
               FROM outbox_delivery_state
              WHERE singleton),
            (SELECT count(*)
               FROM outbox_event)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(after_delivery_first_rollback, (Decimal::ZERO, 1));

    sqlx::query(
        "UPDATE outbox_delivery_state
            SET delivered_through = $1
          WHERE singleton",
    )
    .bind(sequence)
    .execute(&pool)
    .await?;
    let delivered_through: Decimal = sqlx::query_scalar(
        "SELECT delivered_through
           FROM outbox_delivery_state
          WHERE singleton",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(delivered_through, sequence);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-032: the durable sequence, prefix, header, and typed-record tables cannot
/// bypass their row-level guards through PostgreSQL's statement-level truncate.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv032_outbox_storage_rejects_truncate() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE outbox_sequence_state CASCADE").await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE outbox_delivery_state CASCADE").await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE outbox_event CASCADE").await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE session_created_outbox_event CASCADE")
        .await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE turn_failed_outbox_event CASCADE")
        .await?;

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-032: a deferred failure after the production append rolls the
/// CreateSession state, event, and sequence allocation back together; retry
/// commits all three together.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv032_create_session_and_outbox_commit_or_roll_back_together()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    sqlx::query(
        "CREATE FUNCTION fail_test_session_created_outbox_commit()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $$
         BEGIN
             RAISE EXCEPTION 'injected failure after outbox append'
                 USING ERRCODE = '40001';
         END;
         $$",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE CONSTRAINT TRIGGER zz_test_fail_session_created_outbox_commit
         AFTER INSERT ON session_created_outbox_event
         DEFERRABLE INITIALLY DEFERRED
         FOR EACH ROW
         EXECUTE FUNCTION fail_test_session_created_outbox_commit()",
    )
    .execute(&pool)
    .await?;

    let repository = CreateSessionRepository::new(pool.clone());
    let creation = prepared(0xe31, 0xe41, direct(0xe51));
    let command_id = creation.command().command_id().into_uuid();
    let session_id = creation.applied_result().session().into_uuid();
    let error = repository
        .handle(creation)
        .await
        .expect_err("the deferred fixture failure must abort commit");
    assert!(matches!(error, CreateSessionRepositoryError::Database(_)));
    let rolled_back: (i64, i64, i64, i64, Decimal) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM durable_command
              WHERE command_id = $1),
            (SELECT count(*)
               FROM session
              WHERE session_id = $2),
            (SELECT count(*)
               FROM outbox_event
              WHERE session_id = $2),
            (SELECT count(*)
               FROM session_created_outbox_event
              WHERE session_id = $2),
            (SELECT last_sequence
               FROM outbox_sequence_state
              WHERE singleton)",
    )
    .bind(command_id)
    .bind(session_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(rolled_back, (0, 0, 0, 0, Decimal::ZERO));

    sqlx::query(
        "DROP TRIGGER zz_test_fail_session_created_outbox_commit
            ON session_created_outbox_event",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DROP FUNCTION fail_test_session_created_outbox_commit()")
        .execute(&pool)
        .await?;

    assert_eq!(
        repository.handle(creation).await?,
        CreateSessionHandlingOutcome::Applied(creation.applied_result())
    );
    let committed: (i64, i64, i64, i64, Decimal) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM durable_command
              WHERE command_id = $1),
            (SELECT count(*)
               FROM session
              WHERE session_id = $2),
            (SELECT count(*)
               FROM outbox_event
              WHERE session_id = $2),
            (SELECT count(*)
               FROM session_created_outbox_event
              WHERE session_id = $2),
            (SELECT last_sequence
               FROM outbox_sequence_state
              WHERE singleton)",
    )
    .bind(command_id)
    .bind(session_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(committed, (1, 1, 1, 1, Decimal::ONE));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-012 / INV-032: only first committed handling emits the creation
/// event; equal replay and conflicting identifier reuse append nothing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv012_inv032_create_session_first_handling_appends_exactly_once()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = CreateSessionRepository::new(pool.clone());
    let creation = prepared(0xe32, 0xe42, direct(0xe52));

    assert_eq!(
        repository.handle(creation).await?,
        CreateSessionHandlingOutcome::Applied(creation.applied_result())
    );
    assert_eq!(
        repository.handle(creation).await?,
        CreateSessionHandlingOutcome::Applied(creation.applied_result())
    );
    assert_eq!(
        repository
            .handle(prepared(0xe32, 0xe43, direct(0xe53)))
            .await?,
        CreateSessionHandlingOutcome::ConflictingReuse {
            command_id: creation.command().command_id(),
        }
    );

    let events: Vec<(Decimal, String, i16, Uuid)> = sqlx::query_as(
        "SELECT event_sequence, event_kind, storage_version, session_id
           FROM outbox_event
          ORDER BY event_sequence",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        events,
        vec![(
            Decimal::ONE,
            "session_created".to_owned(),
            1,
            creation.applied_result().session().into_uuid(),
        )]
    );
    let typed_events: i64 = sqlx::query_scalar("SELECT count(*) FROM session_created_outbox_event")
        .fetch_one(&pool)
        .await?;
    assert_eq!(typed_events, 1);

    pool.close().await;
    drop(container);
    Ok(())
}
