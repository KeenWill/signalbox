//! Feature-gated PostgreSQL coverage for migrations, durable invariants, and repository composition.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics, explicit fixture expectations, and impossible fixture branches; the workspace gate remains active for production targets"
)]

mod support;

use std::{
    collections::{HashSet, VecDeque},
    error::Error,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use rust_decimal::Decimal;
use signalbox_application::{
    AuthorizeModelCallOutcome, AuthorizeModelCallTransaction,
    CommitModelCallObservationTransaction, CreateSessionError, CreateSessionOutcome,
    CreateSessionRequest, CreateSessionService, EligibilityNudge, EligibilityNudgeOutcome,
    EligibilitySweep, InProcessAttemptDispatchGate, LoadSessionService,
    ModelCallAuthorizationReread, ModelCallCredentialReference, ModelCallExecutionError,
    ModelCallExecutionIdGenerator, ModelCallExecutionOutcome, ModelCallExecutionService,
    ModelConversationMessage, PromptMemberStatement, ReplaceSessionDefaultsOutcome,
    ReplaceSessionDefaultsRequest, ReplaceSessionDefaultsService, RetainedCapabilityFailureStatus,
    RetainedModelCallObservationStatus, ScriptedModelCallProvider, ScriptedModelCallStep,
    SessionIdGenerator, StartEligibleTurnIdGenerator, StartEligibleTurnOutcome,
    StartEligibleTurnService, StartupScanIdGenerator, StartupScanService,
    StartupScanSessionOutcome, SubmitInputIdGenerator, SubmitInputOutcome, SubmitInputRequest,
    SubmitInputRequestError, SubmitInputService, ToolAttemptAuthorizationStatus,
};
use signalbox_domain::{
    AcceptedInputId, AcceptedInputStartingLineage, AcceptedInputTurnActivationIdentities,
    ActivatedAcceptedInputTurn, ActiveTurnPhase, AmbiguousModelCallTurnIdentities,
    AssistantResponsePart, AssistantText, AuthorizedModelCall, CancelledModelCallTurnIdentities,
    CompletedModelCallIdentities, ContextFrontierId, CorrelatedModelCallTerminalObservation,
    CreateSession, CurrentToolAttemptState, CurrentTurnAttemptState, DecideToolRequest,
    DecideToolRequestResult, DeliveryRequest, DirectModelSelection, DurableCommandId,
    FailedModelCallTurnIdentities, InitialToolApproval, ModelAlias, ModelCallId,
    ModelCallTerminalIdentities, ModelCallTerminalObservation, ModelCallTerminalOutcome,
    ModelSelectionOverride, ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition,
    NormalizedToolArguments, PerInputConfigurationChoices,
    PhysicalCancellationModelCallTurnIdentities, PreparedCreateSession, PreparedModelCallRequest,
    ProviderModelCallFailureCause, ProviderModelIdentity, ProviderReportedTokenUsage,
    RefusedModelCallTurnIdentities, ReplaceSessionDefaults, ReplaceSessionDefaultsRejectedResult,
    ReplaceSessionDefaultsResult, ResolvedProviderTarget, SemanticTranscriptEntryId,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionCreationCause,
    SessionCreationProvenance, SessionId, SessionInputPosition, SessionSystemPrompt,
    SessionTemplateContentDigest, SessionTemplateName, SessionTemplateProvenance,
    StoppedToolResponsePartIdentity, StoppedToolRoundModelCallIdentities, SubmitInput,
    SubmitInputAppliedResult, SubmitInputReconstitutionFailure, SubmitInputRejectedResult,
    SubmitInputResult, ToolApprovalDecider, ToolApprovalDecision, ToolApprovalResolution,
    ToolAttemptCrashOutcome, ToolAttemptEnd, ToolAttemptId, ToolAttemptObservation,
    ToolBatchExecutionFailure, ToolCallProposal, ToolEffectClass, ToolExecutionError,
    ToolExecutionErrorKind, ToolName, ToolRequestId, ToolResponsePartIdentity, ToolResultContent,
    ToolResultText, ToolRoundModelCallIdentities, ToolUsingAssistantResponse, TranscriptAncestry,
    TurnAttemptId, TurnConfigurationProvenance, TurnId, UserContent,
};
use signalbox_persistence::{
    MIGRATOR, ModelCredentialFamilyCatalog,
    create_session::{
        CreateSessionCorruption, CreateSessionHandlingOutcome, CreateSessionRepository,
        CreateSessionRepositoryError,
    },
    create_session_from_imported_frontier::{
        ImportedSessionRepository, ImportedSessionRepositoryError,
    },
    local_test_connection_options, migrate,
    model_execution::{
        ModelCallCorruption, ModelCallIdentityCollision, ModelCallRepositoryError,
        PostgresModelCallRepository, PrepareInitialModelCallOutcome,
    },
    outbox::{
        DispatchedModelCallState, DispatchedOutboxEvent, DispatchedOutboxEventKind,
        DispatchedReconciliationOperation, DispatchedToolBatchState, OutboxCorruption,
        OutboxDeliveryDecision, OutboxDispatchError, OutboxDispatchOutcome, OutboxDispatcher,
    },
    plan::{SessionPlanCorruption, SessionPlanRepository, SessionPlanRepositoryError},
    process_read::{
        ProcessCurrentModelCallState, ProcessFailedModelCallDisposition,
        ProcessModelCallInputTokenSemantics, ProcessModelCallRecoveryPrecondition,
        ProcessModelCallUsageProvenance, ProcessModelSelection,
        ProcessProviderModelCallFailureCause, ProcessReadCorruption, ProcessReadError,
        ProcessReadRepository, ProcessReconciliationOperation, ProcessSessionDefaultsRead,
        ProcessTranscriptEntry, ProcessTurnState,
    },
    replace_session_defaults::{
        ReplaceSessionDefaultsCorruption, ReplaceSessionDefaultsHandlingOutcome,
        ReplaceSessionDefaultsRepository, ReplaceSessionDefaultsRepositoryError,
    },
    scheduler::PostgresEligibilitySweep,
    session::{SessionCorruption, SessionRepository, SessionRepositoryError},
    session_credentials::{
        SessionCredentialPin, SessionModelCredential, current_session_credential,
    },
    start_eligible_turn::{
        CommitActivationPreviewOutcome, StartEligibleTurnCorruption,
        StartEligibleTurnIdentityCollision, StartEligibleTurnRepository,
        StartEligibleTurnRepositoryError,
    },
    startup::PostgresStartupScanRepository,
    submit_input::{
        SubmitInputCorruption, SubmitInputHandlingOutcome, SubmitInputRepository,
        SubmitInputRepositoryError,
    },
    tool_loop::{PostgresToolLoopRepository, ToolLoopRepositoryError},
};
use signalbox_tools_plan::{
    PlanAppendOutcome, PlanAppendRejection, PlanAppendRequest, PlanEntryId, PlanEvent,
    PlanEventDraft, PlanEventProvenance, PlanPageCompleteness, PlanReadRequest, PlanStatus,
    PlanText,
};
use sqlx::{PgConnection, PgPool, Row, migrate::Migrate, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

use support::blocked_backends_reached;

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_integration";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

fn test_session_credential_pin() -> signalbox_persistence::SessionCredentialPin {
    signalbox_persistence::SessionCredentialPin::try_new(vec![
        signalbox_persistence::SessionModelCredential::new(
            "test-model-family",
            "test-model-primary",
        ),
    ])
    .expect("test credential pin is valid")
}

fn model_credential_reference() -> ModelCallCredentialReference {
    ModelCallCredentialReference::new("fixture-provider-primary")
}

/// The creation pin is event 1, equal replay never rereads a changed pin, and
/// current credentials are selected only by append-and-head advancement.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_model_credentials_are_an_append_only_creation_snapshot()
-> Result<(), Box<dyn Error>> {
    const ANTHROPIC_FAMILY: &str = "anthropic";
    const CODEX_FAMILY: &str = "codex";
    const FIRST_ANTHROPIC: &str = "anthropic-first";
    const FIRST_CODEX: &str = "codex-first";
    const SECOND_CODEX: &str = "codex-second";

    let (container, pool, _database_url) = migrated_postgres().await?;
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(0xce01));
    let session = SessionId::from_uuid(Uuid::from_u128(0xce02));
    let replay_candidate = SessionId::from_uuid(Uuid::from_u128(0xce03));
    let request = CreateSessionRequest::try_new(
        command_id,
        SessionConfigurationDefaults::new(direct(0xce04)),
    )?;
    let first_pin = SessionCredentialPin::try_new(vec![
        SessionModelCredential::new(ANTHROPIC_FAMILY, FIRST_ANTHROPIC),
        SessionModelCredential::new(CODEX_FAMILY, FIRST_CODEX),
    ])
    .expect("fixture credential snapshot is valid");
    let mut first = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone(), first_pin),
    );

    let CreateSessionOutcome::Applied(first_result) = first.execute(request.clone()).await? else {
        panic!("first handling applies the fixture creation");
    };
    assert_eq!(first_result.session(), session);
    let first_snapshot: Vec<(String, String)> = sqlx::query_as(
        "SELECT model_family, credential_reference
           FROM session_model_credential_entry
          WHERE session_id = $1 AND event_ordinal = 1
          ORDER BY model_family",
    )
    .bind(session.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        first_snapshot,
        vec![
            (ANTHROPIC_FAMILY.to_owned(), FIRST_ANTHROPIC.to_owned()),
            (CODEX_FAMILY.to_owned(), FIRST_CODEX.to_owned()),
        ]
    );

    let changed_pin = SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        CODEX_FAMILY,
        SECOND_CODEX,
    )])
    .expect("changed fixture credential snapshot is valid");
    let mut replay = CreateSessionService::new(
        FixedSessionIds::new([replay_candidate]),
        CreateSessionRepository::new(pool.clone(), changed_pin),
    );
    let CreateSessionOutcome::Applied(replay_result) = replay.execute(request).await? else {
        panic!("equal replay returns the applied fixture creation");
    };
    assert_eq!(replay_result.session(), session);
    let replay_counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM session_model_credential_record WHERE session_id = $1),
            (SELECT count(*) FROM session_model_credential_entry WHERE session_id = $1),
            (SELECT current_event_ordinal::bigint
               FROM session_current_model_credentials WHERE session_id = $1)",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(replay_counts, (1, 2, 1));
    let late_entry_error = sqlx::query(
        "INSERT INTO session_model_credential_entry
            (session_id, event_ordinal, model_family, credential_reference)
         VALUES ($1, 1, 'late-family', 'late-reference')",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a published credential snapshot rejects late entries");
    let late_entry_database_error = late_entry_error
        .as_database_error()
        .expect("the snapshot guard returns a database error");
    assert_eq!(late_entry_database_error.code(), Some("P0001".into()));
    assert_eq!(
        late_entry_database_error.message(),
        "published session model credential snapshots are immutable"
    );

    sqlx::query(
        "INSERT INTO session_model_credential_record
            (session_id, event_ordinal, event_kind, provenance_kind,
             provenance_command_id, recorded_at)
         VALUES ($1, 2, 'updated', 'credential_update', $2, transaction_timestamp())",
    )
    .bind(session.into_uuid())
    .bind(command_id.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_model_credential_entry
            (session_id, event_ordinal, model_family, credential_reference)
         VALUES ($1, 2, $2, $3)",
    )
    .bind(session.into_uuid())
    .bind(CODEX_FAMILY)
    .bind(SECOND_CODEX)
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_model_credentials
            SET current_event_ordinal = 2
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;

    assert_eq!(
        current_session_credential(&pool, session, CODEX_FAMILY)
            .await?
            .as_str(),
        SECOND_CODEX
    );
    let rewrite_error = sqlx::query(
        "UPDATE session_model_credential_entry
            SET credential_reference = 'rewrite'
          WHERE session_id = $1 AND event_ordinal = 1 AND model_family = $2",
    )
    .bind(session.into_uuid())
    .bind(CODEX_FAMILY)
    .execute(&pool)
    .await
    .expect_err("historical credential entries reject rewrites");
    let rewrite_database_error = rewrite_error
        .as_database_error()
        .expect("the history guard returns a database error");
    assert_eq!(rewrite_database_error.code(), Some("P0001".into()));
    assert_eq!(
        rewrite_database_error.message(),
        "session model credential history is append-only"
    );
    let delete_head_error = sqlx::query(
        "DELETE FROM session_current_model_credentials
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("the current credential projection rejects deletion");
    let delete_head_database_error = delete_head_error
        .as_database_error()
        .expect("the current projection guard returns a database error");
    assert_eq!(delete_head_database_error.code(), Some("P0001".into()));
    assert_eq!(
        delete_head_database_error.message(),
        "session model credential head is not deletable"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A call keeps the credential profile selected from its creation-time event
/// after a later credential event advances the session head.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_call_usage_keeps_credential_pin_after_update_event() -> Result<(), Box<dyn Error>> {
    const FAMILY: &str = "cost-proof-family";
    const SUBSCRIPTION_PROFILE: &str = "cost-proof-subscription";
    const API_PROFILE: &str = "cost-proof-api";

    let (container, pool, _database_url) = migrated_postgres().await?;
    let command = DurableCommandId::from_uuid(Uuid::from_u128(0xcf01));
    let session = SessionId::from_uuid(Uuid::from_u128(0xcf02));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0xcf03));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xcf04));
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xcf05));
    let call = ModelCallId::from_uuid(Uuid::from_u128(0xcf06));
    let target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(0xcf07)));
    let pin = SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        FAMILY,
        SUBSCRIPTION_PROFILE,
    )])
    .expect("fixture credential snapshot is valid");
    CreateSessionRepository::new(pool.clone(), pin)
        .handle(prepared(
            command.into_uuid().as_u128(),
            session.into_uuid().as_u128(),
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0xcf08,
                session.into_uuid().as_u128(),
                "credential pin cost proof",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xcf09)),
            Some(turn),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(0xcf0a),
            starting_frontier: Uuid::from_u128(0xcf0b),
            initial_attempt: attempt.into_uuid(),
        },
    )
    .await?;
    let targets =
        ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(selection, target)])
            .expect("one credential proof target forms a catalog");
    let credential_families =
        ModelCredentialFamilyCatalog::try_new([(target, Arc::<str>::from(FAMILY), None)])
            .expect("one target-to-family route forms a catalog");
    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        ModelCallCredentialReference::new("unused-fallback"),
    )
    .with_session_credentials(credential_families);
    let prepared_call = repository
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcf0c)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xcf0d)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xcf0e)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcf0f)),
                    TurnId::from_uuid(Uuid::from_u128(0xcf10)),
                )
            },
        )
        .await?;
    assert_eq!(
        prepared_call,
        PrepareInitialModelCallOutcome::Checkpointed(call)
    );
    repository
        .fail_prepared_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcf11)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xcf12)),
            ),
            |_| TurnId::from_uuid(Uuid::from_u128(0xcf13)),
        )
        .await?;

    sqlx::query(
        "INSERT INTO session_model_credential_record
            (session_id, event_ordinal, event_kind, provenance_kind,
             provenance_command_id, recorded_at)
         VALUES ($1, 2, 'updated', 'credential_update', $2, transaction_timestamp())",
    )
    .bind(session.into_uuid())
    .bind(command.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_model_credential_entry
            (session_id, event_ordinal, model_family, credential_reference)
         VALUES ($1, 2, $2, $3)",
    )
    .bind(session.into_uuid())
    .bind(FAMILY)
    .bind(API_PROFILE)
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_model_credentials
            SET current_event_ordinal = 2
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;

    assert_eq!(
        current_session_credential(&pool, session, FAMILY)
            .await?
            .as_str(),
        API_PROFILE
    );
    let snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(session)
        .await?
        .expect("the terminal call has a transcript projection");
    assert_eq!(snapshot.model_call_usage().len(), 1);
    let usage = &snapshot.model_call_usage()[0];
    assert_eq!(usage.call(), call);
    assert_eq!(usage.target(), target);
    assert_eq!(usage.credential_profile(), SUBSCRIPTION_PROFILE);
    assert_eq!(
        usage.provenance(),
        ProcessModelCallUsageProvenance::Reported
    );
    assert_eq!(
        usage.input_token_semantics(),
        Some(ProcessModelCallInputTokenSemantics::CacheExclusive)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Snapshot publication serializes with entry insertion, so a concurrent late
/// family cannot pass an earlier MVCC visibility check and mutate the new head.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_model_credential_publication_rejects_a_concurrent_late_entry()
-> Result<(), Box<dyn Error>> {
    const FIRST_FAMILY: &str = "first-family";
    const FIRST_REFERENCE: &str = "first-reference";
    const CURRENT_FAMILY: &str = "current-family";
    const CURRENT_REFERENCE: &str = "current-reference";
    const LATE_FAMILY: &str = "late-family";
    const LATE_REFERENCE: &str = "late-reference";

    let (container, pool, _database_url) = migrated_postgres().await?;
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(0xce11));
    let session = SessionId::from_uuid(Uuid::from_u128(0xce12));
    let request = CreateSessionRequest::try_new(
        command_id,
        SessionConfigurationDefaults::new(direct(0xce13)),
    )?;
    let pin = SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        FIRST_FAMILY,
        FIRST_REFERENCE,
    )])
    .expect("fixture credential snapshot is valid");
    let mut service = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone(), pin),
    );
    let CreateSessionOutcome::Applied(created) = service.execute(request).await? else {
        panic!("fixture session creation applies");
    };
    assert_eq!(created.session(), session);

    sqlx::query(
        "INSERT INTO session_model_credential_record
            (session_id, event_ordinal, event_kind, provenance_kind,
             provenance_command_id, recorded_at)
         VALUES ($1, 2, 'updated', 'credential_update', $2, transaction_timestamp())",
    )
    .bind(session.into_uuid())
    .bind(command_id.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_model_credential_entry
            (session_id, event_ordinal, model_family, credential_reference)
         VALUES ($1, 2, $2, $3)",
    )
    .bind(session.into_uuid())
    .bind(CURRENT_FAMILY)
    .bind(CURRENT_REFERENCE)
    .execute(&pool)
    .await?;

    let mut publication = pool.begin().await?;
    sqlx::query(
        "UPDATE session_current_model_credentials
            SET current_event_ordinal = 2
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&mut *publication)
    .await?;
    let late_pool = pool.clone();
    let late_insert = tokio::spawn(async move {
        sqlx::query(
            "INSERT INTO session_model_credential_entry
                (session_id, event_ordinal, model_family, credential_reference)
             VALUES ($1, 2, $2, $3)",
        )
        .bind(session.into_uuid())
        .bind(LATE_FAMILY)
        .bind(LATE_REFERENCE)
        .execute(&late_pool)
        .await
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the late entry must block on the publication's session-row lock"
    );
    publication.commit().await?;
    late_insert
        .await?
        .expect_err("publication makes the concurrent late entry invalid");
    let current_snapshot: Vec<(String, String)> = sqlx::query_as(
        "SELECT model_family, credential_reference
           FROM session_model_credential_entry
          WHERE session_id = $1 AND event_ordinal = 2
          ORDER BY model_family",
    )
    .bind(session.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        current_snapshot,
        vec![(CURRENT_FAMILY.to_owned(), CURRENT_REFERENCE.to_owned())]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

async fn complete_text_turn(
    pool: &PgPool,
    session: SessionId,
    targets: ModelTargetCatalog,
    credential_reference: ModelCallCredentialReference,
    seed: u128,
    response: &str,
) -> Result<Box<[ModelConversationMessage]>, Box<dyn Error>> {
    let repository = PostgresModelCallRepository::new(pool.clone(), targets, credential_reference);
    let call = ModelCallId::from_uuid(Uuid::from_u128(seed + 1));
    let mut service = ModelCallExecutionService::new(
        FixedModelCallExecutionIds::new(
            [
                call,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 16)),
                ModelCallId::from_uuid(Uuid::from_u128(seed + 17)),
            ],
            [
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 2)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 3)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 4)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 5)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 6)),
            ],
            [
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 7)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 8)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 9)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 10)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 11)),
            ],
            [
                TurnId::from_uuid(Uuid::from_u128(seed + 12)),
                TurnId::from_uuid(Uuid::from_u128(seed + 13)),
            ],
            [ToolRequestId::from_uuid(Uuid::from_u128(seed + 14))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(seed + 15))],
        ),
        repository.clone(),
        repository.clone(),
        repository.clone(),
        repository,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    AssistantText::try_new(response.to_owned())
                        .expect("fixture assistant text is valid"),
                ],
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
        return Err("scripted model completion did not commit".into());
    };
    if !matches!(*outcome, ModelCallTerminalOutcome::Completed(_)) {
        return Err("scripted model completion did not complete the turn".into());
    }
    let (_, _, _, _, _, provider, _, _, _) = service.into_parts();
    Ok(provider
        .last_prepared_messages()
        .expect("scripted provider observed prepared messages")
        .to_vec()
        .into_boxed_slice())
}

struct TwoValueTail<'a, Value> {
    penultimate: &'a Value,
    last: &'a Value,
}

#[track_caller]
fn last_two<Value>(values: &[Value]) -> TwoValueTail<'_, Value> {
    let [.., penultimate, last] = values else {
        panic!("fixture must carry a two-value tail");
    };
    TwoValueTail { penultimate, last }
}

#[track_caller]
fn application_user_message(message: &ModelConversationMessage) -> (AcceptedInputId, &str) {
    match message {
        ModelConversationMessage::User {
            accepted_input,
            content,
            ..
        } => (*accepted_input, content.text().as_str()),
        _ => panic!("fixture message must be an application user-role message"),
    }
}

#[track_caller]
fn application_model_identity(message: &ModelConversationMessage) -> (u64, DirectModelSelection) {
    match message {
        ModelConversationMessage::ModelIdentityChanged {
            defaults_version,
            selected,
            ..
        } => (defaults_version.as_u64(), *selected),
        _ => panic!("fixture message must be an application model-identity boundary"),
    }
}

#[track_caller]
fn submit_input_database_error(error: SubmitInputRepositoryError) -> sqlx::Error {
    match error {
        SubmitInputRepositoryError::Database(error) => error,
        error => panic!("fixture expected a submit-input database error, got {error:?}"),
    }
}

#[track_caller]
fn process_user_entry(entry: &ProcessTranscriptEntry) -> (AcceptedInputId, TurnId, &str) {
    match entry {
        ProcessTranscriptEntry::User {
            accepted_input,
            turn,
            content,
            ..
        } => (*accepted_input, *turn, content.as_str()),
        _ => panic!("fixture entry must be a process user entry"),
    }
}

#[track_caller]
fn process_model_identity(entry: &ProcessTranscriptEntry) -> (TurnId, u64, DirectModelSelection) {
    match entry {
        ProcessTranscriptEntry::ModelIdentityChanged {
            turn,
            defaults_version,
            selected,
            ..
        } => (*turn, *defaults_version, *selected),
        _ => panic!("fixture entry must be a process model-identity boundary"),
    }
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct ModelCallPinFacts {
    direct_model_selection_id: Uuid,
    resolved_provider_model_identity_id: Uuid,
    credential_reference: String,
}

#[track_caller]
fn assert_ambiguous_tool_recovery(outcome: StartupScanSessionOutcome) {
    match outcome {
        StartupScanSessionOutcome::RecoveredToolAttempt(outcome) => {
            assert!(matches!(*outcome, ToolAttemptCrashOutcome::Ambiguous(_)));
        }
        _ => panic!("fixture startup recovery must classify an ambiguous tool attempt"),
    }
}

#[track_caller]
fn process_tool_reconciliation_operation(
    state: &ProcessTurnState,
) -> (TurnAttemptId, ToolAttemptId) {
    match state {
        ProcessTurnState::ReconciliationRequired {
            terminal_attempt,
            operation: ProcessReconciliationOperation::ToolAttempt(attempt),
            ..
        } => (*terminal_attempt, *attempt),
        _ => panic!("fixture turn must require tool-attempt reconciliation"),
    }
}

#[track_caller]
fn assistant_tool_request(entries: &[ProcessTranscriptEntry]) -> ToolRequestId {
    entries
        .iter()
        .find_map(|entry| match entry {
            ProcessTranscriptEntry::AssistantToolUse { request, .. } => Some(*request),
            _ => None,
        })
        .expect("fixture transcript must carry assistant tool use")
}

#[track_caller]
fn closed_tool_request(entries: &[ProcessTranscriptEntry]) -> ToolRequestId {
    entries
        .iter()
        .find_map(|entry| match entry {
            ProcessTranscriptEntry::ToolClosed { request, .. } => Some(*request),
            _ => None,
        })
        .expect("fixture transcript must carry tool closure")
}

async fn dispatched_tool_reconciliation(
    pool: &PgPool,
    expected_turn: TurnId,
    expected_attempt: ToolAttemptId,
) -> Result<bool, OutboxDispatchError> {
    let mut dispatched = false;
    drain_outbox(pool, |event| {
        if matches!(
            event.kind(),
            DispatchedOutboxEventKind::TurnReconciliationRequired {
                turn,
                operation: DispatchedReconciliationOperation::ToolAttempt(attempt),
                ..
            } if *turn == expected_turn && *attempt == expected_attempt
        ) {
            dispatched = true;
        }
    })
    .await?;
    Ok(dispatched)
}

#[track_caller]
fn activated_turn(outcome: StartEligibleTurnOutcome) -> TurnId {
    match outcome {
        StartEligibleTurnOutcome::Activated(activated) => activated.turn(),
        StartEligibleTurnOutcome::NoEligibleTurn => {
            panic!("fixture successor must be eligible for activation")
        }
    }
}

fn decide_tool_request(
    command_id: DurableCommandId,
    request: signalbox_domain::ToolRequestId,
    decision: ToolApprovalDecision,
) -> DecideToolRequest {
    DecideToolRequest::try_new(command_id, request, decision)
        .expect("fixture command identities are admitted")
}

static TEST_SUBMIT_ID: AtomicU64 = AtomicU64::new(1);

fn next_test_submit_uuid() -> Uuid {
    let suffix = TEST_SUBMIT_ID.fetch_add(1, Ordering::Relaxed) as u128;
    Uuid::from_u128((0xfeed_cafe_dead_beefu128 << 64) | suffix)
}

trait TestSubmitInputHandle {
    async fn handle(
        &self,
        command: SubmitInput,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
    ) -> Result<SubmitInputHandlingOutcome, SubmitInputRepositoryError>;
}

impl TestSubmitInputHandle for SubmitInputRepository {
    async fn handle(
        &self,
        command: SubmitInput,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
    ) -> Result<SubmitInputHandlingOutcome, SubmitInputRepositoryError> {
        self.handle_with_candidates(
            command,
            accepted_input,
            turn,
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(next_test_submit_uuid()),
                ContextFrontierId::from_uuid(next_test_submit_uuid()),
            ),
            |_| TurnId::from_uuid(next_test_submit_uuid()),
            |requests| {
                (
                    requests
                        .iter()
                        .map(|_| SemanticTranscriptEntryId::from_uuid(next_test_submit_uuid()))
                        .collect(),
                    ContextFrontierId::from_uuid(next_test_submit_uuid()),
                )
            },
        )
        .await
    }
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

async fn postgres_before_approval_migration()
-> Result<(ContainerAsync<Postgres>, PgPool, String), Box<dyn Error>> {
    let (container, pool, database_url) = unmigrated_postgres().await?;
    let mut connection = pool.acquire().await?;
    connection
        .ensure_migrations_table("_sqlx_migrations")
        .await?;
    for migration in MIGRATOR
        .iter()
        .take_while(|migration| migration.version < 202608020015)
    {
        connection.apply("_sqlx_migrations", migration).await?;
    }
    drop(connection);
    Ok((container, pool, database_url))
}

async fn insert_pending_compact_command(
    pool: &PgPool,
    command: Uuid,
    session: Uuid,
    model_call: Uuid,
    source_frontier: Uuid,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(session)
    .bind(source_frontier)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO context_compaction_model_call
            (model_call_id, session_id, direct_model_selection_id,
             resolved_provider_model_identity_id, source_frontier_id,
             credential_reference, state_kind)
         VALUES ($1, $2, $3, $4, $5, 'fixture-compaction-profile', 'prepared')",
    )
    .bind(model_call)
    .bind(session)
    .bind(Uuid::from_u128(0xc041))
    .bind(Uuid::from_u128(0xc042))
    .bind(source_frontier)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'compact_session', 1, transaction_timestamp())",
    )
    .bind(command)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO compact_session_command
            (command_id, command_kind, storage_version, session_id,
             requested_through_position, automatic_for_turn_id,
             result_kind, model_call_id)
         VALUES ($1, 'compact_session', 1, $2, NULL, NULL, 'pending', $3)",
    )
    .bind(command)
    .bind(session)
    .bind(model_call)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
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

async fn insert_completed_context_compaction_call(
    connection: &mut PgConnection,
    call: Uuid,
    session: Uuid,
    selection: Uuid,
    target: Uuid,
    source_frontier: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO context_compaction_model_call
            (model_call_id, session_id, direct_model_selection_id,
             resolved_provider_model_identity_id, source_frontier_id,
             credential_reference, state_kind)
         VALUES ($1, $2, $3, $4, $5, 'synthetic-compaction-credential',
                 'prepared')",
    )
    .bind(call)
    .bind(session)
    .bind(selection)
    .bind(target)
    .bind(source_frontier)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE context_compaction_model_call
         SET state_kind = 'in_flight'
         WHERE model_call_id = $1",
    )
    .bind(call)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE context_compaction_model_call
         SET state_kind = 'terminal', terminal_disposition_kind = 'completed',
             input_tokens = 17, output_tokens = 5
         WHERE model_call_id = $1",
    )
    .bind(call)
    .execute(&mut *connection)
    .await?;
    Ok(())
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
            "INSERT INTO context_frontier_delta
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
            SessionCreationCause::UserInitiated,
            TranscriptAncestry::None,
        ),
        SessionConfigurationDefaults::new(selection),
    )
    .prepare(SessionId::from_uuid(Uuid::from_u128(session)))
    .expect("user-initiated creation without ancestry is preparable")
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

/// Derives the direct model selection installed by the outbox session fixture.
fn outbox_session_fixture_model_selection(session_seed: u128) -> DirectModelSelection {
    DirectModelSelection::from_uuid(Uuid::from_u128(session_seed ^ 0x2000))
}

async fn drain_outbox<Inspect>(
    pool: &PgPool,
    mut inspect: Inspect,
) -> Result<(), OutboxDispatchError>
where
    Inspect: FnMut(&DispatchedOutboxEvent),
{
    let dispatcher = OutboxDispatcher::new(pool.clone());
    loop {
        match dispatcher
            .dispatch_next(|event| {
                inspect(event);
                OutboxDeliveryDecision::Delivered
            })
            .await?
        {
            OutboxDispatchOutcome::Idle => return Ok(()),
            OutboxDispatchOutcome::Delivered { .. } => {}
            OutboxDispatchOutcome::Retry { .. } => {
                unreachable!("an accepting consumer cannot request retry")
            }
        }
    }
}

async fn dispatched_tool_approval_decision(
    pool: &PgPool,
    expected_request: ToolRequestId,
) -> Result<Option<(TurnId, ToolApprovalResolution)>, OutboxDispatchError> {
    let mut found = None;
    drain_outbox(pool, |event| {
        if let DispatchedOutboxEventKind::ToolApprovalDecided { turn, approval, .. } = event.kind()
            && approval.request() == expected_request
        {
            found = Some((*turn, approval.clone()));
        }
    })
    .await?;
    Ok(found)
}

async fn corrupt_ended_attempt_disposition(
    pool: &PgPool,
    attempt: TurnAttemptId,
    disposition: &'static str,
) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE turn_attempt DISABLE TRIGGER USER")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET end_disposition = $1
          WHERE turn_attempt_id = $2",
    )
    .bind(disposition)
    .bind(attempt.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE turn_attempt ENABLE TRIGGER USER")
        .execute(pool)
        .await?;
    Ok(())
}

async fn rewind_outbox_delivery_before(
    pool: &PgPool,
    sequence: Decimal,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "ALTER TABLE outbox_delivery_state
         DISABLE TRIGGER outbox_delivery_advances_prefix",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE outbox_delivery_state
            SET delivered_through = $1 - 1,
                last_delivery_xid = pg_current_xact_id()
          WHERE singleton",
    )
    .bind(sequence)
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_delivery_state
         ENABLE TRIGGER outbox_delivery_advances_prefix",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Inserts the complete pre-outbox session record family for allocator tests.
///
/// The command and model identities derive from the one session seed.
async fn insert_outbox_session_fixture(
    pool: &PgPool,
    session_seed: u128,
) -> Result<Uuid, sqlx::Error> {
    let session = Uuid::from_u128(session_seed);
    let command = Uuid::from_u128(session_seed ^ 0x1000);
    let model = outbox_session_fixture_model_selection(session_seed);
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
    .bind(model.into_uuid())
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
    .bind(model.into_uuid())
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
        PromptMemberStatement::Stated,
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

/// Clones one recorded submission into a well-formed parked-approval interrupt
/// rejection naming `named_active_turn_id`, bypassing every domain guard. The
/// row satisfies each `submit_input_command` `CHECK` and foreign key, so only
/// the deferred correlation trigger can refuse it at commit.
async fn insert_parked_approval_interrupt_rejection(
    pool: &PgPool,
    command_id: Uuid,
    source_command_id: Uuid,
    named_active_turn_id: Uuid,
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
             result_last_position, result_existing_interrupt_command_id)
         SELECT
             $1, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text, 'interrupt',
             $3, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             'rejected', 'interrupt_unavailable_while_awaiting_approval',
             result_session_id,
             NULL, NULL,
             $3, NULL,
             NULL, NULL,
             NULL, NULL,
             NULL, NULL
           FROM submit_input_command
          WHERE command_id = $2",
    )
    .bind(command_id)
    .bind(source_command_id)
    .bind(named_active_turn_id)
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

    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
        SemanticTranscriptEntryId::from_uuid(next_test_submit_uuid())
    }

    fn next_context_frontier_id(&mut self) -> ContextFrontierId {
        ContextFrontierId::from_uuid(next_test_submit_uuid())
    }
}

#[derive(Debug)]
struct FixedStartEligibleTurnIds {
    model_identity_entries: VecDeque<SemanticTranscriptEntryId>,
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
            model_identity_entries: VecDeque::new(),
            origin_entries: origin_entries.into_iter().collect(),
            starting_frontiers: starting_frontiers.into_iter().collect(),
            initial_attempts: initial_attempts.into_iter().collect(),
        }
    }

    fn with_model_identity_entries(
        mut self,
        entries: impl IntoIterator<Item = SemanticTranscriptEntryId>,
    ) -> Self {
        self.model_identity_entries = entries.into_iter().collect();
        self
    }
}

impl StartEligibleTurnIdGenerator for FixedStartEligibleTurnIds {
    fn next_model_identity_entry_id(&mut self) -> SemanticTranscriptEntryId {
        self.model_identity_entries
            .pop_front()
            .unwrap_or_else(|| SemanticTranscriptEntryId::from_uuid(next_test_submit_uuid()))
    }

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
    tool_requests: VecDeque<signalbox_domain::ToolRequestId>,
    tool_attempts: VecDeque<TurnAttemptId>,
}

impl FixedModelCallExecutionIds {
    fn new(
        calls: impl IntoIterator<Item = ModelCallId>,
        entries: impl IntoIterator<Item = SemanticTranscriptEntryId>,
        frontiers: impl IntoIterator<Item = ContextFrontierId>,
        turns: impl IntoIterator<Item = TurnId>,
        tool_requests: impl IntoIterator<Item = signalbox_domain::ToolRequestId>,
        tool_attempts: impl IntoIterator<Item = TurnAttemptId>,
    ) -> Self {
        Self {
            calls: calls.into_iter().collect(),
            entries: entries.into_iter().collect(),
            frontiers: frontiers.into_iter().collect(),
            turns: turns.into_iter().collect(),
            tool_requests: tool_requests.into_iter().collect(),
            tool_attempts: tool_attempts.into_iter().collect(),
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

    fn next_tool_request_id(&mut self) -> ToolRequestId {
        self.tool_requests
            .pop_front()
            .expect("tool-request identity fixture")
    }

    fn next_turn_attempt_id(&mut self) -> TurnAttemptId {
        self.tool_attempts
            .pop_front()
            .expect("tool-attempt identity fixture")
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

    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 14)),
                |_| {
                    (
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 15)),
                        TurnId::from_uuid(Uuid::from_u128(seed + 16)),
                    )
                },
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
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 17)),
                |_| {
                    (
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 18)),
                        TurnId::from_uuid(Uuid::from_u128(seed + 19)),
                    )
                },
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

async fn authorize_checkpointed_model_call_with_prepared(
    pool: &PgPool,
    seed: u128,
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        PreparedModelCallRequest,
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
    let PrepareInitialModelCallOutcome::Ready { request, .. } = repository
        .prepare_initial_call(
            fixture.session,
            ModelCallId::from_uuid(Uuid::from_u128(seed + 14)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 15)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 16)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 17)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 18)),
                    TurnId::from_uuid(Uuid::from_u128(seed + 19)),
                )
            },
        )
        .await?
    else {
        panic!("the existing Prepared fixture reloads")
    };
    let AuthorizeModelCallOutcome::Authorized(authorized) = repository
        .authorize_send(fixture.session, fixture.call)
        .await?
    else {
        panic!("the exact Prepared fixture authorizes")
    };
    Ok((fixture, repository, *request, *authorized))
}

async fn checkpoint_confirmed_tool_round(
    pool: &PgPool,
    seed: u128,
    tool_name: &str,
    arguments: &str,
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        CorrelatedModelCallTerminalObservation,
        signalbox_domain::ToolRequestId,
    ),
    Box<dyn Error>,
> {
    let (fixture, repository, observation, requests) =
        checkpoint_confirmed_tool_batch(pool, seed, &[(tool_name, arguments)]).await?;
    let [request] = requests.as_slice() else {
        panic!("the single-proposal fixture returns one request")
    };
    Ok((fixture, repository, observation, *request))
}

async fn checkpoint_confirmed_tool_batch(
    pool: &PgPool,
    seed: u128,
    proposals: &[(&str, &str)],
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        CorrelatedModelCallTerminalObservation,
        Vec<signalbox_domain::ToolRequestId>,
    ),
    Box<dyn Error>,
> {
    checkpoint_tool_batch_with_approval(pool, seed, proposals, InitialToolApproval::Confirm).await
}

async fn checkpoint_tool_batch_with_approval(
    pool: &PgPool,
    seed: u128,
    proposals: &[(&str, &str)],
    initial_approval: InitialToolApproval,
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        CorrelatedModelCallTerminalObservation,
        Vec<signalbox_domain::ToolRequestId>,
    ),
    Box<dyn Error>,
> {
    let (fixture, model_repository, authorized) =
        authorize_checkpointed_model_call(pool, seed).await?;
    let requests = proposals
        .iter()
        .enumerate()
        .map(|(index, _)| {
            signalbox_domain::ToolRequestId::from_uuid(Uuid::from_u128(
                seed + 0x40 + u128::try_from(index).expect("the bounded batch index fits u128"),
            ))
        })
        .collect::<Vec<_>>();
    let response = ToolUsingAssistantResponse::try_from_parts(
        proposals
            .iter()
            .map(|(tool_name, arguments)| {
                AssistantResponsePart::ToolCall(ToolCallProposal::new(
                    ToolName::try_new(String::from(*tool_name)).expect("valid fixture tool name"),
                    NormalizedToolArguments::try_from_provider_text(String::from(*arguments))
                        .expect("bounded fixture arguments"),
                ))
            })
            .collect(),
    )
    .expect("the proposals form a tool-using response");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools { response });
    let identities = requests
        .iter()
        .enumerate()
        .map(|(index, request)| {
            ToolResponsePartIdentity::tool_call(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 0x80 + u128::try_from(index).expect("the bounded batch index fits u128"),
                )),
                *request,
                initial_approval,
            )
        })
        .collect();
    let outcome = model_repository
        .apply_terminal_observation(
            fixture.session,
            observation.clone(),
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                identities,
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xc0)),
                (!initial_approval.requires_decision())
                    .then(|| TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xc1))),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    if initial_approval.requires_decision() {
        assert!(matches!(
            outcome,
            ModelCallTerminalOutcome::ToolRound(ref round)
                if matches!(
                    round.next_phase(),
                    ActiveTurnPhase::AwaitingApproval { request: waiting }
                        if Some(waiting) == requests.first()
                )
        ));
    }
    Ok((fixture, model_repository, observation, requests))
}
async fn insert_completed_judge(
    connection: &mut PgConnection,
    fixture: &RestartModelCallFixture,
    request: ToolRequestId,
    seed: u128,
    recommendation: &str,
    input_tokens: Option<Decimal>,
) -> Result<(Uuid, Uuid), sqlx::Error> {
    let selection = Uuid::from_u128(seed + 1);
    let call = Uuid::from_u128(seed + 2);
    sqlx::query(
        "INSERT INTO tool_approval_judge_model_call
            (model_call_id, request_id, session_id, turn_id,
             direct_model_selection_id, resolved_provider_model_identity_id,
             credential_reference, state_kind)
         VALUES ($1, $2, $3, $4, $5, $6, 'fixture-credential', 'prepared')",
    )
    .bind(call)
    .bind(request.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(selection)
    .bind(Uuid::from_u128(seed + 3))
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE tool_approval_judge_model_call SET state_kind = 'in_flight'
          WHERE model_call_id = $1",
    )
    .bind(call)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE tool_approval_judge_model_call
            SET state_kind = 'terminal', terminal_disposition_kind = 'completed',
                recommendation_kind = $1, rationale = 'fixture rationale',
                input_tokens = $2
          WHERE model_call_id = $3",
    )
    .bind(recommendation)
    .bind(input_tokens)
    .bind(call)
    .execute(&mut *connection)
    .await?;
    Ok((selection, call))
}

fn database_constraint(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(|error| error.constraint())
}

/// S10 / INV-005: stored tool arguments use the same exact canonical JSON or
/// undecodable representation admitted by the domain.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_inv005_tool_argument_representation_is_database_checked() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7380;
    let canonical = r#"{"exponent":1e+400,"wide":18446744073709551617}"#;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", canonical).await?;
    let stored: (String, String) = sqlx::query_as(
        "SELECT arguments_kind, arguments_text
           FROM tool_request
          WHERE request_id = $1",
    )
    .bind(request.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored, (String::from("json"), String::from(canonical)));

    let depth = 512;
    let deep = format!("{}null{}", "[".repeat(depth), "]".repeat(depth));
    let (_, _, _, deep_request) =
        checkpoint_confirmed_tool_round(&pool, seed + 0x1000, "current_time", &deep).await?;
    let stored_deep: (String, String) = sqlx::query_as(
        "SELECT arguments_kind, arguments_text
           FROM tool_request
          WHERE request_id = $1",
    )
    .bind(deep_request.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored_deep, (String::from("json"), deep));

    let escaped_nul = r#"{"x":"\u0000"}"#;
    let (_, _, _, escaped_nul_request) =
        checkpoint_confirmed_tool_round(&pool, seed + 0x2000, "current_time", escaped_nul).await?;
    let stored_escaped_nul: (String, String) = sqlx::query_as(
        "SELECT arguments_kind, arguments_text
           FROM tool_request
          WHERE request_id = $1",
    )
    .bind(escaped_nul_request.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored_escaped_nul,
        (String::from("json"), String::from(escaped_nul))
    );

    for (offset, kind, text) in [
        (0_u128, "json", "{broken"),
        (1, "json", r#"{"b":2,"a":1}"#),
        (2, "undecodable", "{}"),
        (3, "undecodable", r#"{ "a": 1 }"#),
    ] {
        let error = sqlx::query(
            "INSERT INTO tool_request
                (request_id, session_id, turn_id, producing_model_call_id,
                 request_ordinal, tool_name, arguments_kind, arguments_text)
             VALUES ($1, $2, $3, $4, $5, 'invalid_fixture', $6, $7)",
        )
        .bind(Uuid::from_u128(seed + 100 + offset))
        .bind(fixture.session.into_uuid())
        .bind(fixture.turn.into_uuid())
        .bind(fixture.call.into_uuid())
        .bind(Decimal::from(1_u64 + u64::try_from(offset)?))
        .bind(kind)
        .bind(text)
        .execute(&pool)
        .await
        .expect_err("kind and stored argument representation must agree");
        assert_eq!(
            error
                .as_database_error()
                .and_then(|error| error.constraint()),
            Some("tool_request_arguments_representation")
        );
    }

    pool.close().await;
    drop(container);
    Ok(())
}

/// S10: user-decision receipts for one batch reconstitute from one identity-set
/// load instead of one query per approval row.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_user_decision_receipts_batch_reconstitute() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x73a0;
    let (fixture, _, _, requests) = checkpoint_confirmed_tool_batch(
        &pool,
        seed,
        &[
            ("first-dangerous-tool", "{}"),
            ("second-dangerous-tool", "{}"),
        ],
    )
    .await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let [first_request, second_request] = requests.as_slice() else {
        panic!("the fixture proposes exactly two dangerous tools");
    };
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0)),
                *first_request,
                ToolApprovalDecision::Deny { reason: None },
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe0)),
        )
        .await?;
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd1)),
                *second_request,
                ToolApprovalDecision::Deny { reason: None },
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe1)),
        )
        .await?;

    let reconstituted = repository
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("the fully denied batch remains available for result projection");
    let first_approval = reconstituted
        .approval(*first_request)
        .expect("the first user decision reconstitutes");
    assert!(matches!(
        first_approval.decision(),
        ToolApprovalDecision::Deny { .. }
    ));
    assert_eq!(
        first_approval.source(),
        signalbox_domain::ToolDecisionSource::UserCommand
    );
    let second_approval = reconstituted
        .approval(*second_request)
        .expect("the second user decision reconstitutes");
    assert!(matches!(
        second_approval.decision(),
        ToolApprovalDecision::Deny { .. }
    ));
    assert_eq!(
        second_approval.source(),
        signalbox_domain::ToolDecisionSource::UserCommand
    );
    assert!(
        ProcessReadRepository::new(pool.clone())
            .session_has_tool_history(fixture.session)
            .await?
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S10 / INV-012: a replayed not-earliest receipt can name only an earlier
/// request from the same producing round.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_inv012_decision_receipt_rejects_cross_round_earliest_request()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x73c0;
    let (_, _, _, requested) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;
    let (_, _, _, foreign_earlier) =
        checkpoint_confirmed_tool_round(&pool, seed + 0x100, "current_time", "{}").await?;
    let mut transaction = pool.begin().await?;
    let command = Uuid::from_u128(seed + 0x200);
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'decide_tool_request', 1, transaction_timestamp())",
    )
    .bind(command)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO decide_tool_request_command
            (command_id, command_kind, storage_version, request_id,
             decision_kind, denial_reason, result_kind, rejection_kind,
             result_earliest_undecided_request_id)
         VALUES ($1, 'decide_tool_request', 1, $2,
                 'approve', NULL, 'rejected',
                 'not_earliest_undecided', $3)",
    )
    .bind(command)
    .bind(requested.into_uuid())
    .bind(foreign_earlier.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error =
        sqlx::query("SET CONSTRAINTS decide_tool_request_command_requires_effect IMMEDIATE")
            .execute(&mut *transaction)
            .await
            .expect_err("a recorded blocker from another round is corruption");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("decide_tool_request_command_earliest_correlation")
    );
    transaction.rollback().await?;

    pool.close().await;
    drop(container);
    Ok(())
}

/// S31 / INV-043: durable runner lease binding keeps restart reconstitution from
/// issuing a second runner capability for the same physical attempt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s31_inv043_active_batch_reload_restores_consumed_runner_issuance()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x73f0;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let turn_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe0));
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || turn_attempt,
        )
        .await?;
    let attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe1));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            attempt,
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the approved request prepares its physical attempt");
    repository
        .authorize_attempt(fixture.session, fixture.turn, attempt)
        .await?;
    sqlx::query("ALTER TABLE runner_lease_generation DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             predecessor_generation)
         VALUES ($1, 1, $2, $3, $4, $5, 'pure', 1, $6, 1, NULL)",
    )
    .bind(Uuid::from_u128(seed + 0xe2))
    .bind(attempt.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 0xe3))
    .bind("current_time")
    .bind(Uuid::from_u128(seed + 0xe4))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_lease_generation ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let reloaded = repository
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("the active batch reloads with its durable lease binding");
    let duplicate = reloaded
        .resume_runner_attempt(attempt)
        .expect_err("durably issued runner authority cannot be minted again after restart");

    assert_eq!(
        duplicate.failure(),
        ToolBatchExecutionFailure::AttemptStageMismatch
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// S31 / INV-004 / INV-043: a stored retryable claimed loss leaves its source
/// attempt in flight, so a restarted process reloads an active batch that
/// still carries the exact live source the checked claimed replacement
/// requires, and its retired inventory stays empty until the atomic
/// replacement commit retires the predecessor.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s31_inv004_inv043_batch_reload_preserves_lost_claimed_source_attempt()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7460;
    let (fixture, _, _, requests) = checkpoint_confirmed_tool_batch(
        &pool,
        seed,
        &[("current_time", "{}"), ("current_time", "{}")],
    )
    .await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let turn_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe0));
    let [first_request, second_request] = requests.as_slice() else {
        panic!("the two-proposal fixture returns two requests")
    };
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0)),
                *first_request,
                ToolApprovalDecision::Approve,
            ),
            || turn_attempt,
        )
        .await?;
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd1)),
                *second_request,
                ToolApprovalDecision::Approve,
            ),
            || turn_attempt,
        )
        .await?;
    let source = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe1));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            source,
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the first approved request prepares its physical attempt");
    repository
        .authorize_attempt(fixture.session, fixture.turn, source)
        .await?;
    // The exact durable shape a stored retryable claimed loss leaves before
    // any replacement is reserved: a lost-claimed lease head over the still
    // in-flight source attempt.
    sqlx::query("ALTER TABLE runner_lease_generation DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             predecessor_generation)
         VALUES ($1, 1, $2, $3, $4, $5, 'pure', 1, $6, 1, NULL)",
    )
    .bind(Uuid::from_u128(seed + 0xe2))
    .bind(source.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 0xe3))
    .bind("current_time")
    .bind(Uuid::from_u128(seed + 0xe4))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_lease_generation ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE runner_lease_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, 1, 1, 'offered'), ($1, 1, 2, 'claimed'),
                ($1, 1, 3, 'lost_claimed')",
    )
    .bind(Uuid::from_u128(seed + 0xe2))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_lease_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE runner_current_lease_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_current_lease_event
            (lease_id, generation, event_ordinal)
         VALUES ($1, 1, 3)",
    )
    .bind(Uuid::from_u128(seed + 0xe2))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_current_lease_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let current_source: (Uuid, String) = sqlx::query_as(
        "SELECT attempt_id, state_kind
           FROM runner_current_tool_attempt
          WHERE request_id = $1",
    )
    .bind(first_request.into_uuid())
    .fetch_one(&pool)
    .await?;
    let reloaded = repository
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("the active batch reloads with its live lost-claimed source");
    let live_source = reloaded
        .prepare_next_attempt(
            ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe5)),
            ToolEffectClass::EffectFree,
        )
        .expect_err("the lost-claimed source survives reload as the live attempt");

    assert_eq!(current_source, (source.into_uuid(), "in_flight".to_owned()));
    assert_eq!(reloaded.retired_attempts().count(), 0);
    assert_eq!(
        live_source.failure(),
        ToolBatchExecutionFailure::LiveAttemptPresent
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// S31 / INV-004 / INV-043: active-batch reload restores the durable
/// retired-identity inventory a claimed runner retry leaves behind, so a
/// restarted process rejects reuse of the retired physical-attempt identity in
/// the domain instead of failing on the retained database row's key.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s31_inv004_inv043_batch_reload_restores_retired_attempt_identities()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7440;
    let (fixture, _, _, requests) = checkpoint_confirmed_tool_batch(
        &pool,
        seed,
        &[("current_time", "{}"), ("current_time", "{}")],
    )
    .await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let turn_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe0));
    let [first_request, second_request] = requests.as_slice() else {
        panic!("the two-proposal fixture returns two requests")
    };
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0)),
                *first_request,
                ToolApprovalDecision::Approve,
            ),
            || turn_attempt,
        )
        .await?;
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd1)),
                *second_request,
                ToolApprovalDecision::Approve,
            ),
            || turn_attempt,
        )
        .await?;
    let retired = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe1));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            retired,
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the first approved request prepares its physical attempt");
    repository
        .authorize_attempt(fixture.session, fixture.turn, retired)
        .await?;
    let issuing_attempt: Uuid = sqlx::query_scalar(
        "SELECT issuing_turn_attempt_id
           FROM tool_attempt
          WHERE attempt_id = $1",
    )
    .bind(retired.into_uuid())
    .fetch_one(&pool)
    .await?;
    // The exact durable shape a persisted claimed pure retry leaves behind:
    // a lost-claimed lease head over the retired terminal predecessor and the
    // committed replacement attempt that completed after the restart.
    sqlx::query("ALTER TABLE runner_lease_generation DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             predecessor_generation)
         VALUES ($1, 1, $2, $3, $4, $5, 'pure', 1, $6, 1, NULL)",
    )
    .bind(Uuid::from_u128(seed + 0xe2))
    .bind(retired.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 0xe3))
    .bind("current_time")
    .bind(Uuid::from_u128(seed + 0xe4))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_lease_generation ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE runner_lease_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, 1, 1, 'offered'), ($1, 1, 2, 'claimed'),
                ($1, 1, 3, 'lost_claimed')",
    )
    .bind(Uuid::from_u128(seed + 0xe2))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_lease_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE runner_current_lease_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_current_lease_event
            (lease_id, generation, event_ordinal)
         VALUES ($1, 1, 3)",
    )
    .bind(Uuid::from_u128(seed + 0xe2))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_current_lease_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                error_kind = 'crash_lost'
          WHERE attempt_id = $1",
    )
    .bind(retired.into_uuid())
    .execute(&pool)
    .await?;
    let replacement = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe5));
    sqlx::query(
        "INSERT INTO tool_attempt
            (attempt_id, request_id, session_id, turn_id,
             issuing_turn_attempt_id, effect_class, dispatch_generation,
             state_kind, terminal_disposition_kind, result_content_kind,
             result_text)
         VALUES ($1, $2, $3, $4, $5, 'effect_free', 1,
                 'terminal', 'completed', 'text', 'replacement completed')",
    )
    .bind(replacement.into_uuid())
    .bind(first_request.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(issuing_attempt)
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let reloaded = repository
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("the active batch reloads with its retired-identity inventory");
    let reuse = reloaded
        .prepare_next_attempt(retired, ToolEffectClass::EffectFree)
        .expect_err("a durably retired identity cannot be reused after restart");

    assert_eq!(
        reloaded.retired_attempts().collect::<Vec<_>>(),
        vec![retired]
    );
    assert_eq!(
        reuse.failure(),
        ToolBatchExecutionFailure::AttemptIdentityReuse
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// S02 / S10 / S11 / INV-005 / INV-006 / INV-019 / INV-027 / INV-036: one confirmed
/// proposal survives a repository restart, records a replay-safe user
/// decision, executes through an exact durable fence, and projects one
/// reference-only result atomically with the same-turn continuation call.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s10_s11_inv005_inv006_inv019_inv027_tool_round_survives_restart_and_projects_result()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7400;
    let (fixture, model_repository, observation, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;
    let mut scheduling_probe = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                seed + 30,
            ))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(seed + 31))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(seed + 32))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert_eq!(
        scheduling_probe.execute(fixture.session).await?,
        StartEligibleTurnOutcome::NoEligibleTurn,
        "the scheduler reloads the parked tool round without inventing work"
    );
    assert_eq!(
        model_repository
            .reread_terminal_observation(fixture.session, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );

    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    let restarted_repository = PostgresToolLoopRepository::new(pool.clone());
    let parked = restarted_repository
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("the active logical batch reloads after repository restart");
    assert_eq!(parked.producing_call(), fixture.call);
    assert_eq!(parked.requests()[0].id(), request);
    assert!(parked.awaiting_approval().is_some());

    let continuation_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 23));
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(seed + 24));
    let approve = decide_tool_request(command_id, request, ToolApprovalDecision::Approve);
    let decision = tool_repository
        .decide(approve.clone(), || continuation_attempt)
        .await?;
    assert!(matches!(
        decision.result(),
        DecideToolRequestResult::Applied(_)
    ));
    assert_eq!(
        tool_repository
            .decide(approve, || panic!("replay consumes no identity"))
            .await?,
        decision,
        "same command identity and payload replay the terminal receipt"
    );
    let running_tool_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(fixture.session)
        .await?
        .expect("an approved running tool round remains process-readable");
    assert_eq!(
        running_tool_snapshot.turns()[0].state(),
        &ProcessTurnState::ActiveRunning {
            current_attempt: continuation_attempt,
            current_model_call: None,
        }
    );
    assert!(matches!(
        tool_repository
            .decide(
                decide_tool_request(
                    command_id,
                    request,
                    ToolApprovalDecision::Deny { reason: None },
                ),
                || panic!("conflicting replay consumes no identity"),
            )
            .await,
        Err(ToolLoopRepositoryError::ConflictingCommandReuse)
    ));

    let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 25));
    let prepared_attempt = tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            tool_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the revalidated batch still has work");
    assert_eq!(prepared_attempt.state(), CurrentToolAttemptState::Prepared);
    assert!(matches!(
        tool_repository
            .reread_ambiguous_authorization(fixture.session, fixture.turn, tool_attempt)
            .await?,
        ToolAttemptAuthorizationStatus::Prepared(ref attempt)
            if attempt.attempt() == tool_attempt
    ));
    let authorized_attempt = tool_repository
        .authorize_attempt(fixture.session, fixture.turn, tool_attempt)
        .await?;
    assert!(matches!(
        tool_repository
            .reread_ambiguous_authorization(fixture.session, fixture.turn, tool_attempt)
            .await?,
        ToolAttemptAuthorizationStatus::InFlight(ref reread)
            if reread == &authorized_attempt
    ));
    let impossible_preflight_error = sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                error_kind = 'unknown_tool'
          WHERE attempt_id = $1",
    )
    .bind(tool_attempt.into_uuid())
    .execute(&pool)
    .await
    .expect_err("in-flight work cannot acquire preflight-only terminal evidence");
    assert_eq!(
        impossible_preflight_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    let ended = tool_repository
        .commit_observation(
            authorized_attempt
                .executor_fence()
                .bind(ToolAttemptObservation::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(String::from("2026-07-23T12:00:00Z"))
                            .expect("bounded result"),
                    ),
                }),
        )
        .await?;
    assert!(matches!(ended.end(), ToolAttemptEnd::Completed { .. }));

    let unrelated_session = Uuid::from_u128(seed + 80);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(seed + 81, seed + 80, direct(seed + 82)))
        .await?;
    for (entry, payload_kind, request_reference, attempt_reference) in [
        (
            Uuid::from_u128(seed + 83),
            "tool_closed_by_turn_end",
            Some(request.into_uuid()),
            None,
        ),
        (
            Uuid::from_u128(seed + 84),
            "tool_execution_result",
            None,
            Some(tool_attempt.into_uuid()),
        ),
    ] {
        let cross_session_result_error = sqlx::query(
            "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 tool_result_request_id, tool_result_attempt_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(unrelated_session)
        .bind(entry)
        .bind(payload_kind)
        .bind(request_reference)
        .bind(attempt_reference)
        .execute(&pool)
        .await
        .expect_err("tool-result references must belong to the entry's source session");
        assert_eq!(
            cross_session_result_error
                .as_database_error()
                .and_then(|error| error.code()),
            Some("23503".into())
        );
    }

    let resolved = restarted_repository
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("the ended attempt remains part of the active batch");
    assert!(
        resolved
            .prepare_result_projection(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 26
                ))],
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 27)),
            )
            .is_ok()
    );
    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one continuation target forms a catalog");
    let continuing_repository = PostgresToolLoopRepository::with_model_calls(
        pool.clone(),
        targets,
        model_credential_reference(),
    );
    let result_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 26));
    let continuation_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(seed + 27));
    let continuation_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 28));
    let continuation = continuing_repository
        .prepare_continuation(
            fixture.session,
            fixture.turn,
            fixture.call,
            signalbox_application::ToolContinuationIdentities::new(
                vec![result_entry],
                continuation_frontier,
                continuation_call,
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 29)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 30)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 31)),
            ),
            |_| panic!("fixture has no pending steering"),
        )
        .await?;
    assert_eq!(
        continuation,
        signalbox_application::PrepareToolContinuationOutcome::Checkpointed(continuation_call)
    );

    let durable_shape: (i64, i64, i64, i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM tool_round WHERE producing_model_call_id = $1),
            (SELECT count(*) FROM tool_request WHERE request_id = $2),
            (SELECT count(*) FROM tool_approval_decision
              WHERE request_id = $2
                AND decision_kind = 'approve'
                AND decision_source = 'owner_command'),
            (SELECT count(*) FROM tool_attempt
              WHERE attempt_id = $3
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed'),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE semantic_entry_id = $8
                AND payload_kind = 'tool_execution_result'
                AND tool_result_attempt_id = $3),
            (SELECT count(*) FROM turn_attempt
              WHERE turn_attempt_id = $4
                AND state_kind = 'running'),
            (SELECT count(*) FROM turn_lifecycle
              WHERE session_id = $5
                AND turn_id = $6
                AND state_kind = 'active'
                AND active_phase_kind = 'running'
                AND current_attempt_id = $4
                AND active_tool_round_call_id IS NULL),
            (SELECT count(*) FROM model_call
              WHERE model_call_id = $7
                AND session_id = $5
                AND turn_id = $6
                AND turn_attempt_id = $4
                AND context_frontier_id = $9
                AND state_kind = 'prepared'),
            (SELECT count(*) FROM tool_batch_transition_outbox_event
              WHERE producing_model_call_id = $1
                AND transition_kind = 'proposed'
                AND frontier_id = (
                    SELECT boundary_frontier_id
                      FROM tool_round
                     WHERE producing_model_call_id = $1
                )),
            (SELECT count(*) FROM tool_batch_transition_outbox_event
              WHERE producing_model_call_id = $1
                AND transition_kind = 'results_projected'
                AND frontier_id = $9)",
    )
    .bind(fixture.call.into_uuid())
    .bind(request.into_uuid())
    .bind(tool_attempt.into_uuid())
    .bind(continuation_attempt.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(continuation_call.into_uuid())
    .bind(result_entry.into_uuid())
    .bind(continuation_frontier.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_shape, (1, 1, 1, 1, 1, 1, 1, 1, 1, 1));

    let duplicate_result_error = sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             tool_result_request_id)
         VALUES ($1, $2, 'tool_closed_by_turn_end', $3)",
    )
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 90))
    .bind(request.into_uuid())
    .execute(&pool)
    .await
    .expect_err("one request cannot have attempt- and request-referenced results");
    assert_eq!(
        duplicate_result_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23505".into())
    );
    assert!(matches!(
        ToolLoopRepositoryError::from(duplicate_result_error),
        ToolLoopRepositoryError::Corruption(_)
    ));

    let mut missing_current_result = pool.begin().await?;
    sqlx::query(
        "ALTER TABLE context_frontier_delta
         DISABLE TRIGGER context_frontier_member_is_append_only",
    )
    .execute(&mut *missing_current_result)
    .await?;
    sqlx::query(
        "DELETE FROM context_frontier_delta
          WHERE owning_session_id = $1
            AND context_frontier_id = $2
            AND source_session_id = $1
            AND semantic_entry_id = $3",
    )
    .bind(fixture.session.into_uuid())
    .bind(continuation_frontier.into_uuid())
    .bind(result_entry.into_uuid())
    .execute(&mut *missing_current_result)
    .await?;
    let missing_result_error = sqlx::query("SELECT assert_model_call_final_state_without_stop($1)")
        .bind(continuation_call.into_uuid())
        .execute(&mut *missing_current_result)
        .await
        .expect_err("a continuation call requires every current-round result");
    assert_eq!(
        missing_result_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    missing_current_result.rollback().await?;

    assert!(
        restarted_repository
            .load_active_batch(fixture.session, fixture.turn)
            .await?
            .is_none(),
        "the atomic continuation no longer exposes the completed batch"
    );
    let mut proposed_event = false;
    let mut results_event = false;
    drain_outbox(&pool, |event| match event.kind() {
        DispatchedOutboxEventKind::ToolBatchTransition {
            turn,
            producing_call,
            state:
                DispatchedToolBatchState::Proposed {
                    frontier: proposed_frontier,
                },
        } if *turn == fixture.turn && *producing_call == fixture.call => {
            proposed_event = *proposed_frontier == parked.yielded_snapshot().frontier().snapshot();
        }
        DispatchedOutboxEventKind::ToolBatchTransition {
            turn,
            producing_call,
            state:
                DispatchedToolBatchState::ResultsProjected {
                    frontier: result_frontier,
                },
        } if *turn == fixture.turn && *producing_call == fixture.call => {
            results_event = *result_frontier == continuation_frontier;
        }
        _ => {}
    })
    .await?;
    assert!(proposed_event && results_event);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn explicit_tool_decision_dispatches_full_user_provenance() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7e00;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;
    let command = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x21));
    PostgresToolLoopRepository::new(pool.clone())
        .decide(
            decide_tool_request(command, request, ToolApprovalDecision::Approve),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x22)),
        )
        .await?;

    let (event_turn, approval) = dispatched_tool_approval_decision(&pool, request)
        .await?
        .expect("the explicit decision appends its typed outbox event");
    assert_eq!(event_turn, fixture.turn);
    assert_eq!(approval.request(), request);
    assert_eq!(approval.decision(), &ToolApprovalDecision::Approve);
    assert_eq!(
        approval.decider(),
        Some(&ToolApprovalDecider::User { command })
    );
    assert_eq!(approval.rationale(), None);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_automatic_decision_cannot_widen_a_human_request()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (_fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, 0x7e20, "current_time", "{}").await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source)
         VALUES ($1, 'approve', 'policy_auto')",
    )
    .bind(request.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("human posture rejects auto authority");
    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_automatic_requires_auto_posture")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_posture_migration_backfills_append_only_requests() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = postgres_before_approval_migration().await?;
    let request = Uuid::from_u128(0x7e31);
    let mut connection = pool.acquire().await?;
    sqlx::query("ALTER TABLE tool_request DISABLE TRIGGER ALL")
        .execute(&mut *connection)
        .await?;
    sqlx::query(
        "INSERT INTO tool_request
            (request_id, session_id, turn_id, producing_model_call_id,
             request_ordinal, tool_name, arguments_kind, arguments_text)
         VALUES ($1, $2, $3, $4, 0, 'current_time', 'json', '{}')",
    )
    .bind(request)
    .bind(Uuid::from_u128(0x7e32))
    .bind(Uuid::from_u128(0x7e33))
    .bind(Uuid::from_u128(0x7e34))
    .execute(&mut *connection)
    .await?;
    sqlx::query("ALTER TABLE tool_request ENABLE TRIGGER ALL")
        .execute(&mut *connection)
        .await?;
    drop(connection);

    migrate(&pool).await?;
    let posture: String =
        sqlx::query_scalar("SELECT approval_posture FROM tool_request WHERE request_id = $1")
            .bind(request)
            .fetch_one(&pool)
            .await?;
    assert_eq!(posture, "human");
    let error = sqlx::query("UPDATE tool_request SET tool_name = tool_name WHERE request_id = $1")
        .bind(request)
        .execute(&pool)
        .await
        .expect_err("the migration restores append-only enforcement");
    assert!(error.to_string().contains("tool_request is append-only"));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_judge_completion_respects_posture() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (human, _, _, human_request) =
        checkpoint_confirmed_tool_round(&pool, 0x7e40, "current_time", "{}").await?;
    let mut connection = pool.acquire().await?;
    let posture_error = insert_completed_judge(
        &mut connection,
        &human,
        human_request,
        0x7e50,
        "approve",
        None,
    )
    .await
    .expect_err("a judge cannot approve human-only authority");
    assert_eq!(
        database_constraint(&posture_error),
        Some("tool_approval_judge_recommendation_within_posture")
    );

    drop(connection);
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_completed_judge_requires_atomic_decision_effect()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (fixture, _, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        0x7e54,
        &[("current_time", "{}")],
        InitialToolApproval::Delegated,
    )
    .await?;
    let [request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let mut transaction = pool.begin().await?;
    insert_completed_judge(
        &mut transaction,
        &fixture,
        *request,
        0x7e55,
        "approve",
        None,
    )
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("completed approve requires its decision, event, and lifecycle effect");
    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_judge_completed_requires_decision_effect")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_delegate_decision_requires_event_and_lifecycle_effect()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (fixture, _, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        0x7e56,
        &[("current_time", "{}")],
        InitialToolApproval::Delegated,
    )
    .await?;
    let [request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let mut transaction = pool.begin().await?;
    let (selection, call) = insert_completed_judge(
        &mut transaction,
        &fixture,
        *request,
        0x7e57,
        "approve",
        None,
    )
    .await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source,
             delegate_model_selection_id, delegate_model_call_id, rationale)
         VALUES ($1, 'approve', 'delegate', $2, $3, 'fixture rationale')",
    )
    .bind(request.into_uuid())
    .bind(selection)
    .bind(call)
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("delegate approval requires its event and advanced lifecycle");
    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_explicit_requires_atomic_effect")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_user_decision_requires_event_and_lifecycle_effect()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (_fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, 0x7e57, "current_time", "{}").await?;
    let command = Uuid::from_u128(0x7e58);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'decide_tool_request', 1, transaction_timestamp())",
    )
    .bind(command)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO decide_tool_request_command
            (command_id, command_kind, storage_version, request_id,
             decision_kind, denial_reason, result_kind, rejection_kind,
             result_earliest_undecided_request_id)
         VALUES ($1, 'decide_tool_request', 1, $2,
                 'approve', NULL, 'applied', NULL, NULL)",
    )
    .bind(command)
    .bind(request.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source, owner_command_id)
         VALUES ($1, 'approve', 'owner_command', $2)",
    )
    .bind(request.into_uuid())
    .bind(command)
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("user approval requires its event and advanced lifecycle");
    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_explicit_requires_atomic_effect")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_unsent_judge_call_rejects_usage() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (fixture, _, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        0x7e58,
        &[("current_time", "{}")],
        InitialToolApproval::Delegated,
    )
    .await?;
    let [request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let call = Uuid::from_u128(0x7e59);
    let mut connection = pool.acquire().await?;
    sqlx::query(
        "INSERT INTO tool_approval_judge_model_call
            (model_call_id, request_id, session_id, turn_id,
             direct_model_selection_id, resolved_provider_model_identity_id,
             credential_reference, state_kind)
         VALUES ($1, $2, $3, $4, $5, $6, 'fixture-credential', 'prepared')",
    )
    .bind(call)
    .bind(request.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(Uuid::from_u128(0x7e5a))
    .bind(Uuid::from_u128(0x7e5b))
    .execute(&mut *connection)
    .await?;
    let error = sqlx::query(
        "UPDATE tool_approval_judge_model_call
            SET state_kind = 'terminal', terminal_disposition_kind = 'known_failed',
                input_tokens = 1
          WHERE model_call_id = $1",
    )
    .bind(call)
    .execute(&mut *connection)
    .await
    .expect_err("an unsent judge call cannot report provider usage");
    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_judge_unsent_has_no_usage")
    );

    drop(connection);
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_judge_usage_respects_u64_bounds() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (delegated, _, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        0x7e60,
        &[("current_time", "{}")],
        InitialToolApproval::Delegated,
    )
    .await?;
    let [delegated_request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let mut connection = pool.acquire().await?;
    let too_large = Decimal::from(u64::MAX) + Decimal::ONE;
    let usage_error = insert_completed_judge(
        &mut connection,
        &delegated,
        *delegated_request,
        0x7e70,
        "approve",
        Some(too_large),
    )
    .await
    .expect_err("judge usage above u64 cannot commit");
    assert_eq!(
        database_constraint(&usage_error),
        Some("tool_approval_judge_call_usage_u64_range")
    );

    drop(connection);
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_judge_usage_rejects_fractional_counts() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (delegated, _, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        0x7e74,
        &[("current_time", "{}")],
        InitialToolApproval::Delegated,
    )
    .await?;
    let [delegated_request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let mut connection = pool.acquire().await?;
    let usage_error = insert_completed_judge(
        &mut connection,
        &delegated,
        *delegated_request,
        0x7e75,
        "approve",
        Some(Decimal::new(15, 1)),
    )
    .await
    .expect_err("fractional judge usage cannot be rounded into storage");
    assert_eq!(
        database_constraint(&usage_error),
        Some("tool_approval_judge_call_usage_u64_range")
    );

    drop(connection);
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_user_cannot_decide_delegated_request_before_escalation()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (_fixture, _, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        0x7e80,
        &[("current_time", "{}")],
        InitialToolApproval::Delegated,
    )
    .await?;
    let [request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let error = PostgresToolLoopRepository::new(pool.clone())
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(0x9e81)),
                *request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(0x9e82)),
        )
        .await
        .expect_err("delegated authority requires recorded escalation");
    let ToolLoopRepositoryError::Database { source, .. } = error else {
        panic!("the authority guard returns its database constraint")
    };
    assert_eq!(
        database_constraint(&source),
        Some("tool_approval_user_requires_human_authority")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S02 / S08 / INV-016 / INV-036: a NextSafePoint input accepted while a tool
/// round executes is consumed by the same-turn continuation call, and the
/// committed continuation shape reloads through the scheduling projection —
/// the next submit is accepted and the startup scan classifies the prepared
/// call instead of leaving the session permanently unloadable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s08_inv016_inv036_steering_consumed_at_continuation_reloads_and_scans()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7f00;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;
    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    let continuation_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x22));
    tool_repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x21)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || continuation_attempt,
        )
        .await?;
    let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0x23));
    tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            tool_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?;
    let authorized_attempt = tool_repository
        .authorize_attempt(fixture.session, fixture.turn, tool_attempt)
        .await?;

    let steering_outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 0x24,
                seed + 1,
                "steer the executing tool round",
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: fixture.turn,
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x25)),
            None,
        )
        .await?;
    assert!(matches!(
        steering_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));

    tool_repository
        .commit_observation(
            authorized_attempt
                .executor_fence()
                .bind(ToolAttemptObservation::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(String::from("2026-07-26T12:00:00Z"))
                            .expect("bounded result"),
                    ),
                }),
        )
        .await?;

    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one continuation target forms a catalog");
    let continuation_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0x28));
    let steering_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x2c));
    let continuation = PostgresToolLoopRepository::with_model_calls(
        pool.clone(),
        targets,
        model_credential_reference(),
    )
    .prepare_continuation(
        fixture.session,
        fixture.turn,
        fixture.call,
        signalbox_application::ToolContinuationIdentities::new(
            vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                seed + 0x26,
            ))],
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x27)),
            continuation_call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x29)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2a)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2b)),
        ),
        |_| {
            (
                steering_entry,
                TurnId::from_uuid(Uuid::from_u128(seed + 0x2d)),
            )
        },
    )
    .await?;
    assert_eq!(
        continuation,
        signalbox_application::PrepareToolContinuationOutcome::Checkpointed(continuation_call)
    );
    let consumed_shape: (String, Option<Uuid>, i64) = sqlx::query_as(
        "SELECT accepted.disposition_kind,
                accepted.consuming_model_call_id,
                (SELECT count(*) FROM context_frontier_delta AS delta
                  WHERE delta.owning_session_id = $2
                    AND delta.semantic_entry_id = $3)
           FROM accepted_input AS accepted
          WHERE accepted.session_id = $2
            AND accepted.accepted_input_id = $1",
    )
    .bind(Uuid::from_u128(seed + 0x25))
    .bind(fixture.session.into_uuid())
    .bind(steering_entry.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        consumed_shape,
        (
            String::from("consumed_as_steering"),
            Some(continuation_call.into_uuid()),
            1,
        ),
        "the continuation call durably consumed the steering input"
    );

    let queued_follow_up = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 0x2e,
                seed + 1,
                "queued work behind the continuation",
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: fixture.turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x2f)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 0x30))),
        )
        .await?;
    assert!(
        matches!(
            queued_follow_up,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::TurnOrigin(_)
            ))
        ),
        "writer-produced consumed steering must reconstitute before the next submit"
    );

    let mut recovery_ids = FixedStartupScanIds::new([], []);
    let scan = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.session,
            signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x31)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x32)),
            ),
            &mut recovery_ids,
        )
        .await?;
    let StartupScanSessionOutcome::RecoveredModelCall(recovered) = scan else {
        panic!("the startup scan classifies the prepared continuation call instead of aborting");
    };
    assert!(
        matches!(*recovered, ModelCallTerminalOutcome::Failed(_)),
        "the lost prepared continuation call closes as a known failure"
    );
    let recovered_shape: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT terminal_disposition_kind, terminal_model_call_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        recovered_shape,
        (String::from("failed"), Some(continuation_call.into_uuid()),),
        "restart recovery names the steering-consuming continuation call"
    );

    let post_recovery = SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 0x33,
                seed + 1,
                "work after recovered continuation",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x34)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 0x35))),
        )
        .await?;
    assert!(
        matches!(
            post_recovery,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::TurnOrigin(_)
            ))
        ),
        "the failed terminal continuation shape must reconstitute before the next submit"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S02 / S07 / S10 / INV-006 / INV-037: an interrupt applied while the
/// prepared continuation call of a completed tool round awaits send cancels
/// the turn naming that call, and the committed terminal shape reloads
/// through the scheduling projection — the interrupt successor activates
/// instead of leaving the session permanently unloadable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s07_s10_inv006_inv037_interrupted_continuation_call_reloads_and_activates_successor()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x8100;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;
    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    let continuation_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x22));
    tool_repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x21)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || continuation_attempt,
        )
        .await?;
    let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0x23));
    tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            tool_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?;
    let authorized_attempt = tool_repository
        .authorize_attempt(fixture.session, fixture.turn, tool_attempt)
        .await?;
    tool_repository
        .commit_observation(
            authorized_attempt
                .executor_fence()
                .bind(ToolAttemptObservation::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(String::from("2026-07-26T12:00:00Z"))
                            .expect("bounded result"),
                    ),
                }),
        )
        .await?;

    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one continuation target forms a catalog");
    let continuation_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0x28));
    let continuation = PostgresToolLoopRepository::with_model_calls(
        pool.clone(),
        targets,
        model_credential_reference(),
    )
    .prepare_continuation(
        fixture.session,
        fixture.turn,
        fixture.call,
        signalbox_application::ToolContinuationIdentities::new(
            vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                seed + 0x26,
            ))],
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x27)),
            continuation_call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x29)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2a)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2b)),
        ),
        |_| panic!("the fixture has no pending steering"),
    )
    .await?;
    assert_eq!(
        continuation,
        signalbox_application::PrepareToolContinuationOutcome::Checkpointed(continuation_call)
    );

    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 0x30));
    let interrupt_outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 0x24,
                seed + 1,
                "stop the prepared continuation",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x25)),
            Some(successor),
        )
        .await?;
    assert!(matches!(
        interrupt_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(_))
    ));
    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct CancelledContinuationShape {
        turn_disposition: String,
        terminal_model_call_id: Option<Uuid>,
        call_disposition: String,
    }
    let cancelled_shape: CancelledContinuationShape = sqlx::query_as(
        "SELECT lifecycle.terminal_disposition_kind AS turn_disposition,
                lifecycle.terminal_model_call_id,
                continuation.terminal_disposition_kind AS call_disposition
           FROM turn_lifecycle AS lifecycle
           JOIN model_call AS continuation
             ON continuation.session_id = lifecycle.session_id
            AND continuation.model_call_id = lifecycle.terminal_model_call_id
          WHERE lifecycle.session_id = $1
            AND lifecycle.turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        cancelled_shape,
        CancelledContinuationShape {
            turn_disposition: String::from("cancelled"),
            terminal_model_call_id: Some(continuation_call.into_uuid()),
            call_disposition: String::from("cancelled"),
        },
        "the interrupt terminalizes the turn naming its unsent continuation call"
    );

    let mut recovery_ids = FixedStartupScanIds::new([], []);
    let scan = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.session,
            signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x31)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x32)),
            ),
            &mut recovery_ids,
        )
        .await?;
    assert_eq!(
        scan,
        StartupScanSessionOutcome::NoActiveTurn,
        "writer-produced cancelled continuation history must reconstitute at startup"
    );

    let mut scheduling_probe = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                seed + 0x2c,
            ))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2d))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x2e))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) =
        scheduling_probe.execute(fixture.session).await?
    else {
        panic!("the interrupt successor activates behind the cancelled continuation call");
    };
    assert_eq!(activated.turn(), successor);

    pool.close().await;
    drop(container);
    Ok(())
}

/// Drives one checkpoint-confirmed tool round through approval, execution,
/// and the steering-free continuation transaction, then authorizes the
/// prepared continuation call for send, leaving it durably in flight.
async fn authorize_continuation_after_completed_round(
    pool: &PgPool,
    seed: u128,
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        ModelCallId,
        AuthorizedModelCall,
    ),
    Box<dyn Error>,
> {
    let (fixture, model_repository, _, request) =
        checkpoint_confirmed_tool_round(pool, seed, "current_time", "{}").await?;
    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    let continuation_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x22));
    tool_repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x21)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || continuation_attempt,
        )
        .await?;
    let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0x23));
    tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            tool_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?;
    let authorized_attempt = tool_repository
        .authorize_attempt(fixture.session, fixture.turn, tool_attempt)
        .await?;
    tool_repository
        .commit_observation(
            authorized_attempt
                .executor_fence()
                .bind(ToolAttemptObservation::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(String::from("2026-07-26T12:00:00Z"))
                            .expect("bounded result"),
                    ),
                }),
        )
        .await?;
    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one continuation target forms a catalog");
    let continuation_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0x28));
    let continuation = PostgresToolLoopRepository::with_model_calls(
        pool.clone(),
        targets,
        model_credential_reference(),
    )
    .prepare_continuation(
        fixture.session,
        fixture.turn,
        fixture.call,
        signalbox_application::ToolContinuationIdentities::new(
            vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                seed + 0x26,
            ))],
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x27)),
            continuation_call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x29)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2a)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2b)),
        ),
        |_| panic!("the fixture has no pending steering"),
    )
    .await?;
    assert_eq!(
        continuation,
        signalbox_application::PrepareToolContinuationOutcome::Checkpointed(continuation_call)
    );
    let AuthorizeModelCallOutcome::Authorized(authorized) = model_repository
        .authorize_send(fixture.session, continuation_call)
        .await?
    else {
        panic!("the checkpointed continuation call authorizes for send")
    };
    Ok((fixture, model_repository, continuation_call, *authorized))
}

/// S02 / S10 / INV-006: a provider refusal on the continuation model call of
/// a completed tool round terminalizes the turn naming that call, and the
/// committed refused terminal shape reloads through the scheduling
/// projection — the startup scan completes and the next submit is accepted
/// instead of the session becoming permanently unloadable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s10_inv006_refused_continuation_call_reloads_and_scans() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x8300;
    let (fixture, model_repository, continuation_call, authorized) =
        authorize_continuation_after_completed_round(&pool, seed).await?;
    let refused_observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Refused);
    let refused_outcome = model_repository
        .apply_terminal_observation(
            fixture.session,
            refused_observation,
            ModelCallTerminalIdentities::Refused(RefusedModelCallTurnIdentities::new(
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x36)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert!(
        matches!(refused_outcome, ModelCallTerminalOutcome::Refused(_)),
        "the provider refusal terminalizes the continuation call's turn"
    );
    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct RefusedContinuationShape {
        turn_disposition: String,
        terminal_model_call_id: Option<Uuid>,
        call_disposition: String,
    }
    let refused_shape: RefusedContinuationShape = sqlx::query_as(
        "SELECT lifecycle.terminal_disposition_kind AS turn_disposition,
                lifecycle.terminal_model_call_id,
                continuation.terminal_disposition_kind AS call_disposition
           FROM turn_lifecycle AS lifecycle
           JOIN model_call AS continuation
             ON continuation.session_id = lifecycle.session_id
            AND continuation.model_call_id = lifecycle.terminal_model_call_id
          WHERE lifecycle.session_id = $1
            AND lifecycle.turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        refused_shape,
        RefusedContinuationShape {
            turn_disposition: String::from("refused"),
            terminal_model_call_id: Some(continuation_call.into_uuid()),
            call_disposition: String::from("refused"),
        },
        "the refusal names the round's own continuation call"
    );

    let mut recovery_ids = FixedStartupScanIds::new([], []);
    let scan = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.session,
            signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x31)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x32)),
            ),
            &mut recovery_ids,
        )
        .await?;
    assert_eq!(
        scan,
        StartupScanSessionOutcome::NoActiveTurn,
        "writer-produced refused continuation history must reconstitute at startup"
    );

    let post_refusal = SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 0x33,
                seed + 1,
                "work after refused continuation",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x34)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 0x35))),
        )
        .await?;
    assert!(
        matches!(
            post_refusal,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::TurnOrigin(_)
            ))
        ),
        "the refused terminal continuation shape must reconstitute before the next submit"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S04 / INV-006 / INV-025: a daemon restart with the continuation model call
/// of a completed tool round in flight classifies the call as ambiguous and
/// parks the turn awaiting a user recovery decision — the committed
/// recovery wait reloads through the scheduling projection, the reconcile
/// verb's precondition still names the parked turn, and the reconciling
/// interrupt terminalizes the turn naming that call.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_inv006_inv025_in_flight_continuation_call_restart_parks_recovery()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x8500;
    let (fixture, _, continuation_call, _) =
        authorize_continuation_after_completed_round(&pool, seed).await?;

    let mut recovery_ids = FixedStartupScanIds::new([], []);
    let scan = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.session,
            signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x31)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x32)),
            ),
            &mut recovery_ids,
        )
        .await?;
    let StartupScanSessionOutcome::RecoveredModelCall(recovered) = scan else {
        panic!("the startup scan classifies the in-flight continuation call instead of aborting");
    };
    assert!(
        matches!(*recovered, ModelCallTerminalOutcome::AwaitingRecovery(_)),
        "the lost in-flight continuation call parks awaiting a user decision"
    );

    let mut second_scan_ids = FixedStartupScanIds::new([], []);
    let second_scan = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.session,
            signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x33)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x34)),
            ),
            &mut second_scan_ids,
        )
        .await?;
    assert_eq!(
        second_scan,
        StartupScanSessionOutcome::AwaitingRecoveryDecision { turn: fixture.turn },
        "the committed continuation recovery wait must reconstitute at the next startup"
    );

    assert_eq!(
        ProcessReadRepository::new(pool.clone())
            .model_call_recovery_precondition(fixture.session)
            .await?,
        ProcessModelCallRecoveryPrecondition::Parked { turn: fixture.turn },
        "the reconcile verb's precondition names the parked continuation turn"
    );

    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 0x40));
    let reconcile_outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 0x41,
                seed + 1,
                "reconcile the parked continuation call",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x42)),
            Some(successor),
        )
        .await?;
    assert!(
        matches!(
            reconcile_outcome,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(_))
        ),
        "the reconcile verb's interrupt applies against the parked continuation turn"
    );
    let reconciled_shape: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT terminal_disposition_kind, terminal_model_call_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        reconciled_shape,
        (
            String::from("reconciliation_required"),
            Some(continuation_call.into_uuid()),
        ),
        "reconciliation retains the exact ambiguous continuation call"
    );

    let mut third_scan_ids = FixedStartupScanIds::new([], []);
    let third_scan = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.session,
            signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x43)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x44)),
            ),
            &mut third_scan_ids,
        )
        .await?;
    assert_eq!(
        third_scan,
        StartupScanSessionOutcome::NoActiveTurn,
        "the reconciliation-required continuation terminal must reconstitute at startup"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S04 / S07 / INV-006 / INV-037: a daemon restart with a stop-requested
/// continuation call classifies it as ambiguous under its applied interrupt
/// and terminalizes the turn as reconciliation-required naming that call —
/// the committed terminal shape reloads through the scheduling projection
/// instead of leaving the session permanently unloadable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_s07_inv006_inv037_stop_requested_continuation_call_restart_reconciles()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x8700;
    let (fixture, _, continuation_call, _) =
        authorize_continuation_after_completed_round(&pool, seed).await?;

    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 0x30));
    let interrupt_outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 0x24,
                seed + 1,
                "stop the in-flight continuation",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x25)),
            Some(successor),
        )
        .await?;
    assert!(
        matches!(
            interrupt_outcome,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(_))
        ),
        "the interrupt records a stop request against the in-flight continuation call"
    );

    let mut recovery_ids = FixedStartupScanIds::new([], []);
    let scan = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.session,
            signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x31)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x32)),
            ),
            &mut recovery_ids,
        )
        .await?;
    let StartupScanSessionOutcome::RecoveredModelCall(recovered) = scan else {
        panic!("the startup scan classifies the stop-requested continuation call");
    };
    assert!(
        matches!(
            *recovered,
            ModelCallTerminalOutcome::ReconciliationRequired(_)
        ),
        "the lost stop-requested continuation call requires reconciliation"
    );
    let reconciled_shape: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT terminal_disposition_kind, terminal_model_call_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        reconciled_shape,
        (
            String::from("reconciliation_required"),
            Some(continuation_call.into_uuid()),
        ),
        "restart reconciliation names the stop-requested continuation call"
    );

    let mut second_scan_ids = FixedStartupScanIds::new([], []);
    let second_scan = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.session,
            signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x33)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x34)),
            ),
            &mut second_scan_ids,
        )
        .await?;
    assert_eq!(
        second_scan,
        StartupScanSessionOutcome::NoActiveTurn,
        "the reconciliation-required continuation terminal must reconstitute at startup"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-006 / INV-011 / INV-037: an immediate interrupt after an approved
/// attempt checkpoint classifies the unsent attempt, closes its logical
/// request, and terminalizes through the applied interrupt atomically.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv006_inv011_inv037_interrupt_closes_checkpointed_tool_execution()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7480;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;
    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    tool_repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 23)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 24)),
        )
        .await?;
    let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 25));
    tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            tool_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?;

    let outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 26,
                seed + 1,
                "stop checkpointed tool",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 27)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 28))),
        )
        .await?;
    assert!(matches!(
        outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(_))
    ));
    assert!(
        tool_repository
            .prepare_next_attempt(
                fixture.session,
                fixture.turn,
                ToolAttemptId::from_uuid(Uuid::from_u128(seed + 29)),
                ToolEffectClass::EffectFree,
            )
            .await?
            .is_none(),
        "a winning interrupt makes stale attempt preparation a clean no-op"
    );

    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT entry.payload_kind
           FROM semantic_transcript_entry AS entry
          WHERE entry.source_session_id = $1
            AND (
                entry.tool_result_attempt_id = $2
                OR entry.cancelled_turn_id = $3
            )",
    )
    .bind(fixture.session.into_uuid())
    .bind(tool_attempt.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row == "tool_execution_result"));
    assert!(rows.iter().any(|row| row == "turn_cancelled"));
    let attempt_end: (String, String) = sqlx::query_as(
        "SELECT terminal_disposition_kind, error_kind
           FROM tool_attempt
          WHERE attempt_id = $1",
    )
    .bind(tool_attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        attempt_end,
        (String::from("known_failed"), String::from("crash_lost"))
    );

    let disposition: String = sqlx::query_scalar(
        "SELECT terminal_disposition_kind
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(disposition, "cancelled");

    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one continuation target forms a catalog");
    let stale_continuation = PostgresToolLoopRepository::with_model_calls(
        pool.clone(),
        targets,
        model_credential_reference(),
    )
    .prepare_continuation(
        fixture.session,
        fixture.turn,
        fixture.call,
        signalbox_application::ToolContinuationIdentities::new(
            vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                seed + 29,
            ))],
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 30)),
            ModelCallId::from_uuid(Uuid::from_u128(seed + 31)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 32)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 33)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 34)),
        ),
        |_| panic!("an interrupted batch cannot consume steering"),
    )
    .await?;
    assert_eq!(
        stale_continuation,
        signalbox_application::PrepareToolContinuationOutcome::NoWork,
        "an interrupt that consumed the batch makes a stale continuation hint no work"
    );

    let mut cancellation_dispatched = false;
    drain_outbox(&pool, |event| {
        if matches!(
            event.kind(),
            DispatchedOutboxEventKind::TurnCancelled { turn, .. }
                if *turn == fixture.turn
        ) {
            cancellation_dispatched = true;
        }
    })
    .await?;
    assert!(
        cancellation_dispatched,
        "tool-batch cancellation must remain deliverable after its producing call"
    );

    let follow_up = SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 40,
                seed + 1,
                "work after cancelled tool round",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 41)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 42))),
        )
        .await?;
    assert!(
        matches!(
            follow_up,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::TurnOrigin(_)
            ))
        ),
        "writer-produced cancelled tool history must reconstitute before the next submit"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S07 / S10 / INV-012 / INV-028: an interrupt against a parked approval wait
/// records the authoritative typed rejection instead of failing the submit
/// transaction, the wait remains durably parked with no accepted input, and
/// equal replay returns the recorded rejection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s07_s10_inv012_inv028_parked_approval_interrupt_records_typed_rejection()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7e00;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;
    let interrupt_command = Uuid::from_u128(seed + 23);
    let parked_before: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT active_phase_kind, approval_tool_request_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        parked_before,
        (
            String::from("awaiting_tool_approval"),
            Some(request.into_uuid()),
        ),
        "the confirmed tool round must be parked before the interrupt"
    );

    let interrupt = input_with_delivery(
        seed + 23,
        seed + 1,
        "stop while confirm is pending",
        DeliveryRequest::Interrupt {
            expected_active_turn: fixture.turn,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            interrupt.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 24)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 25))),
        )
        .await?;
    assert!(
        matches!(
            outcome,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
                SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
                    session,
                    active_turn,
                },
            )) if session == fixture.session && active_turn == fixture.turn
        ),
        "an interrupt alone must not bypass the decision command: {outcome:?}"
    );

    let parked_after: (String, Option<Uuid>, i64) = sqlx::query_as(
        "SELECT active_phase_kind, approval_tool_request_id,
                (SELECT count(*) FROM accepted_input
                  WHERE accepting_command_id = $3)
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(interrupt_command)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        parked_after,
        (
            String::from("awaiting_tool_approval"),
            Some(request.into_uuid()),
            0,
        ),
        "the approval wait must remain parked and the rejection must accept no input"
    );

    let replayed = SubmitInputRepository::new(pool.clone())
        .handle(
            interrupt,
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 26)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 27))),
        )
        .await?;
    assert!(
        matches!(
            replayed,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
                SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
                    session,
                    active_turn,
                },
            )) if session == fixture.session && active_turn == fixture.turn
        ),
        "equal replay must return the recorded parked-approval rejection: {replayed:?}"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S07 / S10 / INV-012 / INV-028: a parked-approval interrupt rejection is
/// authoritative only against a turn the database still records as active on
/// its approval wait. The row shape proves only that the receipt names the
/// turn the command expected, so the deferred correlation trigger proves the
/// phase: a directly inserted receipt naming a running or a terminal turn
/// cannot commit and therefore never replays as authoritative.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s07_s10_inv012_inv028_parked_approval_rejection_requires_a_recorded_approval_wait()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let running_seed = 0x7f00;
    let running = checkpoint_restart_model_call(&pool, running_seed, false).await?;
    let running_phase: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, active_phase_kind
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(running.session.into_uuid())
    .bind(running.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        running_phase,
        (String::from("active"), Some(String::from("running"))),
        "the fixture turn must be running before the forged receipt names it"
    );
    let running_error = insert_parked_approval_interrupt_rejection(
        &pool,
        Uuid::from_u128(running_seed + 0x30),
        Uuid::from_u128(running_seed + 8),
        running.turn.into_uuid(),
    )
    .await
    .expect_err("a parked-approval rejection naming a running turn is corruption");
    assert_eq!(
        running_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23503".into())
    );
    assert!(
        running_error
            .to_string()
            .contains("incomplete or cross-wired effect"),
        "the correlation trigger must refuse the running turn: {running_error}"
    );

    let terminal_seed = 0x7f80;
    let terminal = checkpoint_restart_model_call(&pool, terminal_seed, false).await?;
    let selection =
        signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(terminal_seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(terminal_seed + 6));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one restart fixture target forms a catalog");
    PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference())
        .fail_prepared_call(
            terminal.session,
            terminal.call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(terminal_seed + 14)),
                ContextFrontierId::from_uuid(Uuid::from_u128(terminal_seed + 15)),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    let terminal_phase: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, active_phase_kind
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(terminal.session.into_uuid())
    .bind(terminal.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        terminal_phase,
        (String::from("terminal"), None),
        "the fixture turn must be terminal before the forged receipt names it"
    );
    let terminal_error = insert_parked_approval_interrupt_rejection(
        &pool,
        Uuid::from_u128(terminal_seed + 0x30),
        Uuid::from_u128(terminal_seed + 8),
        terminal.turn.into_uuid(),
    )
    .await
    .expect_err("a parked-approval rejection naming a terminal turn is corruption");
    assert_eq!(
        terminal_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23503".into())
    );
    assert!(
        terminal_error
            .to_string()
            .contains("incomplete or cross-wired effect"),
        "the correlation trigger must refuse the terminal turn: {terminal_error}"
    );

    let forged: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM submit_input_command
          WHERE rejection_kind = 'interrupt_unavailable_while_awaiting_approval'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(forged, 0, "no forged parked-approval receipt may survive");

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-006 / INV-025 / INV-029 / INV-037: an interrupt against an external
/// tool recovery wait releases the slot as reconciliation-required while
/// retaining the exact ambiguous tool attempt and closing its logical request.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv006_inv025_inv029_inv037_interrupt_preserves_tool_recovery_ambiguity()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x74c0;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "external-tool", "{}").await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let issuing_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 23));
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 24)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || issuing_attempt,
        )
        .await?;
    let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 25));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            tool_attempt,
            ToolEffectClass::ExternalEffect,
        )
        .await?;
    repository
        .authorize_attempt(fixture.session, fixture.turn, tool_attempt)
        .await?;
    let mut recovery_ids = FixedStartupScanIds::new([], []);
    assert_ambiguous_tool_recovery(
        PostgresStartupScanRepository::new(pool.clone())
            .recover(
                fixture.session,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 26)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 27)),
                ),
                &mut recovery_ids,
            )
            .await?,
    );

    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 30));
    let outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 28,
                seed + 1,
                "stop ambiguous tool",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 29)),
            Some(successor),
        )
        .await?;
    assert!(matches!(
        outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(_))
    ));

    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct DurableToolReconciliationFacts {
        terminal_disposition_kind: String,
        terminal_model_call_id: Option<Uuid>,
        terminal_tool_attempt_id: Option<Uuid>,
        outbox_event_count: i64,
    }
    let durable: DurableToolReconciliationFacts = sqlx::query_as(
        "SELECT lifecycle.terminal_disposition_kind,
                lifecycle.terminal_model_call_id,
                lifecycle.terminal_tool_attempt_id,
                (SELECT count(*)
                   FROM turn_reconciliation_required_outbox_event AS event
                  WHERE event.session_id = lifecycle.session_id
                    AND event.turn_id = lifecycle.turn_id
                    AND event.model_call_id IS NULL
                    AND event.tool_attempt_id = $3
                    AND event.terminal_frontier_id =
                        lifecycle.terminal_frontier_id) AS outbox_event_count
           FROM turn_lifecycle AS lifecycle
          WHERE lifecycle.session_id = $1
            AND lifecycle.turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(tool_attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        durable,
        DurableToolReconciliationFacts {
            terminal_disposition_kind: String::from("reconciliation_required"),
            terminal_model_call_id: None,
            terminal_tool_attempt_id: Some(tool_attempt.into_uuid()),
            outbox_event_count: 1,
        }
    );

    let snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(fixture.session)
        .await?
        .expect("tool reconciliation remains process-readable");
    assert_eq!(
        process_tool_reconciliation_operation(snapshot.turns()[0].state()),
        (issuing_attempt, tool_attempt)
    );
    assert_eq!(assistant_tool_request(snapshot.entries()), request);
    assert_eq!(closed_tool_request(snapshot.entries()), request);
    assert!(
        dispatched_tool_reconciliation(&pool, fixture.turn, tool_attempt).await?,
        "the tool reconciliation event must not block dispatch"
    );

    assert_eq!(
        activated_turn(
            StartEligibleTurnRepository::new(pool.clone())
                .handle(
                    fixture.session,
                    AcceptedInputTurnActivationIdentities::new(
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 34)),
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 31)),
                        ContextFrontierId::from_uuid(Uuid::from_u128(seed + 32)),
                        TurnAttemptId::from_uuid(Uuid::from_u128(seed + 33)),
                    ),
                )
                .await?,
        ),
        successor
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S05 / S10 / S11 / INV-006 / INV-019 / INV-027: denial never dispatches,
/// schema failure is durable result evidence, external-effect crash loss parks
/// on exact recovery authority, and effect-free loss closes every request
/// before the turn fails.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s05_s10_s11_inv006_inv019_inv027_tool_failures_close_durably() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());

    let deny_seed = 0x7500;
    let (denied_fixture, _, _, denied_request) =
        checkpoint_confirmed_tool_round(&pool, deny_seed, "dangerous-tool", "{}").await?;
    let approval_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(denied_fixture.session)
        .await?
        .expect("approval waits remain process-readable");
    assert!(matches!(
        approval_snapshot.turns()[0].state(),
        ProcessTurnState::ActiveAwaitingToolApproval { request }
            if *request == denied_request
    ));
    let mut forged_blanket = pool.begin().await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source, denial_reason,
             owner_command_id)
         VALUES ($1, 'approve', 'session_blanket', NULL, NULL)",
    )
    .bind(denied_request.into_uuid())
    .execute(&mut *forged_blanket)
    .await?;
    let forged_blanket_error =
        sqlx::query("SET CONSTRAINTS tool_approval_session_blanket_provenance IMMEDIATE")
            .execute(&mut *forged_blanket)
            .await
            .expect_err("disabled frozen configuration cannot authorize a blanket approval");
    assert_eq!(
        forged_blanket_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("tool_approval_session_blanket_requires_frozen_approve_all")
    );
    forged_blanket.rollback().await?;
    let malformed_command_error = sqlx::query(
        "INSERT INTO decide_tool_request_command
            (command_id, command_kind, storage_version, request_id,
             decision_kind, denial_reason, result_kind, rejection_kind,
             result_earliest_undecided_request_id)
         VALUES ($1, 'decide_tool_request', 1, $2,
                 'deny', E'unsafe\\nreason', 'applied', NULL, NULL)",
    )
    .bind(Uuid::from_u128(deny_seed + 89))
    .bind(denied_request.into_uuid())
    .execute(&pool)
    .await
    .expect_err("stored decision command reason must reject control characters");
    assert_eq!(
        malformed_command_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("decide_tool_request_command_decision_shape")
    );
    let mut malformed_denial = pool.begin().await?;
    let malformed_command = Uuid::from_u128(deny_seed + 90);
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'decide_tool_request', 1, transaction_timestamp())",
    )
    .bind(malformed_command)
    .execute(&mut *malformed_denial)
    .await?;
    sqlx::query(
        "INSERT INTO decide_tool_request_command
            (command_id, command_kind, storage_version, request_id,
             decision_kind, denial_reason, result_kind, rejection_kind,
             result_earliest_undecided_request_id)
         VALUES ($1, 'decide_tool_request', 1, $2,
                 'deny', 'safe', 'applied', NULL, NULL)",
    )
    .bind(malformed_command)
    .bind(denied_request.into_uuid())
    .execute(&mut *malformed_denial)
    .await?;
    let malformed_denial_error = sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source, denial_reason,
             owner_command_id)
         VALUES ($1, 'deny', 'owner_command', E'unsafe\\nreason', $2)",
    )
    .bind(denied_request.into_uuid())
    .bind(malformed_command)
    .execute(&mut *malformed_denial)
    .await
    .expect_err("stored denial reason must reject control characters");
    assert_eq!(
        malformed_denial_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("tool_approval_decision_shape")
    );
    malformed_denial.rollback().await?;

    let denied_continuation = TurnAttemptId::from_uuid(Uuid::from_u128(deny_seed + 23));
    let denial = repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(deny_seed + 24)),
                denied_request,
                ToolApprovalDecision::Deny { reason: None },
            ),
            || denied_continuation,
        )
        .await?;
    assert!(matches!(
        denial.result(),
        DecideToolRequestResult::Applied(applied)
            if matches!(
                applied.resolution().decision(),
                ToolApprovalDecision::Deny { .. }
            )
    ));
    assert!(matches!(
        repository
            .decide(
                decide_tool_request(
                    DurableCommandId::from_uuid(Uuid::from_u128(deny_seed + 25)),
                    denied_request,
                    ToolApprovalDecision::Approve,
                ),
                || panic!("resolved request consumes no identity"),
            )
            .await?
            .result(),
        DecideToolRequestResult::Rejected(
            signalbox_domain::DecideToolRequestRejectedResult::AlreadyResolved { request }
        ) if *request == denied_request
    ));
    let denied_batch = repository
        .load_active_batch(denied_fixture.session, denied_fixture.turn)
        .await?
        .expect("denied batch remains available for reference-only projection");
    assert!(matches!(
        repository
            .prepare_next_attempt(
                denied_fixture.session,
                denied_fixture.turn,
                ToolAttemptId::from_uuid(Uuid::from_u128(deny_seed + 26)),
                ToolEffectClass::ExternalEffect,
            )
            .await,
        Err(ToolLoopRepositoryError::InvalidTransition(
            "batch has no next serialized attempt"
        ))
    ));
    let denied_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(deny_seed + 27));
    let denied_projection = denied_batch
        .prepare_result_projection(
            vec![denied_entry],
            ContextFrontierId::from_uuid(Uuid::from_u128(deny_seed + 28)),
        )
        .expect("denial is a complete logical result");
    assert_eq!(denied_projection.entries().len(), 1);

    let schema_seed = 0x7600;
    let (schema_fixture, _, _, schema_request) =
        checkpoint_confirmed_tool_round(&pool, schema_seed, "current_time", "{broken").await?;
    let schema_continuation = TurnAttemptId::from_uuid(Uuid::from_u128(schema_seed + 23));
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(schema_seed + 24)),
                schema_request,
                ToolApprovalDecision::Approve,
            ),
            || schema_continuation,
        )
        .await?;
    let schema_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(schema_seed + 25));
    repository
        .prepare_next_attempt(
            schema_fixture.session,
            schema_fixture.turn,
            schema_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?;
    let mut malformed_error_detail = pool.begin().await?;
    let malformed_detail_error = sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                error_kind = 'invalid_arguments',
                error_detail = E'unsafe\\ndetail'
          WHERE attempt_id = $1",
    )
    .bind(schema_attempt.into_uuid())
    .execute(&mut *malformed_error_detail)
    .await
    .expect_err("stored execution detail must reject control characters");
    assert_eq!(
        malformed_detail_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("tool_attempt_error_detail_bounded")
    );
    malformed_error_detail.rollback().await?;
    let schema_failure = repository
        .commit_preflight_error(
            schema_fixture.session,
            schema_fixture.turn,
            schema_attempt,
            ToolExecutionError::new(ToolExecutionErrorKind::InvalidArguments, None),
        )
        .await?;
    assert!(matches!(
        schema_failure.end(),
        ToolAttemptEnd::KnownFailed { error }
            if error.kind() == ToolExecutionErrorKind::InvalidArguments
    ));
    let issuing_attempt_state: String = sqlx::query_scalar(
        "SELECT state_kind
           FROM turn_attempt
          WHERE turn_attempt_id = $1",
    )
    .bind(schema_continuation.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        issuing_attempt_state, "running",
        "preflight terminal evidence makes the result projection continuation-eligible"
    );
    let mut completed_attempt_recovery_ids = FixedStartupScanIds::new([], []);
    assert!(matches!(
        PostgresStartupScanRepository::new(pool.clone())
            .recover(
                schema_fixture.session,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(schema_seed + 90)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(schema_seed + 91)),
                ),
                &mut completed_attempt_recovery_ids,
            )
            .await?,
        StartupScanSessionOutcome::ResumableToolBatch { turn }
            if turn == schema_fixture.turn
    ));
    let mut recovery_sweep = PostgresEligibilitySweep::new(pool.clone());
    let mut recovery_sessions = Vec::new();
    loop {
        let (page, continuation) = recovery_sweep.find_sessions().await?.into_parts();
        recovery_sessions.extend(page);
        if !continuation {
            break;
        }
    }
    assert!(
        recovery_sessions.contains(&schema_fixture.session),
        "the durable sweep must reschedule a resumable active tool batch"
    );
    let schema_batch = repository
        .load_active_batch(schema_fixture.session, schema_fixture.turn)
        .await?
        .expect("schema failure remains exact terminal attempt evidence");
    let schema_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(schema_seed + 26));
    let schema_projection = schema_batch
        .prepare_result_projection(
            vec![schema_entry],
            ContextFrontierId::from_uuid(Uuid::from_u128(schema_seed + 27)),
        )
        .expect("definitive preflight failure projects as a tool result");
    assert_eq!(schema_projection.entries().len(), 1);

    let crash_seed = 0x7700;
    let (crash_fixture, _, _, crash_request) =
        checkpoint_confirmed_tool_round(&pool, crash_seed, "external-tool", "{}").await?;
    let crash_continuation = TurnAttemptId::from_uuid(Uuid::from_u128(crash_seed + 23));
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(crash_seed + 24)),
                crash_request,
                ToolApprovalDecision::Approve,
            ),
            || crash_continuation,
        )
        .await?;
    let crash_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(crash_seed + 25));
    repository
        .prepare_next_attempt(
            crash_fixture.session,
            crash_fixture.turn,
            crash_attempt,
            ToolEffectClass::ExternalEffect,
        )
        .await?;
    repository
        .authorize_attempt(crash_fixture.session, crash_fixture.turn, crash_attempt)
        .await?;
    let pending_ambiguous_input = AcceptedInputId::from_uuid(Uuid::from_u128(crash_seed + 29));
    assert!(matches!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                input_with_delivery(
                    crash_seed + 28,
                    crash_seed + 1,
                    "steer while external work is in flight",
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn: crash_fixture.turn,
                    },
                ),
                pending_ambiguous_input,
                None,
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));
    let mut crash_recovery_ids = FixedStartupScanIds::new([], []);
    assert!(matches!(
        PostgresStartupScanRepository::new(pool.clone())
            .recover(
                crash_fixture.session,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(crash_seed + 26)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(crash_seed + 27)),
                ),
                &mut crash_recovery_ids,
            )
            .await?,
        StartupScanSessionOutcome::RecoveredToolAttempt(outcome)
            if matches!(*outcome, ToolAttemptCrashOutcome::Ambiguous(_))
    ));
    let restarted = PostgresToolLoopRepository::new(pool.clone())
        .load_active_batch(crash_fixture.session, crash_fixture.turn)
        .await?
        .expect("external-effect ambiguity reloads after restart");
    assert!(matches!(
        restarted.awaiting_recovery(),
        Some(waiting) if waiting.attempt() == crash_attempt
    ));
    let recovery_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(crash_fixture.session)
        .await?
        .expect("tool recovery waits remain process-readable");
    assert!(matches!(
        recovery_snapshot.turns()[0].state(),
        ProcessTurnState::ActiveAwaitingToolRecovery {
            ended_attempt,
            recovery_attempt,
        } if *ended_attempt == crash_continuation && *recovery_attempt == crash_attempt
    ));
    let pending_ambiguous_disposition: String = sqlx::query_scalar(
        "SELECT disposition_kind
           FROM accepted_input
          WHERE accepted_input_id = $1",
    )
    .bind(pending_ambiguous_input.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(pending_ambiguous_disposition, "pending_steering");

    let durable_shape: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM tool_attempt
              WHERE request_id = $1),
            (SELECT count(*) FROM tool_attempt
              WHERE attempt_id = $2
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'known_failed'
                AND error_kind = 'invalid_arguments'),
            (SELECT count(*) FROM turn_lifecycle
              WHERE session_id = $3
                AND turn_id = $4
                AND state_kind = 'active'
                AND active_phase_kind = 'awaiting_tool_recovery'
                AND active_tool_round_call_id = $5
                AND recovery_tool_attempt_id = $6)",
    )
    .bind(denied_request.into_uuid())
    .bind(schema_attempt.into_uuid())
    .bind(crash_fixture.session.into_uuid())
    .bind(crash_fixture.turn.into_uuid())
    .bind(crash_fixture.call.into_uuid())
    .bind(crash_attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_shape, (0, 1, 1));

    let effect_free_seed = 0x7800;
    let (effect_free_fixture, _, _, effect_free_request) =
        checkpoint_confirmed_tool_round(&pool, effect_free_seed, "current_time", "{}").await?;
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(effect_free_seed + 24)),
                effect_free_request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(effect_free_seed + 23)),
        )
        .await?;
    let effect_free_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(effect_free_seed + 25));
    repository
        .prepare_next_attempt(
            effect_free_fixture.session,
            effect_free_fixture.turn,
            effect_free_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?;
    repository
        .authorize_attempt(
            effect_free_fixture.session,
            effect_free_fixture.turn,
            effect_free_attempt,
        )
        .await?;
    let pending_effect_free_input =
        AcceptedInputId::from_uuid(Uuid::from_u128(effect_free_seed + 29));
    assert!(matches!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                input_with_delivery(
                    effect_free_seed + 28,
                    effect_free_seed + 1,
                    "steer after effect-free dispatch",
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn: effect_free_fixture.turn,
                    },
                ),
                pending_effect_free_input,
                None,
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));
    let recovered_effect_free_turn = TurnId::from_uuid(Uuid::from_u128(effect_free_seed + 30));
    let mut effect_free_recovery_ids = FixedStartupScanIds::new(
        [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
            effect_free_seed + 31,
        ))],
        [ContextFrontierId::from_uuid(Uuid::from_u128(
            effect_free_seed + 32,
        ))],
    )
    .with_reclassified_turns([recovered_effect_free_turn]);
    assert!(matches!(
        PostgresStartupScanRepository::new(pool.clone())
            .recover(
                effect_free_fixture.session,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(effect_free_seed + 26)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(effect_free_seed + 27)),
                ),
                &mut effect_free_recovery_ids,
            )
            .await?,
        StartupScanSessionOutcome::RecoveredToolAttempt(outcome)
            if matches!(*outcome, ToolAttemptCrashOutcome::KnownFailed(_))
    ));
    let effect_free_shape: (String, String, String, String, Uuid) = sqlx::query_as(
        "SELECT
            (SELECT state_kind FROM turn_lifecycle WHERE turn_id = $1),
            (SELECT terminal_disposition_kind FROM turn_lifecycle WHERE turn_id = $1),
            (SELECT error_kind FROM tool_attempt WHERE attempt_id = $2),
            (SELECT disposition_kind FROM accepted_input
              WHERE accepted_input_id = $3),
            (SELECT origin_turn_id FROM accepted_input
              WHERE accepted_input_id = $3)",
    )
    .bind(effect_free_fixture.turn.into_uuid())
    .bind(effect_free_attempt.into_uuid())
    .bind(pending_effect_free_input.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        effect_free_shape,
        (
            "terminal".to_owned(),
            "failed".to_owned(),
            "crash_lost".to_owned(),
            "reclassified_as_turn_origin".to_owned(),
            recovered_effect_free_turn.into_uuid(),
        )
    );
    let terminal_kinds: Vec<String> = sqlx::query_scalar(
        "SELECT array_agg(entry.payload_kind ORDER BY member.member_position)
           FROM context_frontier_member AS member
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
          WHERE member.owning_session_id = $1
            AND member.context_frontier_id = $2",
    )
    .bind(effect_free_fixture.session.into_uuid())
    .bind(Uuid::from_u128(effect_free_seed + 27))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        terminal_kinds,
        [
            "origin_accepted_input",
            "assistant_tool_use",
            "tool_execution_result",
            "turn_failed",
        ]
    );
    let mut reordered_terminal = pool.begin().await?;
    sqlx::query(
        "ALTER TABLE context_frontier_delta
         DISABLE TRIGGER context_frontier_member_is_append_only",
    )
    .execute(&mut *reordered_terminal)
    .await?;
    for (entry, position) in [
        (Uuid::from_u128(effect_free_seed + 31), 99_i64),
        (Uuid::from_u128(effect_free_seed + 26), 3_i64),
        (Uuid::from_u128(effect_free_seed + 31), 4_i64),
    ] {
        sqlx::query(
            "UPDATE context_frontier_delta
                SET member_position = $1
              WHERE owning_session_id = $2
                AND context_frontier_id = $3
                AND semantic_entry_id = $4",
        )
        .bind(position)
        .bind(effect_free_fixture.session.into_uuid())
        .bind(Uuid::from_u128(effect_free_seed + 27))
        .bind(entry)
        .execute(&mut *reordered_terminal)
        .await?;
    }
    let reordered_terminal_error = sqlx::query("SELECT assert_tool_loop_turn_final_state($1)")
        .bind(effect_free_fixture.turn.into_uuid())
        .execute(&mut *reordered_terminal)
        .await
        .expect_err("failure requires proposal-ordered tool results before its marker");
    assert_eq!(
        reordered_terminal_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("tool_loop_terminal_result_suffix_exact")
    );
    reordered_terminal.rollback().await?;
    let effect_free_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(effect_free_fixture.session)
        .await?
        .expect("known tool-crash failure remains process-readable");
    assert!(effect_free_snapshot.entries().iter().any(|entry| matches!(
        entry,
        ProcessTranscriptEntry::ToolExecutionResult {
            request,
            attempt,
            ..
        } if *request == effect_free_request && *attempt == effect_free_attempt
    )));
    let mut failure_dispatched = false;
    drain_outbox(&pool, |event| {
        if matches!(
            event.kind(),
            DispatchedOutboxEventKind::TurnFailed { turn, .. }
                if *turn == effect_free_fixture.turn
        ) {
            failure_dispatched = true;
        }
    })
    .await?;
    assert!(
        failure_dispatched,
        "known tool-crash failure must not be rejected for earlier call history"
    );

    let follow_up = SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                effect_free_seed + 40,
                effect_free_seed + 1,
                "work after failed tool round",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(effect_free_seed + 41)),
            Some(TurnId::from_uuid(Uuid::from_u128(effect_free_seed + 42))),
        )
        .await?;
    assert!(
        matches!(
            follow_up,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::TurnOrigin(_)
            ))
        ),
        "writer-produced failed tool history must reconstitute before the next submit"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-012: concurrent user-global command claims serialize before either
/// request-local decision can commit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv012_tool_decision_command_race_has_one_global_winner() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first_seed = 0x7900;
    let second_seed = 0x7a00;
    let (first, _, _, first_request) =
        checkpoint_confirmed_tool_round(&pool, first_seed, "current_time", "{}").await?;
    let (second, _, _, second_request) =
        checkpoint_confirmed_tool_round(&pool, second_seed, "current_time", "{}").await?;
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(0x7b00));
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let first_decision = repository.decide(
        decide_tool_request(command_id, first_request, ToolApprovalDecision::Approve),
        || TurnAttemptId::from_uuid(Uuid::from_u128(first_seed + 23)),
    );
    let second_decision = repository.decide(
        decide_tool_request(command_id, second_request, ToolApprovalDecision::Approve),
        || TurnAttemptId::from_uuid(Uuid::from_u128(second_seed + 23)),
    );
    let (first_result, second_result) = tokio::join!(first_decision, second_decision);
    assert!(
        matches!(
            (&first_result, &second_result),
            (Ok(_), Err(ToolLoopRepositoryError::ConflictingCommandReuse))
                | (Err(ToolLoopRepositoryError::ConflictingCommandReuse), Ok(_))
        ),
        "exactly one request-local decision wins the user-global identity"
    );
    let winner_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM tool_approval_decision
          WHERE request_id IN ($1, $2)
            AND owner_command_id = $3",
    )
    .bind(first_request.into_uuid())
    .bind(second_request.into_uuid())
    .bind(command_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(winner_count, 1);

    assert_ne!(first.session, second.session);
    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-006 / INV-012: an applied interrupt racing a tool-using response closes
/// every request in proposal order, binds those facts into the terminal
/// frontier, and makes a later user decision canonically AlreadyResolved.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv006_inv012_stopped_tool_round_closes_requests_and_decision_replay()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7c00;
    let (fixture, model_repository, _prepared, authorized) =
        authorize_checkpointed_model_call_with_prepared(&pool, seed).await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 19,
                seed + 1,
                "stop tool response",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 20)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 21))),
        )
        .await?;

    let first_request = signalbox_domain::ToolRequestId::from_uuid(Uuid::from_u128(seed + 22));
    let second_request = signalbox_domain::ToolRequestId::from_uuid(Uuid::from_u128(seed + 23));
    let response = ToolUsingAssistantResponse::try_from_parts(vec![
        AssistantResponsePart::ToolCall(ToolCallProposal::new(
            ToolName::try_new(String::from("first_tool")).expect("valid fixture tool name"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("bounded fixture arguments"),
        )),
        AssistantResponsePart::Text(
            AssistantText::try_new(String::from("between")).expect("valid fixture text"),
        ),
        AssistantResponsePart::ToolCall(ToolCallProposal::new(
            ToolName::try_new(String::from("second_tool")).expect("valid fixture tool name"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("bounded fixture arguments"),
        )),
    ])
    .expect("the fixture contains tool proposals");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools { response });
    let outcome = model_repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::StoppedToolRound(
                StoppedToolRoundModelCallIdentities::new(
                    vec![
                        StoppedToolResponsePartIdentity::tool_call(
                            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 24)),
                            first_request,
                            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 25)),
                            InitialToolApproval::Confirm,
                        ),
                        StoppedToolResponsePartIdentity::text(
                            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 26)),
                        ),
                        StoppedToolResponsePartIdentity::tool_call(
                            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 27)),
                            second_request,
                            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 28)),
                            InitialToolApproval::Confirm,
                        ),
                    ],
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 29)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 30)),
                ),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert!(matches!(
        outcome,
        ModelCallTerminalOutcome::CancelledWithToolResponse(_)
    ));

    let rejection = PostgresToolLoopRepository::new(pool.clone())
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 31)),
                first_request,
                ToolApprovalDecision::Approve,
            ),
            || panic!("turn-closed request consumes no continuation identity"),
        )
        .await?;
    assert!(matches!(
        rejection.result(),
        DecideToolRequestResult::Rejected(
            signalbox_domain::DecideToolRequestRejectedResult::AlreadyResolved { request }
        ) if *request == first_request
    ));
    let terminal_suffix: Vec<String> = sqlx::query_scalar(
        "SELECT entry.payload_kind
           FROM turn_lifecycle AS lifecycle
           JOIN context_frontier_member AS member
             ON member.owning_session_id = lifecycle.session_id
            AND member.context_frontier_id = lifecycle.terminal_frontier_id
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
          WHERE lifecycle.turn_id = $1
            AND entry.payload_kind IN (
                'tool_closed_by_turn_end',
                'turn_cancelled'
            )
          ORDER BY member.member_position",
    )
    .bind(fixture.turn.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        terminal_suffix,
        [
            "tool_closed_by_turn_end",
            "tool_closed_by_turn_end",
            "turn_cancelled"
        ]
    );

    let response_positions: Vec<(Uuid, Decimal)> = sqlx::query_as(
        "SELECT entry.semantic_entry_id, member.member_position
           FROM semantic_transcript_entry AS entry
           JOIN context_frontier_member AS member
             ON member.owning_session_id = entry.source_session_id
            AND member.context_frontier_id = $1
            AND member.semantic_entry_id = entry.semantic_entry_id
          WHERE entry.producing_model_call_id = $2
            AND entry.payload_kind IN ('assistant_text', 'assistant_tool_use')
          ORDER BY entry.assistant_response_part_ordinal",
    )
    .bind(Uuid::from_u128(seed + 30))
    .bind(fixture.call.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(response_positions.len(), 3);
    let mut swapped = pool.begin().await?;
    sqlx::query(
        "ALTER TABLE context_frontier_delta
         DISABLE TRIGGER context_frontier_member_is_append_only",
    )
    .execute(&mut *swapped)
    .await?;
    sqlx::query(
        "UPDATE context_frontier_delta
            SET member_position = member_position + 100
          WHERE owning_session_id = $1
            AND context_frontier_id = $2
            AND semantic_entry_id = $3",
    )
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 30))
    .bind(response_positions[0].0)
    .execute(&mut *swapped)
    .await?;
    sqlx::query(
        "UPDATE context_frontier_delta
            SET member_position = $1
          WHERE owning_session_id = $2
            AND context_frontier_id = $3
            AND semantic_entry_id = $4",
    )
    .bind(response_positions[0].1)
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 30))
    .bind(response_positions[1].0)
    .execute(&mut *swapped)
    .await?;
    sqlx::query(
        "UPDATE context_frontier_delta
            SET member_position = $1
          WHERE owning_session_id = $2
            AND context_frontier_id = $3
            AND semantic_entry_id = $4",
    )
    .bind(response_positions[1].1)
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 30))
    .bind(response_positions[0].0)
    .execute(&mut *swapped)
    .await?;
    let swapped_error = sqlx::query("SELECT assert_tool_round_final_state($1)")
        .bind(fixture.call.into_uuid())
        .execute(&mut *swapped)
        .await
        .expect_err("swapped text/tool parts must fail complete response-order validation");
    assert_eq!(
        swapped_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    swapped.rollback().await?;

    let closed_entry: Uuid = sqlx::query_scalar(
        "SELECT entry.semantic_entry_id
           FROM semantic_transcript_entry AS entry
           JOIN tool_request AS request
             ON request.request_id = entry.tool_result_request_id
          WHERE request.producing_model_call_id = $1
            AND entry.payload_kind = 'tool_closed_by_turn_end'
          ORDER BY request.request_ordinal
          LIMIT 1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    let mut omitted_closure = pool.begin().await?;
    sqlx::query(
        "ALTER TABLE context_frontier_delta
         DISABLE TRIGGER context_frontier_member_is_append_only",
    )
    .execute(&mut *omitted_closure)
    .await?;
    sqlx::query(
        "DELETE FROM context_frontier_delta
          WHERE owning_session_id = $1
            AND context_frontier_id = $2
            AND semantic_entry_id = $3",
    )
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 30))
    .bind(closed_entry)
    .execute(&mut *omitted_closure)
    .await?;
    let omitted_error = sqlx::query("SELECT assert_tool_round_final_state($1)")
        .bind(fixture.call.into_uuid())
        .execute(&mut *omitted_closure)
        .await
        .expect_err("terminal frontier must include every closed result");
    assert_eq!(
        omitted_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    omitted_closure.rollback().await?;

    pool.close().await;
    drop(container);
    Ok(())
}

/// S02 / S07 / S11 / INV-006 / INV-037: the terminal shape committed when a
/// stop request races a tool-using response reloads through the scheduling
/// projection, so the interrupt successor activates instead of leaving the
/// session permanently unloadable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s07_s11_inv006_inv037_stopped_tool_round_reloads_and_activates_successor()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7d00;
    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 21));
    let (fixture, model_repository, _prepared, authorized) =
        authorize_checkpointed_model_call_with_prepared(&pool, seed).await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 19,
                seed + 1,
                "stop tool response",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 20)),
            Some(successor),
        )
        .await?;

    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            ToolCallProposal::new(
                ToolName::try_new(String::from("first_tool")).expect("valid fixture tool name"),
                NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                    .expect("bounded fixture arguments"),
            ),
        )])
        .expect("the fixture contains one tool proposal");
    let outcome = model_repository
        .apply_terminal_observation(
            fixture.session,
            authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools {
                    response,
                }),
            ModelCallTerminalIdentities::StoppedToolRound(
                StoppedToolRoundModelCallIdentities::new(
                    vec![StoppedToolResponsePartIdentity::tool_call(
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 24)),
                        signalbox_domain::ToolRequestId::from_uuid(Uuid::from_u128(seed + 22)),
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 25)),
                        InitialToolApproval::Confirm,
                    )],
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 29)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 30)),
                ),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert!(matches!(
        outcome,
        ModelCallTerminalOutcome::CancelledWithToolResponse(_)
    ));

    let activation = StartEligibleTurnRepository::new(pool.clone())
        .handle(
            fixture.session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 34)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 31)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 32)),
                TurnAttemptId::from_uuid(Uuid::from_u128(seed + 33)),
            ),
        )
        .await?;
    assert_eq!(
        activated_turn(activation),
        successor,
        "the committed stopped tool round must reload as a terminal predecessor"
    );

    pool.close().await;
    drop(container);
    Ok(())
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

/// INV-014: the credential-reference column is total; the migrated schema
/// rejects a NULL stored reference.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv014_model_call_credential_reference_is_total() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let is_nullable: String = sqlx::query_scalar(
        "SELECT is_nullable
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND table_name = 'model_call'
            AND column_name = 'credential_reference'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(is_nullable, "NO");

    pool.close().await;
    drop(container);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_call_usage_transcript_lookup_is_session_indexed() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let index_definition: String = sqlx::query_scalar(
        "SELECT indexdef
           FROM pg_indexes
          WHERE schemaname = current_schema()
            AND indexname = 'model_call_usage_by_session_state_turn_call'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(index_definition.contains("(session_id, state_kind, turn_id, model_call_id)"));

    pool.close().await;
    drop(container);
    Ok(())
}

/// Provider token fields reject fractional SQL input instead of rounding it
/// into nearby evidence before constraint validation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_call_usage_rejects_fractional_evidence_without_rounding()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x6d00, true).await?;
    let fractional_input_tokens = Decimal::new(5, 1);

    let error = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                usage_input_tokens = $1
          WHERE model_call_id = $2",
    )
    .bind(fractional_input_tokens)
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("fractional provider usage must not be rounded");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("model_call_usage_input_tokens_u64")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_call_usage_provenance_rejects_unknown_values() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x6d40, true).await?;

    let error = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                usage_provenance_kind = 'inferred'
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("the usage provenance vocabulary is closed");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("model_call_usage_provenance_kind_closed")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_call_input_semantics_are_immutable() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x6d60, true).await?;

    let stored: bool = sqlx::query_scalar(
        "SELECT usage_input_includes_cache_tokens
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert!(!stored);
    let error = sqlx::query(
        "UPDATE model_call
            SET usage_input_includes_cache_tokens = true
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a prepared call's input semantics must be immutable");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("model_call_usage_metadata_immutable")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_call_input_semantics_keep_historical_unknown_and_new_default()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let (is_nullable, column_default): (String, Option<String>) = sqlx::query_as(
        "SELECT is_nullable, column_default
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND table_name = 'model_call'
            AND column_name = 'usage_input_includes_cache_tokens'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(is_nullable, "YES");
    assert_eq!(column_default.as_deref(), Some("false"));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-006: cancellation evidence cannot carry provider usage because neither
/// cancellation-confirmed nor pre-send cancellation reports token evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv006_cancelled_model_call_usage_is_unreported() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x6d80, true).await?;
    let reported_output_tokens = Decimal::from(1_u64);

    let error = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'cancelled',
                usage_output_tokens = $1
          WHERE model_call_id = $2",
    )
    .bind(reported_output_tokens)
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("cancelled calls cannot carry provider usage");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("model_call_cancelled_usage_is_unreported")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-006: a call terminalized directly from Prepared cannot carry usage because
/// no provider send was authorized.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv006_unsent_model_call_usage_is_unreported() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x6e00, false).await?;
    let reported_input_tokens = Decimal::from(1_u64);

    let error = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                usage_input_tokens = $1
          WHERE model_call_id = $2",
    )
    .bind(reported_input_tokens)
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("an unsent call cannot carry provider usage");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("model_call_unsent_usage_unreported")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-006: a call terminalized directly from Prepared cannot carry a
/// provider-failure cause because no provider send was authorized.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv006_unsent_model_call_provider_failure_cause_is_absent() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x6e80, false).await?;

    let error = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                terminal_provider_failure_cause = 'quota_exhausted'
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("an unsent call cannot carry a provider-failure cause");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("model_call_unsent_provider_failure_cause_absent")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-014: a reference pinned on a new model call cannot be replaced or
/// cleared.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv014_model_call_credential_reference_is_immutable() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x6f00, false).await?;

    let replacement = sqlx::query(
        "UPDATE model_call
            SET credential_reference = 'replacement-provider-reference'
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a pinned credential reference cannot be replaced");
    assert_eq!(
        replacement
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let clearing = sqlx::query(
        "UPDATE model_call
            SET credential_reference = NULL
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a pinned credential reference cannot be cleared");
    assert_eq!(
        clearing.as_database_error().and_then(|error| error.code()),
        Some("23514".into())
    );

    let stored: String = sqlx::query_scalar(
        "SELECT credential_reference
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored, model_credential_reference().as_str());

    pool.close().await;
    drop(container);

    Ok(())
}

/// INV-006: an uncertain capability-failure closure is reconciled from exact
/// durable Prepared or complete known-failure state, including its terminal
/// attempt and call provenance, before any resubmission.
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

/// INV-006 / INV-014 / INV-037: retained capability failure and ambiguous
/// authorization rereads accept an exact interrupt-caused cancellation of the
/// still-Prepared call as authoritative no-work, and reject an incomplete
/// cancellation closure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv006_inv014_inv037_failure_rereads_accept_prepared_cancellation()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7580;
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
    let PrepareInitialModelCallOutcome::Ready {
        request: prepared, ..
    } = repository
        .prepare_initial_call(
            fixture.session,
            ModelCallId::from_uuid(Uuid::from_u128(seed + 22)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 23)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 24)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 25)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 26)),
                    TurnId::from_uuid(Uuid::from_u128(seed + 27)),
                )
            },
        )
        .await?
    else {
        panic!("the fixture call must resume from its Prepared checkpoint")
    };

    SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 19,
                seed + 1,
                "cancel retained capability failure",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 20)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 21))),
        )
        .await?;
    assert_eq!(
        repository
            .reread_capability_failure(fixture.session, fixture.call)
            .await?,
        RetainedCapabilityFailureStatus::Cancelled
    );
    assert_eq!(
        repository
            .reread_ambiguous_authorization(fixture.session, &prepared)
            .await?,
        ModelCallAuthorizationReread::Cancelled
    );

    sqlx::query("ALTER TABLE turn_cancelled_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM turn_cancelled_outbox_event WHERE turn_id = $1")
        .bind(fixture.turn.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE turn_cancelled_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    assert!(matches!(
        repository
            .reread_capability_failure(fixture.session, fixture.call)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "retained capability failure cancellation closure is incomplete"
        ))
    ));
    assert!(matches!(
        repository
            .reread_ambiguous_authorization(fixture.session, &prepared)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "ambiguous authorization terminal cancellation closure is incomplete"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// docs/spec/model-call-execution.md: retained non-completed observations
/// converge only when their complete disposition-specific durable closure
/// remains present.
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
            ModelCallTerminalIdentities::PhysicalCancellation(
                PhysicalCancellationModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(cancelled_seed + 17)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(cancelled_seed + 18)),
                ),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert_eq!(
        cancelled_repository
            .reread_terminal_observation(cancelled.session, &cancelled_observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    let cancelled_failure_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(cancelled.session)
        .await?
        .expect("the failed-after-cancellation session has a transcript projection");
    let ProcessTurnState::Failed {
        terminal_attempt: Some(terminal_attempt),
        terminal_model_call: Some(terminal_call),
        ..
    } = cancelled_failure_snapshot.turns()[0].state()
    else {
        panic!("the failed projection must retain its cancelled call");
    };
    assert_eq!(*terminal_attempt, cancelled.attempt);
    assert_eq!(terminal_call.call(), cancelled.call);
    assert_eq!(
        terminal_call.disposition(),
        ProcessFailedModelCallDisposition::Cancelled
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
    let refused_sequence: Decimal = sqlx::query_scalar(
        "SELECT event_sequence
           FROM turn_refused_outbox_event
          WHERE turn_id = $1",
    )
    .bind(refused.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE turn_lifecycle
            DROP CONSTRAINT turn_lifecycle_terminal_attempt_fk,
            DROP CONSTRAINT turn_lifecycle_terminal_call_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET terminal_attempt_id = $1,
                terminal_model_call_id = $2
          WHERE turn_id = $3",
    )
    .bind(Uuid::from_u128(refused_seed + 19))
    .bind(Uuid::from_u128(refused_seed + 20))
    .bind(refused.turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE outbox_delivery_state
         DISABLE TRIGGER outbox_delivery_advances_prefix",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE outbox_delivery_state
            SET delivered_through = $1 - 1,
                last_delivery_xid = pg_current_xact_id()
          WHERE singleton",
    )
    .bind(refused_sequence)
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_delivery_state
         ENABLE TRIGGER outbox_delivery_advances_prefix",
    )
    .execute(&pool)
    .await?;
    assert!(matches!(
        OutboxDispatcher::new(pool.clone())
            .dispatch_next(|_| panic!("cross-wired refused ownership must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidTerminalEventCorrelation
        ))
    ));
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET terminal_attempt_id = $1,
                terminal_model_call_id = $2
          WHERE turn_id = $3",
    )
    .bind(refused.attempt.into_uuid())
    .bind(refused.call.into_uuid())
    .bind(refused.turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
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
            ModelCallTerminalIdentities::Ambiguous(
                signalbox_domain::AmbiguousModelCallTurnIdentities::new(
                    ContextFrontierId::from_uuid(Uuid::from_u128(ambiguous_seed + 20)),
                ),
            ),
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

/// S03 / S09 / INV-006 / INV-008 / INV-012 / INV-014 / INV-015: interrupting
/// an issued call atomically records its stop proof and cancellation request;
/// the durable signal resolves, physical cancellation closes the turn with its
/// exact attempt history, and both command and observation replays converge on
/// the recorded outcome.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn issued_interrupt_requests_and_confirms_durable_cancellation() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7600;
    let (fixture, model_repository, prepared, authorized) =
        authorize_checkpointed_model_call_with_prepared(&pool, seed).await?;
    let interrupt = input_with_delivery(
        seed + 19,
        seed + 1,
        "stop issued call",
        DeliveryRequest::Interrupt {
            expected_active_turn: fixture.turn,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let successor_input = AcceptedInputId::from_uuid(Uuid::from_u128(seed + 20));
    let successor_turn = TurnId::from_uuid(Uuid::from_u128(seed + 21));
    let interrupt_outcome = SubmitInputRepository::new(pool.clone())
        .handle(interrupt.clone(), successor_input, Some(successor_turn))
        .await?;
    assert!(matches!(
        &interrupt_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(applied)
        )) if applied.turn() == successor_turn
            && applied
                .applied_interrupt()
                .is_some_and(|interrupt| interrupt.proof().predecessor() == fixture.turn)
    ));

    let stopped_shape: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_attempt
              WHERE turn_attempt_id = $1
                AND state_kind = 'stop_requested'
                AND interrupt_command_id = $4
                AND interrupt_predecessor_turn_id = $2),
            (SELECT count(*)
               FROM model_call
              WHERE model_call_id = $3
                AND state_kind = 'cancellation_requested'),
            (SELECT count(*)
               FROM model_call_transition_outbox_event
              WHERE model_call_id = $3
                AND call_state_kind = 'cancellation_requested')",
    )
    .bind(fixture.attempt.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(fixture.call.into_uuid())
    .bind(Uuid::from_u128(seed + 19))
    .fetch_one(&pool)
    .await?;
    assert_eq!(stopped_shape, (1, 1, 1));
    let stopped_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(fixture.session)
        .await?
        .expect("the stopped session has a transcript projection");
    assert_running_current_model_call(
        stopped_snapshot.turns()[0].state(),
        fixture.attempt,
        fixture.call,
        ProcessCurrentModelCallState::CancellationRequested,
    );

    let ModelCallAuthorizationReread::CancellationRequested(stopped) = model_repository
        .reread_ambiguous_authorization(fixture.session, &prepared)
        .await?
    else {
        panic!("the authoritative reread must retain stopped non-consumption")
    };
    assert_eq!(
        stopped.observation_correlation(),
        authorized.observation_correlation()
    );

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        AuthorizeModelCallTransaction::cancellation_signal(
            &model_repository,
            fixture.session,
            fixture.call,
        ),
    )
    .await
    .expect("durable cancellation signal resolves after the stop commit");

    let observation = stopped
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Cancelled);
    let terminal = model_repository
        .apply_terminal_observation(
            fixture.session,
            observation.clone(),
            ModelCallTerminalIdentities::PhysicalCancellation(
                PhysicalCancellationModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 22)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 23)),
                ),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert!(matches!(
        terminal,
        ModelCallTerminalOutcome::Cancelled(ref cancelled)
            if cancelled.turn() == fixture.turn
                && cancelled.call().is_some_and(|call| call.id() == fixture.call)
    ));

    let terminal_shape: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE turn_id = $1
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'cancelled'),
            (SELECT count(*)
               FROM turn_attempt
              WHERE turn_attempt_id = $2
                AND state_kind = 'ended'
                AND end_variant = 'after_cancellation'
                AND end_disposition = 'cancelled'),
            (SELECT count(*)
               FROM semantic_transcript_entry
              WHERE cancelled_turn_id = $1
                AND payload_kind = 'turn_cancelled'),
            (SELECT count(*)
               FROM turn_cancelled_outbox_event
              WHERE turn_id = $1)",
    )
    .bind(fixture.turn.into_uuid())
    .bind(fixture.attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(terminal_shape, (1, 1, 1, 1));
    let cancelled_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(fixture.session)
        .await?
        .expect("the cancelled session has a transcript projection");
    assert_eq!(
        cancelled_snapshot.turns()[0].state(),
        &ProcessTurnState::Cancelled {
            terminal_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(seed + 23)),
            terminal_attempt: fixture.attempt,
            terminal_call: Some(fixture.call),
        }
    );
    assert!(matches!(
        cancelled_snapshot.entries().last(),
        Some(ProcessTranscriptEntry::TurnCancelled {
            entry,
            turn,
            ..
        }) if *entry == SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 22))
            && *turn == fixture.turn
    ));
    assert_eq!(
        model_repository
            .reread_terminal_observation(fixture.session, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    assert_eq!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                interrupt,
                AcceptedInputId::from_uuid(Uuid::from_u128(seed + 24)),
                Some(TurnId::from_uuid(Uuid::from_u128(seed + 25))),
            )
            .await?,
        interrupt_outcome
    );

    let mut cancellation_event = None;
    drain_outbox(&pool, |event| {
        if let DispatchedOutboxEventKind::TurnCancelled {
            turn,
            cancellation_entry,
            terminal_frontier,
        } = event.kind()
        {
            cancellation_event = Some((
                event.session(),
                *turn,
                *cancellation_entry,
                *terminal_frontier,
            ));
        }
    })
    .await?;
    assert_eq!(
        cancellation_event,
        Some((
            fixture.session,
            fixture.turn,
            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 22)),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 23)),
        ))
    );

    sqlx::query("ALTER TABLE turn_attempt DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, $4, 'ended', 'without_stop', 'known_failure')",
    )
    .bind(Uuid::from_u128(seed + 26))
    .bind(fixture.turn.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.attempt.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_attempt ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let cardinality_error = sqlx::query("SELECT assert_turn_lifecycle_final_state($1)")
        .bind(fixture.turn.into_uuid())
        .execute(&pool)
        .await
        .expect_err("a cancelled turn cannot hide an additional ended attempt");
    assert_eq!(
        cardinality_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    assert!(cardinality_error.as_database_error().is_some_and(|error| {
        error
            .message()
            .contains("lacks its exact single ended attempt history")
    }));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S04 / S07 / INV-025 / INV-029 / INV-032 / INV-037: ambiguity observed
/// before or after an applied interrupt terminalizes as exact proof-bearing
/// reconciliation, and retained observation and origin rereads recognize the
/// committed closure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stopped_ambiguity_commits_reconciliation_and_rereads_exactly() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7680;
    let (fixture, model_repository, authorized) =
        authorize_checkpointed_model_call(&pool, seed).await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 19,
                seed + 1,
                "stop before ambiguous result",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 20)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 21))),
        )
        .await?;

    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Ambiguous);
    let terminal = model_repository
        .apply_terminal_observation(
            fixture.session,
            observation.clone(),
            ModelCallTerminalIdentities::Ambiguous(
                signalbox_domain::AmbiguousModelCallTurnIdentities::new(
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22)),
                ),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert!(matches!(
        terminal,
        ModelCallTerminalOutcome::ReconciliationRequired(ref reconciliation)
            if reconciliation.turn() == fixture.turn
                && reconciliation.call().id() == fixture.call
                && matches!(
                    reconciliation.disposition(),
                    signalbox_domain::TurnDisposition::ReconciliationRequired { marker }
                        if marker.ambiguous_operations().contains(
                            signalbox_domain::IssuedOperationRef::ModelCall(fixture.call)
                        )
                )
    ));
    assert_eq!(
        model_repository
            .reread_terminal_observation(fixture.session, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    let stored: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
              FROM turn_attempt
              WHERE turn_attempt_id = $1
                AND end_variant = 'after_cancellation'
                AND end_disposition = 'ambiguous'),
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE turn_id = $2
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'reconciliation_required'),
            (SELECT count(*)
               FROM turn_reconciliation_required_outbox_event
              WHERE turn_id = $2
                AND model_call_id = $3)",
    )
    .bind(fixture.attempt.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored, (1, 1, 1));
    let reconciliation_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(fixture.session)
        .await?
        .expect("the reconciliation-required session has a transcript projection");
    assert_eq!(
        reconciliation_snapshot.turns()[0].state(),
        &ProcessTurnState::ReconciliationRequired {
            terminal_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22)),
            terminal_attempt: fixture.attempt,
            operation: ProcessReconciliationOperation::ModelCall(fixture.call),
        }
    );

    let mut reconciliation_event = None;
    drain_outbox(&pool, |event| {
        if let DispatchedOutboxEventKind::TurnReconciliationRequired {
            turn,
            operation: DispatchedReconciliationOperation::ModelCall(call),
            terminal_frontier,
        } = event.kind()
        {
            reconciliation_event = Some((event.session(), *turn, *call, *terminal_frontier));
        }
    })
    .await?;
    assert_eq!(
        reconciliation_event,
        Some((
            fixture.session,
            fixture.turn,
            fixture.call,
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22)),
        ))
    );

    let waiting_seed = seed + 0x20;
    let (waiting, waiting_repository, waiting_authorized) =
        authorize_checkpointed_model_call(&pool, waiting_seed).await?;
    let waiting_observation = waiting_authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Ambiguous);
    let waiting_outcome = waiting_repository
        .apply_terminal_observation(
            waiting.session,
            waiting_observation.clone(),
            ModelCallTerminalIdentities::Ambiguous(
                signalbox_domain::AmbiguousModelCallTurnIdentities::new(
                    ContextFrontierId::from_uuid(Uuid::from_u128(waiting_seed + 22)),
                ),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert!(matches!(
        waiting_outcome,
        ModelCallTerminalOutcome::AwaitingRecovery(ref ambiguous)
            if ambiguous.turn() == waiting.turn
                && ambiguous.call().id() == waiting.call
    ));
    let waiting_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(waiting.session)
        .await?
        .expect("the ambiguous call has a transcript projection");
    assert_eq!(
        waiting_snapshot.turns()[0].state(),
        &ProcessTurnState::ActiveAwaitingModelCallRecovery {
            ended_attempt: waiting.attempt,
            recovery_call: waiting.call,
        }
    );
    assert_eq!(waiting_snapshot.entries().len(), 1);
    let submit_repository = SubmitInputRepository::new(pool.clone());
    let waiting_steering_command = input_with_delivery(
        waiting_seed + 0x100,
        waiting_seed + 1,
        "steering retained through existing ambiguity wait",
        DeliveryRequest::NextSafePoint {
            expected_active_turn: waiting.turn,
        },
    );
    assert!(matches!(
        submit_repository
            .handle(
                waiting_steering_command,
                AcceptedInputId::from_uuid(Uuid::from_u128(waiting_seed + 0x101)),
                None,
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));
    let waiting_interrupt = submit_repository
        .handle(
            input_with_delivery(
                waiting_seed + 23,
                waiting_seed + 1,
                "interrupt existing ambiguity wait",
                DeliveryRequest::Interrupt {
                    expected_active_turn: waiting.turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(waiting_seed + 24)),
            Some(TurnId::from_uuid(Uuid::from_u128(waiting_seed + 25))),
        )
        .await?;
    assert!(matches!(
        waiting_interrupt,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    assert_eq!(
        waiting_repository
            .reread_terminal_observation(waiting.session, &waiting_observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    let waiting_stored: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_attempt
              WHERE turn_attempt_id = $1
                AND end_variant = 'without_stop'
                AND end_disposition = 'ambiguous'),
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE turn_id = $2
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'reconciliation_required'),
            (SELECT count(*)
               FROM turn_reconciliation_required_outbox_event
              WHERE turn_id = $2
                AND model_call_id = $3)",
    )
    .bind(waiting.attempt.into_uuid())
    .bind(waiting.turn.into_uuid())
    .bind(waiting.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(waiting_stored, (1, 1, 1));

    let activated_interrupt = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: waiting.session.into_uuid(),
            origin_entry: Uuid::from_u128(waiting_seed + 0x110),
            starting_frontier: Uuid::from_u128(waiting_seed + 0x111),
            initial_attempt: Uuid::from_u128(waiting_seed + 0x112),
        },
    )
    .await?;
    assert_eq!(
        activated_interrupt.turn(),
        TurnId::from_uuid(Uuid::from_u128(waiting_seed + 25))
    );
    let unavailable = PostgresModelCallRepository::new(
        pool.clone(),
        ModelTargetCatalog::try_from_definitions([]).expect("an empty target catalog is valid"),
        model_credential_reference(),
    );
    assert!(matches!(
        unavailable
            .prepare_initial_call(
                waiting.session,
                ModelCallId::from_uuid(Uuid::from_u128(waiting_seed + 0x113)),
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(waiting_seed + 0x114)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(waiting_seed + 0x115)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(waiting_seed + 0x116)),
                |_| panic!("the interrupt successor has no pending steering"),
            )
            .await?,
        PrepareInitialModelCallOutcome::TargetUnavailable(_)
    ));
    let activated_reclassified = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: waiting.session.into_uuid(),
            origin_entry: Uuid::from_u128(waiting_seed + 0x120),
            starting_frontier: Uuid::from_u128(waiting_seed + 0x121),
            initial_attempt: Uuid::from_u128(waiting_seed + 0x122),
        },
    )
    .await?;
    let descendant_command = input_with_delivery(
        waiting_seed + 0x123,
        waiting_seed + 1,
        "descendant of reconciliation-origin steering",
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: activated_reclassified.turn(),
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let descendant_outcome = submit_repository
        .handle(
            descendant_command.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(waiting_seed + 0x124)),
            Some(TurnId::from_uuid(Uuid::from_u128(waiting_seed + 0x125))),
        )
        .await?;
    assert!(matches!(
        &descendant_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    assert_eq!(
        submit_repository
            .handle(
                descendant_command,
                AcceptedInputId::from_uuid(Uuid::from_u128(waiting_seed + 0x126)),
                Some(TurnId::from_uuid(Uuid::from_u128(waiting_seed + 0x127))),
            )
            .await?,
        descendant_outcome
    );

    let failed_seed = seed + 0x40;
    let (failed, failed_repository, failed_authorized) =
        authorize_checkpointed_model_call(&pool, failed_seed).await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                failed_seed + 19,
                failed_seed + 1,
                "stop before known failure",
                DeliveryRequest::Interrupt {
                    expected_active_turn: failed.turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(failed_seed + 20)),
            Some(TurnId::from_uuid(Uuid::from_u128(failed_seed + 21))),
        )
        .await?;
    let failed_observation = failed_authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed);
    failed_repository
        .apply_terminal_observation(
            failed.session,
            failed_observation.clone(),
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(failed_seed + 22)),
                ContextFrontierId::from_uuid(Uuid::from_u128(failed_seed + 23)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert_eq!(
        failed_repository
            .reread_terminal_observation(failed.session, &failed_observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    let failed_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(failed.session)
        .await?
        .expect("the failed session has a transcript projection");
    let ProcessTurnState::Failed {
        terminal_frontier,
        terminal_attempt: Some(terminal_attempt),
        terminal_model_call: Some(terminal_call),
    } = failed_snapshot.turns()[0].state()
    else {
        panic!("the failed projection must retain its physical evidence");
    };
    assert_eq!(
        *terminal_frontier,
        ContextFrontierId::from_uuid(Uuid::from_u128(failed_seed + 23))
    );
    assert_eq!(*terminal_attempt, failed.attempt);
    assert_eq!(terminal_call.call(), failed.call);
    assert_eq!(
        terminal_call.disposition(),
        ProcessFailedModelCallDisposition::KnownFailed
    );

    let refused_seed = seed + 0x80;
    let (refused, refused_repository, refused_authorized) =
        authorize_checkpointed_model_call(&pool, refused_seed).await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                refused_seed + 19,
                refused_seed + 1,
                "stop before refusal",
                DeliveryRequest::Interrupt {
                    expected_active_turn: refused.turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(refused_seed + 20)),
            Some(TurnId::from_uuid(Uuid::from_u128(refused_seed + 21))),
        )
        .await?;
    let refused_observation = refused_authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Refused);
    refused_repository
        .apply_terminal_observation(
            refused.session,
            refused_observation.clone(),
            ModelCallTerminalIdentities::Refused(
                signalbox_domain::RefusedModelCallTurnIdentities::new(
                    ContextFrontierId::from_uuid(Uuid::from_u128(refused_seed + 22)),
                ),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert_eq!(
        refused_repository
            .reread_terminal_observation(refused.session, &refused_observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A definitive provider failure persists and projects only its closed cause
/// classification, independently of provider-authored native evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn provider_failure_cause_round_trips_through_persistence_and_process_read()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x76c0;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let observation = authorized
        .observation_correlation()
        .bind_provider_failure_observation_with_usage(
            ProviderModelCallFailureCause::QuotaExhausted,
            ProviderReportedTokenUsage::unreported(),
        );
    repository
        .apply_terminal_observation(
            fixture.session,
            observation.clone(),
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 17)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 18)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    let stored_cause: Option<String> = sqlx::query_scalar(
        "SELECT terminal_provider_failure_cause
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored_cause.as_deref(), Some("quota_exhausted"));
    assert_eq!(
        repository
            .reread_terminal_observation(fixture.session, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    let snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(fixture.session)
        .await?
        .expect("the failed session has a transcript projection");
    let ProcessTurnState::Failed {
        terminal_model_call: Some(terminal_call),
        ..
    } = snapshot.turns()[0].state()
    else {
        panic!("the failed projection retains its terminal call");
    };
    assert_eq!(
        terminal_call.provider_failure_cause(),
        Some(ProcessProviderModelCallFailureCause::QuotaExhausted)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S07 / S08 / INV-006 / INV-012 / INV-037: the stop-request migration keeps
/// each stopping rejection paired with its immutable delivery and admits only
/// a known-failed call as failed post-cancellation provenance.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stop_request_schema_keeps_delivery_and_failure_shapes_closed() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;

    let result_shape: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid)
           FROM pg_constraint
          WHERE conname = 'submit_input_command_result_shape'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(result_shape.contains(
        "((rejection_kind = 'safe_point_unavailable_while_stopping'::text) AND \
         (delivery_kind = 'next_safe_point'::text))"
    ));
    assert!(result_shape.contains(
        "((rejection_kind = 'interrupt_already_applied'::text) AND \
         (delivery_kind = 'interrupt'::text))"
    ));

    let failed_assertion: String = sqlx::query_scalar(
        "SELECT pg_get_functiondef(oid)
           FROM pg_proc
          WHERE proname = 'assert_failed_terminal_execution_final_state'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(failed_assertion.contains("terminal_disposition_kind = 'known_failed'"));
    assert!(
        !failed_assertion.contains("terminal_disposition_kind IN ('known_failed', 'cancelled')")
    );
    assert!(!failed_assertion.contains("end_disposition IN ('known_failure', 'lost')"));
    assert!(failed_assertion.contains("attempt_count <> 1"));

    let seed = 0x75c0;
    let (failed, failed_repository, failed_authorized) =
        authorize_checkpointed_model_call(&pool, seed).await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 19,
                seed + 1,
                "stop before cardinality check",
                DeliveryRequest::Interrupt {
                    expected_active_turn: failed.turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 20)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 21))),
        )
        .await?;
    let failed_observation = failed_authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed);
    failed_repository
        .apply_terminal_observation(
            failed.session,
            failed_observation,
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 22)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 23)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    sqlx::query("ALTER TABLE turn_attempt DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, $4, 'ended', 'without_stop', 'known_failure')",
    )
    .bind(Uuid::from_u128(seed + 24))
    .bind(failed.turn.into_uuid())
    .bind(failed.session.into_uuid())
    .bind(failed.attempt.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_attempt ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let cardinality_error = sqlx::query("SELECT assert_failed_terminal_execution_final_state($1)")
        .bind(failed.turn.into_uuid())
        .execute(&pool)
        .await
        .expect_err("a cancellation failure cannot hide an additional ended attempt");
    assert_eq!(
        cardinality_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    assert!(cardinality_error.as_database_error().is_some_and(|error| {
        error
            .message()
            .contains("post-cancellation failure lacks its exact single attempt")
    }));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S03 / S04 / S07 / INV-006 / INV-012 / INV-029: completion and restart can
/// win after a durable stop request without erasing the applied interrupt.
/// Terminal reload accepts the completion race, while restart retains an
/// ambiguous call in proof-bearing terminal reconciliation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn interrupt_completion_and_restart_races_retain_stop_history() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;

    let completed_seed = 0x7700;
    let (completed, completed_repository, completed_authorized) =
        authorize_checkpointed_model_call(&pool, completed_seed).await?;
    let completed_interrupt = input_with_delivery(
        completed_seed + 19,
        completed_seed + 1,
        "completion race interrupt",
        DeliveryRequest::Interrupt {
            expected_active_turn: completed.turn,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let completed_interrupt_outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            completed_interrupt.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(completed_seed + 20)),
            Some(TurnId::from_uuid(Uuid::from_u128(completed_seed + 21))),
        )
        .await?;
    assert!(matches!(
        completed_interrupt_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    let assistant = AssistantText::try_new(String::from("already completed"))
        .expect("fixture assistant text is admitted");
    let completed_observation = completed_authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Completed {
            assistant_text: vec![assistant],
        });
    let completed_outcome = completed_repository
        .apply_terminal_observation(
            completed.session,
            completed_observation.clone(),
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    completed_seed + 22,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(completed_seed + 23)),
                ContextFrontierId::from_uuid(Uuid::from_u128(completed_seed + 24)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert!(matches!(
        completed_outcome,
        ModelCallTerminalOutcome::Completed(ref outcome)
            if matches!(
                outcome.attempt().end(),
                signalbox_domain::AttemptEnd::AfterCancellation {
                    disposition:
                        signalbox_domain::CancellationStopDisposition::TurnCompleted,
                    ..
                }
            )
    ));
    assert_eq!(
        completed_repository
            .reread_terminal_observation(completed.session, &completed_observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    assert!(matches!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                start_input(
                    completed_seed + 25,
                    completed_seed + 1,
                    "work after completion race",
                    1,
                    ModelSelectionOverride::UseSessionDefault,
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(completed_seed + 26)),
                Some(TurnId::from_uuid(Uuid::from_u128(completed_seed + 27))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));

    let restart_seed = 0x7800;
    let (restarted, restarted_repository, _) =
        authorize_checkpointed_model_call(&pool, restart_seed).await?;
    let restart_interrupt = input_with_delivery(
        restart_seed + 19,
        restart_seed + 1,
        "restart race interrupt",
        DeliveryRequest::Interrupt {
            expected_active_turn: restarted.turn,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle(
            restart_interrupt,
            AcceptedInputId::from_uuid(Uuid::from_u128(restart_seed + 20)),
            Some(TurnId::from_uuid(Uuid::from_u128(restart_seed + 21))),
        )
        .await?;
    let restart_outcome = restarted_repository
        .recover_after_restart(
            restarted.session,
            restarted.call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(restart_seed + 22)),
                ContextFrontierId::from_uuid(Uuid::from_u128(restart_seed + 23)),
            ),
        )
        .await?;
    assert!(matches!(
        restart_outcome,
        ModelCallTerminalOutcome::ReconciliationRequired(ref reconciliation)
            if matches!(
                reconciliation.attempt().end(),
                signalbox_domain::AttemptEnd::AfterCancellation {
                    disposition: signalbox_domain::CancellationStopDisposition::Lost,
                    ..
                }
            )
    ));
    let restart_terminal_shape: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE turn_id = $1
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'reconciliation_required'),
            (SELECT count(*)
               FROM model_call
              WHERE model_call_id = $2
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'ambiguous'),
            (SELECT count(*)
               FROM turn_reconciliation_required_outbox_event
              WHERE turn_id = $1
                AND model_call_id = $2)",
    )
    .bind(restarted.turn.into_uuid())
    .bind(restarted.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(restart_terminal_shape, (1, 1, 1));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / S20 / S21 / INV-014 / INV-015 / INV-032 / INV-035: the production
/// persistence chain checkpoints Prepared with its credential and input-token
/// semantics pins, reloads them instead of changed deployment values,
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
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
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
        signalbox_application::InProcessToolDispatchGate::default(),
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
    let resolved_target = ResolvedProviderTarget::naming(provider_identity);
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        direct_selection,
        resolved_target,
    )])
    .expect("one immutable direct target forms a catalog");
    let pinned_credential_reference = model_credential_reference();
    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        targets.clone(),
        pinned_credential_reference.clone(),
    )
    .with_cache_inclusive_input_targets(HashSet::from([resolved_target]));
    let call = ModelCallId::from_uuid(Uuid::from_u128(0xce2));
    let PrepareInitialModelCallOutcome::Checkpointed(checkpointed_call) = repository
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde8)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xee8)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xfe8)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdf8)),
                    TurnId::from_uuid(Uuid::from_u128(0xdf9)),
                )
            },
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
        ..
    } = repository
        .prepare_initial_call(
            session,
            unused_call_candidate,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde9)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xee9)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xfe9)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdf9)),
                    TurnId::from_uuid(Uuid::from_u128(0xdfa)),
                )
            },
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
    let input_includes_cache_tokens: bool = sqlx::query_scalar(
        "SELECT usage_input_includes_cache_tokens
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert!(input_includes_cache_tokens);

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
                ContextFrontierId::from_uuid(Uuid::from_u128(0xfea)),
                |_| {
                    (
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdfa)),
                        TurnId::from_uuid(Uuid::from_u128(0xdfb)),
                    )
                },
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

    let completion_sequence: Decimal = sqlx::query_scalar(
        "SELECT event_sequence
           FROM turn_completed_outbox_event
          WHERE turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE turn_lifecycle
            DROP CONSTRAINT turn_lifecycle_terminal_attempt_fk,
            DROP CONSTRAINT turn_lifecycle_terminal_call_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET terminal_attempt_id = $1,
                terminal_model_call_id = $2
          WHERE turn_id = $3",
    )
    .bind(Uuid::from_u128(0xbad1))
    .bind(Uuid::from_u128(0xbad2))
    .bind(turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE outbox_delivery_state
         DISABLE TRIGGER outbox_delivery_advances_prefix",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE outbox_delivery_state
            SET delivered_through = $1 - 1,
                last_delivery_xid = pg_current_xact_id()
          WHERE singleton",
    )
    .bind(completion_sequence)
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_delivery_state
         ENABLE TRIGGER outbox_delivery_advances_prefix",
    )
    .execute(&pool)
    .await?;
    assert!(matches!(
        OutboxDispatcher::new(pool.clone())
            .dispatch_next(|_| panic!("cross-wired terminal ownership must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidTerminalEventCorrelation
        ))
    ));
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET terminal_attempt_id = $1,
                terminal_model_call_id = $2
          WHERE turn_id = $3",
    )
    .bind(attempt.into_uuid())
    .bind(call.into_uuid())
    .bind(turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

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

#[track_caller]
fn assert_projected_steering_entry(
    entry: &ProcessTranscriptEntry,
    expected_input: AcceptedInputId,
    expected_turn: TurnId,
    expected_content: &str,
) {
    assert!(matches!(
        entry,
        ProcessTranscriptEntry::User {
            accepted_input,
            turn,
            content,
            ..
        } if *accepted_input == expected_input
            && *turn == expected_turn
            && content == expected_content
    ));
}

/// S02 / S08 / INV-005 / INV-012 / INV-014 / INV-015 / INV-032 / INV-036: the scripted
/// application path consumes multiple steering inputs at preparation, renders
/// them immediately in the process projection and to the provider in acceptance
/// order, rejects noncontiguous stored snapshot ordinals before resume,
/// preserves the staged terminal commits, and replays each immutable
/// pending-steering receipt after consumption.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_inv014_inv015_application_service_completes_scripted_reply()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x18e1));
    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(0x1ce1));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    let steering_inputs = [
        AcceptedInputId::from_uuid(Uuid::from_u128(0x19e2)),
        AcceptedInputId::from_uuid(Uuid::from_u128(0x19e3)),
    ];
    let submit_repository = SubmitInputRepository::new(pool.clone());
    let first_steering_command = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0x14e3)),
        session,
        UserContent::try_text(String::from("first steering"))
            .expect("fixture steering content is admitted"),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: turn,
        },
    );
    let first_steering = submit_repository
        .handle(first_steering_command.clone(), steering_inputs[0], None)
        .await?;
    assert!(matches!(
        &first_steering,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));
    let second_steering_command = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0x14e4)),
        session,
        UserContent::try_text(String::from("second steering"))
            .expect("fixture steering content is admitted"),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: turn,
        },
    );
    let second_steering = submit_repository
        .handle(second_steering_command.clone(), steering_inputs[1], None)
        .await?;
    assert!(matches!(
        &second_steering,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
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
    let corrupt_snapshot_unused_call = ModelCallId::from_uuid(Uuid::from_u128(0x1ce4));
    let unused_call = ModelCallId::from_uuid(Uuid::from_u128(0x1ce3));
    let assistant_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de4));
    let completion_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de5));
    let steering_entries = [
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de6)),
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de7)),
    ];
    let steering_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0x1ee2));
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0x1ee4));
    let assistant_text = AssistantText::try_new(String::from("service assistant reply"))
        .expect("fixture assistant content is admitted");
    let mut reused_frontier_entries = [
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1df8)),
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1df9)),
    ]
    .into_iter();
    let mut reused_frontier_turns = [
        TurnId::from_uuid(Uuid::from_u128(0x1af8)),
        TurnId::from_uuid(Uuid::from_u128(0x1af9)),
    ]
    .into_iter();
    let collision = repository
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1dfa)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1ef8)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x1ee1)),
            |_| {
                (
                    reused_frontier_entries
                        .next()
                        .expect("one entry candidate per pending steering input"),
                    reused_frontier_turns
                        .next()
                        .expect("one turn candidate per pending steering input"),
                )
            },
        )
        .await
        .expect_err("a reused steering-frontier identity must be retryable");
    assert!(
        matches!(
            collision,
            ModelCallRepositoryError::IdentityCollision(
                ModelCallIdentityCollision::TerminalFrontier
            )
        ),
        "unexpected reused-frontier result: {collision:?}"
    );
    let collision = repository
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1df0)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1ef0)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x1ef1)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de1)),
                    TurnId::from_uuid(Uuid::from_u128(0x1af0)),
                )
            },
        )
        .await
        .expect_err("a steering identity already in the frontier must be retryable");
    assert!(matches!(
        collision,
        ModelCallRepositoryError::IdentityCollision(ModelCallIdentityCollision::SemanticEntry)
    ));
    let duplicate_candidate = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1df1));
    let collision = repository
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1df2)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1ef2)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x1ef3)),
            |_| {
                (
                    duplicate_candidate,
                    TurnId::from_uuid(Uuid::from_u128(0x1af1)),
                )
            },
        )
        .await
        .expect_err("duplicate generated steering identities must be retryable");
    assert!(matches!(
        collision,
        ModelCallRepositoryError::IdentityCollision(ModelCallIdentityCollision::SemanticEntry)
    ));
    let mut service = ModelCallExecutionService::new(
        FixedModelCallExecutionIds::new(
            [call, corrupt_snapshot_unused_call, unused_call],
            [
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de2)),
                steering_entries[0],
                steering_entries[1],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1daa)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de3)),
                assistant_entry,
                completion_entry,
            ],
            [
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1ee5)),
                steering_frontier,
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1eaa)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1eab)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1ee3)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1ee6)),
                terminal_frontier,
            ],
            [
                TurnId::from_uuid(Uuid::from_u128(0x1ae2)),
                TurnId::from_uuid(Uuid::from_u128(0x1ae3)),
            ],
            [signalbox_domain::ToolRequestId::from_uuid(Uuid::from_u128(
                0x1ce1,
            ))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0x1ae4))],
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
    let prepared_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(session)
        .await?
        .expect("the prepared call has a transcript projection");
    assert_eq!(prepared_snapshot.entries().len(), 3);
    assert_projected_steering_entry(
        &prepared_snapshot.entries()[1],
        steering_inputs[0],
        turn,
        "first steering",
    );
    assert_projected_steering_entry(
        &prepared_snapshot.entries()[2],
        steering_inputs[1],
        turn,
        "second steering",
    );
    sqlx::query("ALTER TABLE context_frontier_delta DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE context_frontier_delta
            SET member_position = 4
          WHERE owning_session_id = $1
            AND context_frontier_id = $2
            AND member_position = 3",
    )
    .bind(session.into_uuid())
    .bind(steering_frontier.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE context_frontier_delta ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let corrupt_snapshot = service
        .execute(session)
        .await
        .expect_err("the noncontiguous call snapshot must fail closed");
    assert!(
        matches!(
            corrupt_snapshot,
            ModelCallExecutionError::Prepare(ModelCallRepositoryError::Corruption(
                ModelCallCorruption::Scheduling(SubmitInputCorruption::Inconsistent(
                    "context frontier contiguous membership"
                ))
            ))
        ),
        "unexpected noncontiguous-snapshot result: {corrupt_snapshot:?}"
    );
    sqlx::query("ALTER TABLE context_frontier_delta DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE context_frontier_delta
            SET member_position = 3
          WHERE owning_session_id = $1
            AND context_frontier_id = $2
            AND member_position = 4",
    )
    .bind(session.into_uuid())
    .bind(steering_frontier.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE context_frontier_delta ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
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
    let (_, _, _, _, _, provider, _, _, _) = service.into_parts();
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(provider.interaction_count(), 1);
    let messages = provider
        .last_prepared_messages()
        .expect("the scripted provider observed the prepared messages");
    assert_eq!(messages.len(), 3);
    assert!(matches!(
        &messages[0],
        ModelConversationMessage::User {
            accepted_input: message_input,
            content,
            ..
        } if *message_input == accepted_input
            && content.text().as_str() == "service user request"
    ));
    assert!(matches!(
        &messages[1],
        ModelConversationMessage::User {
            accepted_input: message_input,
            content,
            ..
        } if *message_input == steering_inputs[0]
            && content.text().as_str() == "first steering"
    ));
    assert!(matches!(
        &messages[2],
        ModelConversationMessage::User {
            accepted_input: message_input,
            content,
            ..
        } if *message_input == steering_inputs[1]
            && content.text().as_str() == "second steering"
    ));

    let durable_terminal: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM model_call
              WHERE model_call_id = $1
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed'
                AND context_frontier_id = $4),
            (SELECT count(*) FROM turn_lifecycle
              WHERE turn_id = $2
                AND state_kind = 'terminal'
                AND terminal_frontier_id = $3),
            (SELECT count(*) FROM accepted_input
              WHERE accepted_input_id = ANY($5)
                AND disposition_kind = 'consumed_as_steering'
                AND consuming_model_call_id = $1),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE semantic_entry_id = ANY($6)
                AND payload_kind = 'steering_accepted_input'
                AND steering_source_turn_id = $2)",
    )
    .bind(call.into_uuid())
    .bind(turn.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .bind(steering_frontier.into_uuid())
    .bind(steering_inputs.map(AcceptedInputId::into_uuid).to_vec())
    .bind(
        steering_entries
            .map(SemanticTranscriptEntryId::into_uuid)
            .to_vec(),
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_terminal, (1, 1, 2, 2));
    let transcript = ProcessReadRepository::new(pool.clone())
        .read_transcript(session)
        .await?
        .expect("the completed session has a transcript projection");
    assert_projected_steering_entry(
        &transcript.entries()[1],
        steering_inputs[0],
        turn,
        "first steering",
    );
    assert_projected_steering_entry(
        &transcript.entries()[2],
        steering_inputs[1],
        turn,
        "second steering",
    );
    let successor_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x19e4));
    let successor_turn = TurnId::from_uuid(Uuid::from_u128(0x1ae4));
    let successor = submit_repository
        .handle(
            start_input(
                0x14e5,
                0x18e1,
                "request after consumed-steering restart",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            successor_input,
            Some(successor_turn),
        )
        .await?;
    assert!(matches!(
        successor,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    assert_eq!(
        submit_repository
            .handle(
                first_steering_command,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x19f2)),
                None,
            )
            .await?,
        first_steering
    );
    assert_eq!(
        submit_repository
            .handle(
                second_steering_command,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x19f3)),
                None,
            )
            .await?,
        second_steering
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S03 / S04 / S07 / INV-006 / INV-016 / INV-029 / INV-034: a restart-parked
/// ambiguous model call wedges the session — the scan classifies nothing, the
/// wait stays visible across a second restart, and ordinary input is refused —
/// and the user reconciliation decision then terminalizes the exact ambiguity
/// without inventing an outcome, releases the slot, and lets the session
/// activate the accepted successor.
///
/// This is one restart-and-recovery contract, so it stays one test
/// (testing-style rule 17): CONTRIBUTING's restart category conjoins the final
/// state, the absence of forbidden effects, and scan idempotency, and each step
/// below runs against the durable state the previous step committed. Every
/// assertion names the leg it guards (rule 20) so a failure identifies which
/// guarantee broke rather than only that the timeline broke.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_inv029_inv034_user_reconciliation_releases_a_restart_parked_ambiguous_turn()
-> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let parked = checkpoint_restart_model_call(&pool, 0xB100, true).await?;

    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let mut scan = StartupScanService::new(
        FixedStartupScanIds::new(
            [
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xB201)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xB203)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xB205)),
            ],
            [
                ContextFrontierId::from_uuid(Uuid::from_u128(0xB202)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xB204)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xB206)),
            ],
        ),
        PostgresStartupScanRepository::new(restarted_pool.clone()),
    );

    let first_restart = scan.execute().await?;
    assert_eq!(
        first_restart.recovered_turn_count(),
        0,
        "an unobserved issued call parks its turn instead of terminalizing it"
    );
    assert_eq!(
        first_restart.awaiting_recovery_decision_sessions(),
        &[parked.session],
        "the restart that parks the turn reports the wait it just created"
    );

    let parked_shape: (String, String, String, String, String, Uuid) = sqlx::query_as(
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
    .bind(parked.call.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        parked_shape,
        (
            "terminal".into(),
            "ambiguous".into(),
            "ended".into(),
            "lost".into(),
            "awaiting_model_call_recovery".into(),
            parked.call.into_uuid(),
        ),
        "the restart boundary leaves the exact durable ambiguity it observed"
    );
    assert_eq!(
        ProcessReadRepository::new(restarted_pool.clone())
            .model_call_recovery_precondition(parked.session)
            .await?,
        ProcessModelCallRecoveryPrecondition::Parked { turn: parked.turn },
        "the operator surface can see the wait it is expected to decide"
    );

    let wedged = SubmitInputRepository::new(restarted_pool.clone())
        .handle(
            start_input(
                0xB210,
                0xB101,
                "work refused while the ambiguity is unreconciled",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xB211)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xB212))),
        )
        .await?;
    assert_eq!(
        wedged,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::ActiveTurnPresent {
                session: parked.session,
                active_turn: parked.turn,
            }
        )),
        "the slot is never released without a user decision"
    );

    let second_restart = scan.execute().await?;
    assert_eq!(
        second_restart.recovered_turn_count(),
        0,
        "a re-run scan reclassifies nothing it already parked"
    );
    assert_eq!(
        second_restart.awaiting_recovery_decision_sessions(),
        &[parked.session],
        "the wait stays reported until a decision resolves it"
    );

    let successor = TurnId::from_uuid(Uuid::from_u128(0xB222));
    let reconciled = SubmitInputRepository::new(restarted_pool.clone())
        .handle(
            input_with_delivery(
                0xB220,
                0xB101,
                "continue after the user reconciliation decision",
                DeliveryRequest::Interrupt {
                    expected_active_turn: parked.turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xB221)),
            Some(successor),
        )
        .await?;
    assert!(
        matches!(
            reconciled,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::TurnOrigin(_)
            ))
        ),
        "the user decision is accepted as the successor origin"
    );

    let reconciled_shape: (String, String, Uuid, Uuid, i64) = sqlx::query_as(
        "SELECT turn.state_kind,
                turn.terminal_disposition_kind,
                turn.terminal_attempt_id,
                turn.terminal_model_call_id,
                (SELECT count(*)
                   FROM turn_reconciliation_required_outbox_event
                  WHERE turn_id = $1
                    AND model_call_id = $2)
           FROM turn_lifecycle AS turn
          WHERE turn.turn_id = $1",
    )
    .bind(parked.turn.into_uuid())
    .bind(parked.call.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        reconciled_shape,
        (
            "terminal".into(),
            "reconciliation_required".into(),
            parked.attempt.into_uuid(),
            parked.call.into_uuid(),
            1,
        ),
        "reconciliation records the exact durable ambiguity instead of a fabricated outcome"
    );
    let ambiguous_call_unchanged: (String, String) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(parked.call.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        ambiguous_call_unchanged,
        ("terminal".into(), "ambiguous".into()),
        "reconciliation never rewrites the ambiguous call it reports"
    );
    assert_eq!(
        ProcessReadRepository::new(restarted_pool.clone())
            .model_call_recovery_precondition(parked.session)
            .await?,
        ProcessModelCallRecoveryPrecondition::NoParkedTurn,
        "the decided wait no longer offers itself to the operator surface"
    );

    let healed_restart = scan.execute().await?;
    assert_eq!(
        healed_restart.recovered_turn_count(),
        0,
        "a re-run scan after the decision changes nothing"
    );
    assert_eq!(
        healed_restart.awaiting_recovery_decision_sessions(),
        &[] as &[SessionId],
        "a decided session is no longer reported as awaiting one"
    );

    let snapshot = ProcessReadRepository::new(restarted_pool.clone())
        .read_transcript(parked.session)
        .await?
        .expect("the reconciled session remains process-readable");
    let ProcessTurnState::ReconciliationRequired {
        terminal_attempt,
        operation,
        ..
    } = snapshot.turns()[0].state()
    else {
        panic!("the reconciled turn stays readable as reconciliation-required");
    };
    assert_eq!(
        *terminal_attempt, parked.attempt,
        "the readable turn retains its exact terminal attempt"
    );
    assert_eq!(
        *operation,
        ProcessReconciliationOperation::ModelCall(parked.call),
        "the readable turn retains its exact ambiguous call"
    );

    let activated = activate_earliest_queued_turn(
        &restarted_pool,
        EarliestQueuedTurnActivation {
            session: parked.session.into_uuid(),
            origin_entry: Uuid::from_u128(0xB230),
            starting_frontier: Uuid::from_u128(0xB231),
            initial_attempt: Uuid::from_u128(0xB232),
        },
    )
    .await?;
    assert_eq!(
        activated.turn(),
        successor,
        "the session activates the successor accepted by the reconciliation decision"
    );

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S03 / S04 / S08 / INV-006 / INV-014 / INV-016 / INV-034: the production
/// startup repository applies call-aware recovery under its session lock:
/// Prepared is known-failed with exact terminal execution provenance while
/// reclassifying newly observed steering, an issued call becomes an exact
/// ambiguity wait, a stopped call terminalizes as reconciliation while
/// reclassifying its steering, that successor remains a valid replay origin,
/// and replay changes neither.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s03_s04_inv006_inv014_inv034_startup_scan_classifies_prepared_and_issued_model_calls()
-> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let prepared = checkpoint_restart_model_call(&pool, 0x2000, false).await?;
    let issued = checkpoint_restart_model_call(&pool, 0x3000, true).await?;
    let stopped = checkpoint_restart_model_call(&pool, 0x3500, true).await?;
    let prepared_steering = AcceptedInputId::from_uuid(Uuid::from_u128(0x6100));
    let issued_steering = AcceptedInputId::from_uuid(Uuid::from_u128(0x6101));
    let stopped_steering = AcceptedInputId::from_uuid(Uuid::from_u128(0x6102));
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
                    DurableCommandId::from_uuid(Uuid::from_u128(0x4300)),
                    stopped.session,
                    UserContent::try_text(String::from("steering accepted before stopped restart"))
                        .expect("fixture steering content is admitted"),
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn: stopped.turn,
                    },
                ),
                stopped_steering,
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
                input_with_delivery(
                    0x4301,
                    0x3501,
                    "stop before restart",
                    DeliveryRequest::Interrupt {
                        expected_active_turn: stopped.turn,
                        configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault,),
                    },
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x6103)),
                Some(TurnId::from_uuid(Uuid::from_u128(0x6203))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
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
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x4005)),
            ],
            [
                ContextFrontierId::from_uuid(Uuid::from_u128(0x5001)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x5002)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x5003)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x5004)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x5005)),
            ],
        )
        .with_reclassified_turns([
            prepared.turn,
            TurnId::from_uuid(Uuid::from_u128(0x6201)),
            TurnId::from_uuid(Uuid::from_u128(0x6202)),
        ]),
        PostgresStartupScanRepository::new(restarted_pool.clone()),
    );

    let first = scan.execute().await?;
    assert_eq!(first.recovered_turn_count(), 2);

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
    let stopped_state: (String, String, String, String, String) = sqlx::query_as(
        "SELECT call.state_kind,
                call.terminal_disposition_kind,
                attempt.end_variant,
                attempt.end_disposition,
                turn.terminal_disposition_kind
           FROM model_call AS call
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = call.turn_attempt_id
           JOIN turn_lifecycle AS turn
             ON turn.turn_id = call.turn_id
          WHERE call.model_call_id = $1",
    )
    .bind(stopped.call.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        stopped_state,
        (
            "terminal".into(),
            "ambiguous".into(),
            "after_cancellation".into(),
            "lost".into(),
            "reconciliation_required".into(),
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
    let stopped_steering_state: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT disposition_kind, origin_turn_id
           FROM accepted_input
          WHERE accepted_input_id = $1",
    )
    .bind(stopped_steering.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        stopped_steering_state,
        (
            "reclassified_as_turn_origin".into(),
            Some(Uuid::from_u128(0x6202)),
        )
    );
    assert!(matches!(
        SubmitInputRepository::new(restarted_pool.clone())
            .handle(
                start_input(
                    0x4302,
                    0x3501,
                    "work after reconciled restart",
                    1,
                    ModelSelectionOverride::UseSessionDefault,
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x6104)),
                Some(TurnId::from_uuid(Uuid::from_u128(0x6204))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));

    let mut stale_recovery_ids = FixedStartupScanIds::new([], []);
    assert_eq!(
        PostgresStartupScanRepository::new(restarted_pool.clone())
            .recover(
                prepared.session,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x6301)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(0x6302)),
                ),
                &mut stale_recovery_ids,
            )
            .await?,
        StartupScanSessionOutcome::NoActiveTurn
    );

    let replay = scan.execute().await?;
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

    let activated_interrupt = activate_earliest_queued_turn(
        &restarted_pool,
        EarliestQueuedTurnActivation {
            session: stopped.session.into_uuid(),
            origin_entry: Uuid::from_u128(0x6400),
            starting_frontier: Uuid::from_u128(0x6401),
            initial_attempt: Uuid::from_u128(0x6402),
        },
    )
    .await?;
    assert_eq!(
        activated_interrupt.turn(),
        TurnId::from_uuid(Uuid::from_u128(0x6203))
    );
    let empty_targets =
        ModelTargetCatalog::try_from_definitions([]).expect("an empty target catalog is valid");
    let target_miss = PostgresModelCallRepository::new(
        restarted_pool.clone(),
        empty_targets,
        model_credential_reference(),
    );
    let PrepareInitialModelCallOutcome::TargetUnavailable(failed_interrupt) = target_miss
        .prepare_initial_call(
            stopped.session,
            ModelCallId::from_uuid(Uuid::from_u128(0x6403)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x6404)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x6405)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x6406)),
            |_| panic!("the interrupt successor has no pending steering"),
        )
        .await?
    else {
        panic!("the unavailable target must release the interrupt successor");
    };
    assert_eq!(
        failed_interrupt.turn(),
        TurnId::from_uuid(Uuid::from_u128(0x6203))
    );

    let activated_reclassified = activate_earliest_queued_turn(
        &restarted_pool,
        EarliestQueuedTurnActivation {
            session: stopped.session.into_uuid(),
            origin_entry: Uuid::from_u128(0x6410),
            starting_frontier: Uuid::from_u128(0x6411),
            initial_attempt: Uuid::from_u128(0x6412),
        },
    )
    .await?;
    let reclassified_turn = TurnId::from_uuid(Uuid::from_u128(0x6202));
    assert_eq!(activated_reclassified.turn(), reclassified_turn);

    let descendant_command = DurableCommandId::from_uuid(Uuid::from_u128(0x4303));
    let descendant = SubmitInputRepository::new(restarted_pool.clone())
        .handle(
            input_with_delivery(
                0x4303,
                0x3501,
                "work after reconciliation-origin steering",
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: reclassified_turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x6105)),
            Some(TurnId::from_uuid(Uuid::from_u128(0x6205))),
        )
        .await?;
    let SubmitInputHandlingOutcome::Recorded(descendant_result) = &descendant else {
        panic!("the descendant command was newly recorded");
    };
    assert!(matches!(
        descendant_result,
        SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(_))
    ));
    assert_eq!(
        SubmitInputRepository::new(restarted_pool.clone())
            .load(descendant_command)
            .await?
            .expect("the descendant command must replay")
            .result(),
        descendant_result
    );

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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
                ContextFrontierId::from_uuid(Uuid::from_u128(0xff4)),
                |_| {
                    (
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcf4)),
                        TurnId::from_uuid(Uuid::from_u128(0xcf5)),
                    )
                },
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
            signalbox_application::ModelCallTerminalIdentityCandidates::Exact(
                ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                    vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde5))],
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde6)),
                    terminal_frontier,
                )),
            ),
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

    let durable: (String, Uuid, Uuid, String, i64, i64) = sqlx::query_as(
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
                    AND queued.model_fallback IS NULL),
                (SELECT count(*)
                   FROM input_accepted_outbox_event AS event
                  WHERE event.accepted_input_id = $1
                    AND event.session_id = $2
                    AND event.turn_id = $3
                    AND event.acceptance_position = accepted.acceptance_position)
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

/// S08 / S21 / INV-006 / INV-014 / INV-032 / INV-036: immutable target
/// resolution failure creates no targetless call, reclassifies the complete
/// pending steering prefix, and atomically closes the prepared attempt and turn
/// with its semantic failure boundary and typed outbox event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s08_s21_inv006_inv014_inv032_inv036_target_unavailable_reclassifies_steering()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8f1));
    let direct_selection =
        signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(0xcf1));
    let mut create_service = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
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
        signalbox_application::InProcessToolDispatchGate::default(),
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

    let pending_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9f2));
    let reclassified_turn = TurnId::from_uuid(Uuid::from_u128(0xaf2));
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::PendingSteering(_),
    )) = SubmitInputRepository::new(pool.clone())
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(0x4f3)),
                session,
                UserContent::try_text("steering before target miss".to_owned())
                    .expect("fixture steering is admitted"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: turn,
                },
            ),
            pending_input,
            None,
        )
        .await?
    else {
        panic!("the target-miss fixture steering must remain pending");
    };

    let targets = ModelTargetCatalog::try_from_definitions([])
        .expect("an empty immutable target catalog is valid");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let call_candidate = ModelCallId::from_uuid(Uuid::from_u128(0xcf2));
    let failure_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdf2));
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xef2));
    let collision = repository
        .prepare_initial_call(
            session,
            call_candidate,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdf3)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xef3)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xff1)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcf4)),
                    turn,
                )
            },
        )
        .await
        .expect_err("a source-turn fallback candidate must be retryable");
    assert!(matches!(
        collision,
        ModelCallRepositoryError::IdentityCollision(ModelCallIdentityCollision::ReclassifiedTurn)
    ));
    let PrepareInitialModelCallOutcome::TargetUnavailable(failed) = repository
        .prepare_initial_call(
            session,
            call_candidate,
            FailedModelCallTurnIdentities::new(failure_entry, terminal_frontier),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xff2)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcf3)),
                    reclassified_turn,
                )
            },
        )
        .await?
    else {
        panic!("the unavailable configured target must close without a call");
    };
    assert_eq!(failed.turn(), turn);
    assert!(failed.call().is_none());

    let reclassification_shape: (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM accepted_input
              WHERE accepted_input_id = $1
                AND disposition_kind = 'reclassified_as_turn_origin'
                AND origin_turn_id = $2),
            (SELECT count(*) FROM queued_input_origin
              WHERE accepted_input_id = $1
                AND turn_id = $2
                AND source_configuration_turn_id = $3)",
    )
    .bind(pending_input.into_uuid())
    .bind(reclassified_turn.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(reclassification_shape, (1, 1));

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
    let repository = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let mut service = CreateSessionService::new(
        FixedSessionIds::new([winner, replay_candidate, conflicting_candidate]),
        repository,
    );

    let first = service.execute(request.clone()).await?;
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

/// INV-047: template creation durably copies the original bundle and its
/// provenance; a same-command replay after a catalog edit returns that winner.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv047_template_creation_persists_copy_and_name_keyed_replay() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(0x1601));
    let winner = SessionId::from_uuid(Uuid::from_u128(0x1701));
    let replay_candidate = SessionId::from_uuid(Uuid::from_u128(0x1702));
    let name = SessionTemplateName::try_new("reviewer".to_owned())?;
    let original_provenance = SessionTemplateProvenance::new(
        name.clone(),
        SessionTemplateContentDigest::from_bytes([0x21; 32]),
    );
    let edited_provenance = SessionTemplateProvenance::new(
        name.clone(),
        SessionTemplateContentDigest::from_bytes([0x22; 32]),
    );
    let original_defaults = SessionConfigurationDefaults::complete(
        direct(0x1801),
        signalbox_domain::DangerousToolAutoApproval::ApproveAll,
        Some(SessionSystemPrompt::try_new(
            "original reviewer prompt".to_owned(),
        )?),
    );
    let edited_defaults = SessionConfigurationDefaults::complete(
        alias(0x1802),
        signalbox_domain::DangerousToolAutoApproval::Disabled,
        Some(SessionSystemPrompt::try_new(
            "edited reviewer prompt".to_owned(),
        )?),
    );
    let original_request = CreateSessionRequest::try_new_from_template(
        command_id,
        original_provenance.clone(),
        original_defaults.clone(),
    )?;
    let replay_after_edit = CreateSessionRequest::try_new_from_template(
        command_id,
        edited_provenance,
        edited_defaults,
    )?;
    let mut service = CreateSessionService::new(
        FixedSessionIds::new([winner, replay_candidate]),
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
    );

    let first = service.execute(original_request).await?;
    let replay = service.execute(replay_after_edit).await?;

    assert_eq!(first, replay);
    let CreateSessionOutcome::Applied(replay_receipt) = replay else {
        panic!("same-name replay must return the recorded receipt");
    };
    assert_eq!(replay_receipt.session(), winner);
    assert_ne!(replay_receipt.session(), replay_candidate);

    let stored = sqlx::query(
        "SELECT s.template_name AS session_template_name,
                s.template_content_digest AS session_template_content_digest,
                c.template_name AS command_template_name,
                c.template_content_digest AS command_template_content_digest,
                d.storage_version AS registry_storage_version,
                c.storage_version AS command_storage_version
         FROM session AS s
         JOIN create_session_command AS c
           ON c.created_session_id = s.session_id
         JOIN durable_command AS d USING (command_id)
         WHERE s.session_id = $1",
    )
    .bind(winner.into_uuid())
    .fetch_one(&pool)
    .await?;
    let session_template_name: String = stored.try_get("session_template_name")?;
    let session_template_content_digest: Vec<u8> =
        stored.try_get("session_template_content_digest")?;
    let command_template_name: String = stored.try_get("command_template_name")?;
    let command_template_content_digest: Vec<u8> =
        stored.try_get("command_template_content_digest")?;
    let registry_storage_version: i16 = stored.try_get("registry_storage_version")?;
    let command_storage_version: i16 = stored.try_get("command_storage_version")?;
    assert_eq!(session_template_name, name.as_str());
    assert_eq!(
        session_template_content_digest,
        original_provenance.content_digest().as_bytes()
    );
    assert_eq!(command_template_name, name.as_str());
    assert_eq!(
        command_template_content_digest,
        original_provenance.content_digest().as_bytes()
    );
    assert_eq!(registry_storage_version, 4);
    assert_eq!(command_storage_version, 4);

    let loaded = LoadSessionService::new(SessionRepository::new(pool.clone()))
        .execute(winner)
        .await?
        .expect("template-created session remains loadable");
    assert_eq!(loaded.template_provenance(), Some(&original_provenance));
    assert_eq!(
        loaded.current_configuration_defaults().defaults(),
        &original_defaults
    );

    sqlx::query(
        "DROP TRIGGER create_session_command_is_append_only
         ON create_session_command",
    )
    .execute(&pool)
    .await?;
    let mut disagreement = pool.begin().await?;
    sqlx::query(
        "UPDATE create_session_command
         SET template_name = NULL,
             template_content_digest = NULL
         WHERE created_session_id = $1",
    )
    .bind(winner.into_uuid())
    .execute(&mut *disagreement)
    .await?;
    let disagreement = disagreement
        .commit()
        .await
        .expect_err("session provenance must bind back to its creation command");
    let disagreement_error = disagreement
        .as_database_error()
        .expect("provenance disagreement must be a database error");
    assert_eq!(disagreement_error.code().as_deref(), Some("23503"));
    assert_eq!(
        disagreement_error.constraint(),
        Some("session_template_provenance_creation_fk")
    );

    sqlx::query(
        "ALTER TABLE create_session_command
         DROP CONSTRAINT create_session_command_initial_defaults_fk",
    )
    .execute(&pool)
    .await?;
    let promptless_schema = sqlx::query(
        "UPDATE create_session_command
         SET system_prompt = NULL
         WHERE command_id = $1",
    )
    .bind(command_id.into_uuid())
    .execute(&pool)
    .await
    .expect_err("template provenance requires a command prompt");
    let promptless_schema_error = promptless_schema
        .as_database_error()
        .expect("promptless template rejection must be a database error");
    assert_eq!(promptless_schema_error.code().as_deref(), Some("23514"));
    assert_eq!(
        promptless_schema_error.constraint(),
        Some("create_session_command_template_prompt_required")
    );

    sqlx::query(
        "ALTER TABLE create_session_command
         DROP CONSTRAINT create_session_command_template_prompt_required",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE create_session_command
         SET system_prompt = NULL
         WHERE command_id = $1",
    )
    .bind(command_id.into_uuid())
    .execute(&pool)
    .await?;
    let promptless_reader =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
            .load(command_id)
            .await
            .expect_err("promptless template provenance must fail closed");
    assert!(matches!(
        promptless_reader,
        CreateSessionRepositoryError::Corruption(CreateSessionCorruption::Inconsistent(
            "template creation without system prompt"
        ))
    ));

    sqlx::query("DROP TRIGGER durable_command_is_append_only ON durable_command")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE create_session_command
         DROP CONSTRAINT create_session_command_template_provenance_versioned,
         DROP CONSTRAINT create_session_command_registry_fk",
    )
    .execute(&pool)
    .await?;
    let mut pre_version_four = pool.begin().await?;
    sqlx::query(
        "UPDATE durable_command
         SET storage_version = 3
         WHERE command_id = $1",
    )
    .bind(command_id.into_uuid())
    .execute(&mut *pre_version_four)
    .await?;
    sqlx::query(
        "UPDATE create_session_command
         SET storage_version = 3
         WHERE command_id = $1",
    )
    .bind(command_id.into_uuid())
    .execute(&mut *pre_version_four)
    .await?;
    pre_version_four.commit().await?;
    let pre_version_four =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
            .load(command_id)
            .await
            .expect_err("pre-version-four template provenance must fail closed");
    assert!(matches!(
        pre_version_four,
        CreateSessionRepositoryError::Corruption(CreateSessionCorruption::Inconsistent(
            "pre-version-four template provenance"
        ))
    ));
    let pre_version_four_session = SessionRepository::new(pool.clone())
        .load_session(winner)
        .await
        .expect_err("session loading must reject pre-version-four template provenance");
    assert!(matches!(
        pre_version_four_session,
        SessionRepositoryError::Corruption(SessionCorruption::Inconsistent(
            "pre-version-four template provenance"
        ))
    ));

    pool.close().await;
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
    .expect_err("the user-global command ID must be unique");
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
    let repository = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let first = prepared(0x101, 0x701, direct(0x801));

    assert_eq!(
        repository.handle(first.clone()).await?,
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
        repository.handle(separate.clone()).await?,
        CreateSessionHandlingOutcome::Applied(separate.applied_result())
    );
    assert_eq!(
        repository.handle(alias_creation.clone()).await?,
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
    let restarted =
        CreateSessionRepository::new(restarted_pool.clone(), test_session_credential_pin());
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

/// S01 / INV-012: the user-global primary key is the concurrency boundary.
/// Equal duplicates return one winner; unequal duplicates retain that winner
/// and report one typed conflict.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv012_concurrent_duplicates_converge_on_the_committed_winner()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());

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
    let repository = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let existing = prepared(0x121, 0x721, direct(0x821));
    repository.handle(existing).await?;

    let colliding = prepared(0x122, 0x721, direct(0x822));
    let error = repository
        .handle(colliding.clone())
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
        repository.handle(retry.clone()).await?,
        CreateSessionHandlingOutcome::Applied(retry.applied_result())
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-012: an observed user-global claim is never treated as unseen merely
/// because its typed record is missing or its storage version is unknown.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv012_incomplete_or_unknown_claims_fail_closed_as_corruption()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let defaults_repository = ReplaceSessionDefaultsRepository::new(pool.clone());
    let input_repository = SubmitInputRepository::new(pool.clone());
    let cross_wired = replacement(0x135, 0x735, 1, direct(0x835));
    defaults_repository.handle(cross_wired.clone()).await?;

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

    let repository = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
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
/// complete typed record, while the user-global registry and append-only
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
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let mut defaults_service =
        ReplaceSessionDefaultsService::new(ReplaceSessionDefaultsRepository::new(pool.clone()));
    let load_service = LoadSessionService::new(SessionRepository::new(pool.clone()));
    let creation = prepared(0x211, 0x711, direct(0x811));
    create_repository.handle(creation.clone()).await?;

    let first = replacement_request(0x212, 0x711, 1, alias(0x812));
    let first_outcome = defaults_service.execute(first.clone()).await?;
    let ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Applied(
        first_applied,
    )) = &first_outcome
    else {
        panic!("the first replacement must apply");
    };
    assert_eq!(
        first_applied.installed().version(),
        SessionConfigurationDefaultsVersion::try_from_u64(2).expect("positive version")
    );
    assert_eq!(
        defaults_service.execute(first.clone()).await?,
        first_outcome
    );

    let conflict = replacement_request(0x212, 0x711, 1, direct(0x813));
    assert_eq!(
        defaults_service.execute(conflict).await?,
        ReplaceSessionDefaultsOutcome::ConflictingReuse {
            command_id: first.command_id()
        }
    );

    let stale = replacement_request(0x213, 0x711, 1, direct(0x814));
    let stale_outcome = defaults_service.execute(stale.clone()).await?;
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

/// S33 / INV-008 / INV-015 / INV-046: replacing defaults while a turn is
/// active leaves that turn bound to its accepted epoch, while the next origin
/// freezes the successor and starts behind an injected model-identity entry.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s33_inv008_inv015_inv046_mid_session_model_switch_is_forward_only()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x731));
    let first_selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x831));
    let second_selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x832));
    let first_target = ProviderModelIdentity::from_uuid(Uuid::from_u128(0x833));
    let second_target = ProviderModelIdentity::from_uuid(Uuid::from_u128(0x834));
    let first_credential = ModelCallCredentialReference::new("fixture-provider-first");
    let second_credential = ModelCallCredentialReference::new("fixture-provider-second");
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            0x231,
            0x731,
            ModelSelectionRequest::Direct(first_selection),
        ))
        .await?;

    let first_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x931));
    let first_turn = TurnId::from_uuid(Uuid::from_u128(0xa31));
    let mut first_submit = SubmitInputService::new(
        FixedSubmitInputIds::new([first_input], [first_turn]),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    let first_content = "before model replacement";
    first_submit
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x232)),
            session,
            UserContent::try_text(first_content.to_owned())
                .expect("fixture user content is admitted"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        )?)
        .await?;
    let false_boundary_requirement = sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_accepted_input_id,
             acceptance_position, state_kind,
             model_identity_boundary_required)
         SELECT turn_id, session_id, origin_accepted_input_id,
                acceptance_position, state_kind, false
           FROM turn_lifecycle
          WHERE turn_id = $1",
    )
    .bind(first_turn.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a newly queued turn cannot claim pre-boundary compatibility");
    let false_boundary_database_error = false_boundary_requirement
        .as_database_error()
        .expect("the compatibility-shape check returns a database error");
    assert_eq!(false_boundary_database_error.code(), Some("23514".into()));
    assert_eq!(
        false_boundary_database_error.constraint(),
        Some("turn_lifecycle_model_identity_boundary_requirement_state")
    );
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(0xd31),
            starting_frontier: Uuid::from_u128(0xe31),
            initial_attempt: Uuid::from_u128(0xb31),
        },
    )
    .await?;

    let mut defaults_service =
        ReplaceSessionDefaultsService::new(ReplaceSessionDefaultsRepository::new(pool.clone()));
    defaults_service
        .execute(ReplaceSessionDefaultsRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x233)),
            session,
            SessionConfigurationDefaultsVersion::first(),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(second_selection)),
            PromptMemberStatement::Stated,
        )?)
        .await?;

    let targets = ModelTargetCatalog::try_from_definitions([
        ModelTargetDefinition::new(
            first_selection,
            ResolvedProviderTarget::naming(first_target),
        ),
        ModelTargetDefinition::new(
            second_selection,
            ResolvedProviderTarget::naming(second_target),
        ),
    ])
    .expect("two exact selections form one immutable target catalog");
    let first_messages = complete_text_turn(
        &pool,
        session,
        targets.clone(),
        first_credential.clone(),
        0x10_000,
        "first reply",
    )
    .await?;
    assert_eq!(
        application_user_message(
            first_messages
                .first()
                .expect("the first call carries its user input")
        ),
        (first_input, first_content)
    );

    let second_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x932));
    let second_turn = TurnId::from_uuid(Uuid::from_u128(0xa32));
    let mut second_submit = SubmitInputService::new(
        FixedSubmitInputIds::new([second_input], [second_turn]),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    let second_content = "after model replacement";
    second_submit
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x234)),
            session,
            UserContent::try_text(second_content.to_owned())
                .expect("fixture user content is admitted"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: input_choices(2, ModelSelectionOverride::UseSessionDefault),
            },
        )?)
        .await?;
    let mut colliding_activation = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd33))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xe33))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xb33))],
        )
        .with_model_identity_entries([SemanticTranscriptEntryId::from_uuid(
            Uuid::from_u128(0xd31),
        )]),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert!(matches!(
        colliding_activation
            .execute(session)
            .await
            .expect_err("the reused model-boundary identity must fail before activation"),
        StartEligibleTurnRepositoryError::IdentityCollision(
            StartEligibleTurnIdentityCollision::ModelIdentityEntry
        )
    ));
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(0xd32),
            starting_frontier: Uuid::from_u128(0xe32),
            initial_attempt: Uuid::from_u128(0xb32),
        },
    )
    .await?;

    let active_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(session)
        .await?
        .expect("the active successor has a transcript");
    let process_tail = last_two(active_snapshot.entries());
    assert_eq!(
        process_model_identity(process_tail.penultimate),
        (second_turn, 2, second_selection)
    );
    assert_eq!(
        process_user_entry(process_tail.last),
        (second_input, second_turn, second_content)
    );

    let second_messages = complete_text_turn(
        &pool,
        session,
        targets,
        second_credential.clone(),
        0x20_000,
        "second reply",
    )
    .await?;
    let application_tail = last_two(&second_messages);
    assert_eq!(
        application_model_identity(application_tail.penultimate),
        (2, second_selection)
    );
    assert_eq!(
        application_user_message(application_tail.last),
        (second_input, second_content)
    );

    let first_pin: ModelCallPinFacts = sqlx::query_as(
        "SELECT direct_model_selection_id,
                resolved_provider_model_identity_id,
                credential_reference
           FROM model_call
          WHERE turn_id = $1",
    )
    .bind(first_turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    let second_pin: ModelCallPinFacts = sqlx::query_as(
        "SELECT direct_model_selection_id,
                resolved_provider_model_identity_id,
                credential_reference
           FROM model_call
          WHERE turn_id = $1",
    )
    .bind(second_turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        first_pin,
        ModelCallPinFacts {
            direct_model_selection_id: first_selection.into_uuid(),
            resolved_provider_model_identity_id: first_target.into_uuid(),
            credential_reference: first_credential.as_str().to_owned(),
        }
    );
    assert_eq!(
        second_pin,
        ModelCallPinFacts {
            direct_model_selection_id: second_selection.into_uuid(),
            resolved_provider_model_identity_id: second_target.into_uuid(),
            credential_reference: second_credential.as_str().to_owned(),
        }
    );

    sqlx::query(
        "ALTER TABLE semantic_transcript_entry
            DROP CONSTRAINT semantic_transcript_entry_payload_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE semantic_transcript_entry DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE semantic_transcript_entry
            SET failed_turn_id = $1
          WHERE model_identity_turn_id = $1",
    )
    .bind(second_turn.into_uuid())
    .execute(&pool)
    .await?;
    let mut corrupt_read = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd34))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xe34))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xb34))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert!(matches!(
        corrupt_read
            .execute(session)
            .await
            .expect_err("a mixed model-boundary payload must fail closed"),
        StartEligibleTurnRepositoryError::Corruption(StartEligibleTurnCorruption::Scheduling(
            SubmitInputCorruption::Inconsistent("semantic entry payload")
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn compact_session_command_id_reuse_is_a_client_conflict() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = Uuid::from_u128(0xc051);
    let command = Uuid::from_u128(0xc052);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0xc053, 0xc051, direct(0xc054)))
        .await?;
    insert_pending_compact_command(
        &pool,
        command,
        session,
        Uuid::from_u128(0xc055),
        Uuid::from_u128(0xc056),
    )
    .await?;
    let input = start_input(
        0xc052,
        0xc051,
        "compact command identity reuse",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );

    assert_eq!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                input.clone(),
                AcceptedInputId::from_uuid(Uuid::from_u128(0xc057)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xc058))),
            )
            .await?,
        SubmitInputHandlingOutcome::ConflictingReuse {
            command_id: input.command_id(),
        }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-012: registry dispatch remains user-global across command kinds while
/// purpose-specific loads distinguish a valid other-kind claim from absence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv012_cross_kind_reuse_is_conflict_not_corruption_or_absence()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let imported_repository =
        ImportedSessionRepository::new(pool.clone(), test_session_credential_pin());
    let defaults_repository = ReplaceSessionDefaultsRepository::new(pool.clone());
    let input_repository = SubmitInputRepository::new(pool.clone());
    let creation = prepared(0x221, 0x721, direct(0x821));
    create_repository.handle(creation).await?;
    assert!(matches!(
        imported_repository
            .load(DurableCommandId::from_uuid(Uuid::from_u128(0x221)))
            .await
            .expect_err("a CreateSession ID is not an unseen imported creation"),
        ImportedSessionRepositoryError::DifferentCommandKind { .. }
    ));

    let defaults_reuse = replacement(0x221, 0x721, 1, alias(0x822));
    assert_eq!(
        defaults_repository.handle(defaults_reuse.clone()).await?,
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
    defaults_repository.handle(defaults.clone()).await?;
    let create_reuse = prepared(0x222, 0x722, direct(0x824));
    assert_eq!(
        create_repository.handle(create_reuse.clone()).await?,
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
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
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
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
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
    let exhausted_outcome = defaults_repository.handle(exhausted.clone()).await?;
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
            .execute(fails_after_claim.clone())
            .await
            .expect_err("the colliding immutable successor aborts the transaction"),
        ReplaceSessionDefaultsRepositoryError::Database { .. }
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
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
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
        create_repository.handle(direct_creation.clone()).await?,
        CreateSessionHandlingOutcome::Applied(direct_creation.applied_result())
    );
    assert_eq!(
        create_repository.handle(alias_creation.clone()).await?,
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
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let session_repository = SessionRepository::new(pool.clone());
    let missing_pointer = prepared(0x511, 0x911, direct(0x811));
    let invalid_pointer = prepared(0x512, 0x912, direct(0x812));
    let missing_selected = prepared(0x513, 0x913, direct(0x813));
    let malformed_selected = prepared(0x514, 0x914, direct(0x814));
    let unknown_provenance = prepared(0x515, 0x915, direct(0x815));
    let duplicate_projection = prepared(0x516, 0x916, direct(0x816));
    create_repository.handle(missing_pointer.clone()).await?;
    create_repository.handle(invalid_pointer.clone()).await?;
    create_repository.handle(missing_selected.clone()).await?;
    create_repository.handle(malformed_selected.clone()).await?;
    create_repository.handle(unknown_provenance.clone()).await?;
    create_repository
        .handle(duplicate_projection.clone())
        .await?;

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

    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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

/// The persistence contract mirrors the one-mebibyte accepted-input
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

    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
        signalbox_application::InProcessToolDispatchGate::default(),
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
        signalbox_application::InProcessToolDispatchGate::default(),
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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

/// S03 / S10 / INV-007 / INV-009: the Postgres safety-net sweep finds durable
/// queued work and resumable tool batches while excluding unrelated active
/// model work.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s03_inv007_inv009_postgres_sweep_reconstructs_only_candidate_sessions()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x389, 0x789, direct(0x889)))
        .await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    let tool_seed = 0x7900;
    let (tool_fixture, _, _, tool_request) =
        checkpoint_confirmed_tool_round(&pool, tool_seed, "current_time", "{}").await?;
    PostgresToolLoopRepository::new(pool.clone())
        .decide(
            DecideToolRequest::try_new(
                DurableCommandId::from_uuid(Uuid::from_u128(tool_seed + 24)),
                tool_request,
                ToolApprovalDecision::Approve,
            )
            .expect("fixture decision command is valid"),
            || TurnAttemptId::from_uuid(Uuid::from_u128(tool_seed + 23)),
        )
        .await?;

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

    assert_eq!(candidates, vec![queued_session, tool_fixture.session]);
    assert_eq!(
        PostgresToolLoopRepository::new(pool.clone())
            .find_resumable_turn(tool_fixture.session)
            .await?,
        Some(tool_fixture.turn)
    );
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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

/// S01 / INV-009: once the scheduler lock admits and prepares one exact
/// queued candidate, a guarded activation that matches no row is durable
/// divergence, not a stale wake-up, and rolls back every preceding write.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv009_start_eligible_turn_zero_row_guard_is_inconsistent()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
        "UPDATE context_frontier_delta
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
        "INSERT INTO context_frontier_delta
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
        "INSERT INTO context_frontier_delta
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
        "INSERT INTO context_frontier_delta
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
        signalbox_application::InProcessToolDispatchGate::default(),
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
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
    );
    let CreateSessionOutcome::Applied(created) = create_service
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x4a1)),
            SessionConfigurationDefaults::new(direct(0xca1)),
        )?)
        .await?
    else {
        panic!("user-initiated composed creation must apply");
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
        signalbox_application::InProcessToolDispatchGate::default(),
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
    let SubmitInputOutcome::Recorded(SubmitInputResult::Rejected(
        SubmitInputRejectedResult::ActiveTurnPresent {
            session: rejected_session,
            active_turn,
        },
    )) = blocked
    else {
        panic!("a start against the service-activated slot must be rejected");
    };
    assert_eq!(
        rejected_session, session,
        "the occupied-slot rejection names the session"
    );
    assert_eq!(
        active_turn,
        activated.turn(),
        "the occupied-slot rejection names the active turn"
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
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
    );
    let CreateSessionOutcome::Applied(created) = create_service
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x4c1)),
            SessionConfigurationDefaults::new(direct(0xcc1)),
        )?)
        .await?
    else {
        panic!("user-initiated composed creation must apply");
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
        signalbox_application::InProcessToolDispatchGate::default(),
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
    let SubmitInputOutcome::Recorded(SubmitInputResult::Rejected(
        SubmitInputRejectedResult::ActiveTurnPresent {
            session: rejected_session,
            active_turn,
        },
    )) = blocked
    else {
        panic!("a start against the After-lineage slot must be rejected");
    };
    assert_eq!(
        rejected_session, session,
        "the successor rejection names the session"
    );
    assert_eq!(
        active_turn,
        second_activated.turn(),
        "the successor rejection names the active turn"
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    let steering_frontier_assertion: String = sqlx::query_scalar(
        "SELECT pg_get_functiondef(oid)
           FROM pg_proc
          WHERE proname = 'assert_model_call_steering_final_state'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(steering_frontier_assertion.contains(
        "earlier.disposition_kind IN (
                    'pending_steering',
                    'reclassified_as_turn_origin'
               )"
    ));

    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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

    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "pending acceptance must remain blocked on the source lifecycle row"
    );

    terminalize_source.commit().await?;
    let pending_database_error = submit_input_database_error(
        pending_acceptance
            .await?
            .expect_err("steering must fail after racing source terminalization commits"),
    );
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    assert_eq!(first.recovered_turn_count(), 1);

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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
        ("active".into(), "prepared".into(), 0, 0, Decimal::from(3))
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
        ("terminal".into(), "ended".into(), 1, 1, Decimal::from(4))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S08 / S09 / INV-016 / INV-034 / INV-036: evidence-free restart recovery
/// ends the abandoned source attempt and atomically reclassifies pending
/// steering, leaving no startup blocker on replay.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s08_s09_inv016_inv034_inv036_restart_reclassifies_pending_steering()
-> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let session_uuid = Uuid::from_u128(0x7c1);
    let turn_uuid = Uuid::from_u128(0xac1);
    let attempt_uuid = Uuid::from_u128(0xbc1);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
        )
        .with_reclassified_turns([TurnId::from_uuid(Uuid::from_u128(0xac2))]),
        PostgresStartupScanRepository::new(restarted_pool.clone()),
    );

    let first = scan.execute().await?;
    assert_eq!(first.recovered_turn_count(), 1);
    let replay = scan.execute().await?;
    assert_eq!(replay.recovered_turn_count(), 0);

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
    assert_eq!(recovery_events, (1, 1));

    let recovered: (String, String, i64, i64, String, Uuid, String) = sqlx::query_as(
        "SELECT turn.state_kind,
                attempt.state_kind,
                (SELECT count(*) FROM semantic_transcript_entry
                  WHERE payload_kind = 'turn_failed' AND failed_turn_id = $1),
                (SELECT count(*) FROM context_frontier
                  WHERE owning_session_id = $2),
                accepted.disposition_kind,
                accepted.origin_turn_id,
                successor.state_kind
           FROM turn_lifecycle AS turn
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = $4
            JOIN accepted_input AS accepted
              ON accepted.accepted_input_id = $3
            JOIN turn_lifecycle AS successor
              ON successor.turn_id = accepted.origin_turn_id
          WHERE turn.turn_id = $1",
    )
    .bind(turn_uuid)
    .bind(session_uuid)
    .bind(pending_input.into_uuid())
    .bind(attempt_uuid)
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        recovered,
        (
            "terminal".into(),
            "ended".into(),
            1,
            2,
            "reclassified_as_turn_origin".into(),
            Uuid::from_u128(0xac2),
            "queued".into(),
        )
    );
    let mut completed_recovery_ids = FixedStartupScanIds::new([], []);
    assert_eq!(
        PostgresStartupScanRepository::new(restarted_pool.clone())
            .recover(
                SessionId::from_uuid(session_uuid),
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xec3)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(0xfc3)),
                ),
                &mut completed_recovery_ids,
            )
            .await?,
        StartupScanSessionOutcome::NoActiveTurn
    );

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / S03 / S07 / S08 / S09 / INV-001 / INV-008 / INV-012 / INV-029 /
/// INV-037: occupied-slot rejection evidence is recorded exactly, generated
/// identities cannot reuse the active origin, and a matching interrupt
/// atomically cancels prepared work while recording and prioritizing its exact
/// immediate successor ahead of previously queued ordinary work.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s03_s07_inv008_inv012_inv029_inv037_prepared_interrupt_is_exact()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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

    let queued_before_interrupt = repository
        .handle(
            input_with_delivery(
                0x44b,
                0x841,
                "ordinary queued before interrupt",
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: active_origin_turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x948)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa48))),
        )
        .await?;
    assert!(matches!(
        queued_before_interrupt,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    let pending_before_interrupt = repository
        .handle(
            input_with_delivery(
                0x44c,
                0x841,
                "pending steering before interrupt",
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active_origin_turn,
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x949)),
            None,
        )
        .await?;
    assert!(matches!(
        pending_before_interrupt,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));

    let matching_interrupt = input_with_delivery(
        0x447,
        0x841,
        "matching interrupt",
        DeliveryRequest::Interrupt {
            expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa41)),
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let outcome = repository
        .handle(
            matching_interrupt.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x946)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa46))),
        )
        .await
        .expect("matching interrupt applies atomically");
    assert!(matches!(
        &outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(applied)
        )) if applied.turn() == TurnId::from_uuid(Uuid::from_u128(0xa46))
            && applied.applied_interrupt().is_some()
    ));
    let claimed: (i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM durable_command WHERE command_id = $1),
            (SELECT count(*) FROM submit_input_command WHERE command_id = $1),
            (SELECT count(*) FROM accepted_input WHERE accepting_command_id = $1),
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE origin_accepted_input_id = $2),
            (SELECT count(*)
               FROM queued_input_origin
              WHERE accepted_input_id = $2
                AND priority_kind = 'interrupt_immediately_after'
                AND interrupt_predecessor_turn_id = $3),
            (SELECT count(*)
               FROM turn_attempt
              WHERE turn_id = $3
                AND state_kind = 'ended'
                AND end_variant = 'after_cancellation'
                AND end_disposition = 'cancelled'
                AND interrupt_command_id = $1
                AND interrupt_predecessor_turn_id = $3),
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE turn_id = $3
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'cancelled')",
    )
    .bind(Uuid::from_u128(0x447))
    .bind(Uuid::from_u128(0x946))
    .bind(Uuid::from_u128(0xa41))
    .fetch_one(&pool)
    .await?;
    assert_eq!(claimed, (1, 1, 1, 1, 1, 1, 1));

    let next = input_with_delivery(
        0x448,
        0x841,
        "safe point after direct cancellation",
        DeliveryRequest::NextSafePoint {
            expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa41)),
        },
    );
    let next_outcome = repository
        .handle(
            next.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x947)),
            None,
        )
        .await?;
    assert!(matches!(
        &next_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::NoActiveTurn {
                session,
                expected_active_turn,
            }
        )) if *session == SessionId::from_uuid(Uuid::from_u128(0x841))
            && *expected_active_turn == TurnId::from_uuid(Uuid::from_u128(0xa41))
    ));
    assert_eq!(
        repository
            .handle(
                matching_interrupt,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fd)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xafd))),
            )
            .await?,
        outcome
    );
    assert_eq!(
        repository
            .handle(
                next,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fc)),
                None,
            )
            .await?,
        next_outcome
    );

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

    let interrupt_successor = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: Uuid::from_u128(0x841),
            origin_entry: Uuid::from_u128(0xd46),
            starting_frontier: Uuid::from_u128(0xe46),
            initial_attempt: Uuid::from_u128(0xb46),
        },
    )
    .await?;
    assert_eq!(
        interrupt_successor.turn(),
        TurnId::from_uuid(Uuid::from_u128(0xa46))
    );
    assert_eq!(
        interrupt_successor.start().lineage(),
        AcceptedInputStartingLineage::After {
            immediate_predecessor: active_origin_turn,
        }
    );
    let remaining_queue: (String, i64) = sqlx::query_as(
        "SELECT
            (SELECT state_kind FROM turn_lifecycle WHERE turn_id = $1),
            (SELECT count(*)
               FROM accepted_input AS accepted
               JOIN turn_lifecycle AS lifecycle
                 ON lifecycle.turn_id = accepted.origin_turn_id
              WHERE accepted.accepted_input_id = $2
                AND accepted.disposition_kind = 'reclassified_as_turn_origin'
                AND lifecycle.state_kind = 'queued')",
    )
    .bind(Uuid::from_u128(0xa48))
    .bind(Uuid::from_u128(0x949))
    .fetch_one(&pool)
    .await?;
    assert_eq!(remaining_queue, ("queued".to_owned(), 1));

    let mut interrupted_recovery_ids = FixedStartupScanIds::new([], []);
    assert!(matches!(
        PostgresStartupScanRepository::new(pool.clone())
            .recover(
                SessionId::from_uuid(Uuid::from_u128(0x841)),
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd47)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(0xe47)),
                ),
                &mut interrupted_recovery_ids,
            )
            .await?,
        StartupScanSessionOutcome::Recovered { .. }
    ));
    let ordinary_successor = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: Uuid::from_u128(0x841),
            origin_entry: Uuid::from_u128(0xd48),
            starting_frontier: Uuid::from_u128(0xe48),
            initial_attempt: Uuid::from_u128(0xb48),
        },
    )
    .await?;
    assert_eq!(
        ordinary_successor.turn(),
        TurnId::from_uuid(Uuid::from_u128(0xa48))
    );
    assert_eq!(
        ordinary_successor.start().lineage(),
        AcceptedInputStartingLineage::After {
            immediate_predecessor: TurnId::from_uuid(Uuid::from_u128(0xa46)),
        }
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
        "INSERT INTO context_frontier_delta
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
                WHERE relation.relname = 'context_frontier_delta'
                  AND candidate.tgname = 'context_frontier_member_requires_complete_membership'
            ),
            count(*) FILTER (
                WHERE relation.relname = 'context_frontier_delta'
                  AND candidate.tgname = 'context_frontier_member_stays_within_declared_count'
                  AND NOT candidate.tgdeferrable
            ),
            count(*) FILTER (
                WHERE relation.relname = 'context_frontier_delta'
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

    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
        "INSERT INTO context_frontier_delta
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
    let create = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
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
    let non_user = repository
        .load(first.command_id())
        .await
        .expect_err("domain reconstitution rejects a stored non-user actor");
    assert!(matches!(
        non_user,
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
    sqlx::query(
        "ALTER TABLE input_accepted_outbox_event
            DROP CONSTRAINT input_accepted_outbox_origin_fk",
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
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
        SubmitInputRejectedResult::AcceptancePositionExhausted { last, .. },
    )) = repository
        .handle(
            exhausted,
            AcceptedInputId::from_uuid(Uuid::from_u128(0x933)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa33))),
        )
        .await?
    else {
        panic!("the maximum stored position rejects the next input");
    };
    assert_eq!(
        last.as_u64(),
        u64::MAX,
        "the exhaustion receipt retains the maximum position"
    );

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

/// S24: session summaries are one complete repeatable-read projection in
/// stable session-identity order, including the selected defaults row.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_process_session_summary_sequence_matches_repeatable_projection()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let earlier_selection = outbox_session_fixture_model_selection(0xe31);
    let earlier_session = insert_outbox_session_fixture(&pool, 0xe31).await?;
    let later_session = Uuid::from_u128(0xe32);
    let alias = ModelAlias::from_uuid(Uuid::from_u128(0xae32));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x4e32, 0xe32, ModelSelectionRequest::Alias(alias)))
        .await?;

    let mut summaries = ProcessReadRepository::new(pool.clone())
        .open_session_summaries()
        .await?;
    let earlier = summaries
        .next_summary()
        .await?
        .ok_or("the earlier session summary is present")?;
    let later = summaries
        .next_summary()
        .await?
        .ok_or("the later session summary is present")?;
    assert!(summaries.next_summary().await?.is_none());

    assert_eq!(summaries.summary_count(), Some(2));
    assert_eq!(earlier.session().into_uuid(), earlier_session);
    assert_eq!(earlier.defaults_version(), 1);
    assert_eq!(
        earlier.model_selection(),
        ProcessModelSelection::Direct(earlier_selection)
    );
    assert_eq!(later.session().into_uuid(), later_session);
    assert_eq!(later.defaults_version(), 1);
    assert_eq!(later.model_selection(), ProcessModelSelection::Alias(alias));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24 / INV-032: the process transcript read observes the global outbox
/// cursor, ordered turn state, and latest semantic frontier in one
/// repeatable-read snapshot.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_inv032_process_transcript_is_one_authoritative_snapshot() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8e41));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0xce41));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            0x4e41,
            0x8e41,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9e41));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xae41));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x4e42,
                0x8e41,
                "projected user request",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            accepted_input,
            Some(turn),
        )
        .await?;
    let repository = ProcessReadRepository::new(pool.clone());
    assert!(
        repository
            .read_transcript(SessionId::from_uuid(Uuid::from_u128(0xffff)))
            .await?
            .is_none()
    );
    let queued_snapshot = repository
        .read_transcript(session)
        .await?
        .expect("the committed session has a transcript projection");

    assert_eq!(queued_snapshot.session(), session);
    assert_eq!(queued_snapshot.cursor(), 2);
    assert_eq!(queued_snapshot.turns().len(), 1);
    assert_eq!(queued_snapshot.turns()[0].turn(), turn);
    assert_eq!(queued_snapshot.turns()[0].acceptance_position(), 1);
    assert_eq!(
        queued_snapshot.turns()[0].state(),
        &ProcessTurnState::Queued {
            accepted_input,
            content: "projected user request".to_owned(),
        }
    );
    assert!(queued_snapshot.entries().is_empty());

    let origin_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde41));
    let starting_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xee41));
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xbe41));
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: origin_entry.into_uuid(),
            starting_frontier: starting_frontier.into_uuid(),
            initial_attempt: attempt.into_uuid(),
        },
    )
    .await?;

    let snapshot = repository
        .read_transcript(session)
        .await?
        .expect("the committed session has a transcript projection");

    assert_eq!(snapshot.session(), session);
    assert_eq!(snapshot.cursor(), 3);
    assert_eq!(snapshot.turns().len(), 1);
    assert_eq!(snapshot.turns()[0].turn(), turn);
    assert_eq!(snapshot.turns()[0].acceptance_position(), 1);
    assert_eq!(
        snapshot.turns()[0].state(),
        &ProcessTurnState::ActiveRunning {
            current_attempt: attempt,
            current_model_call: None,
        }
    );
    assert_eq!(
        snapshot.entries(),
        [ProcessTranscriptEntry::User {
            entry_index: 0,
            source_session: session,
            entry: origin_entry,
            accepted_input,
            turn,
            content: "projected user request".to_owned(),
        }]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[track_caller]
fn assert_running_current_model_call(
    state: &ProcessTurnState,
    expected_attempt: TurnAttemptId,
    expected_call: ModelCallId,
    expected_state: ProcessCurrentModelCallState,
) {
    let ProcessTurnState::ActiveRunning {
        current_attempt,
        current_model_call: Some(current_model_call),
    } = state
    else {
        panic!("expected one current model call on a running turn");
    };
    assert_eq!(*current_attempt, expected_attempt);
    assert_eq!(current_model_call.call(), expected_call);
    assert_eq!(current_model_call.state(), expected_state);
}

/// S24 / INV-032: a process transcript snapshot exposes the exact durable
/// Prepared, InFlight, or CancellationRequested state of the current model
/// call.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_inv032_process_transcript_projects_current_model_call_state()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let prepared = checkpoint_restart_model_call(&pool, 0x8e50, false).await?;
    let in_flight = checkpoint_restart_model_call(&pool, 0x8e60, true).await?;
    let repository = ProcessReadRepository::new(pool.clone());
    let prepared_snapshot = repository
        .read_transcript(prepared.session)
        .await?
        .expect("the prepared-call session is committed");
    let in_flight_snapshot = repository
        .read_transcript(in_flight.session)
        .await?
        .expect("the in-flight-call session is committed");

    assert_eq!(prepared_snapshot.turns().len(), 1);
    assert_running_current_model_call(
        prepared_snapshot.turns()[0].state(),
        prepared.attempt,
        prepared.call,
        ProcessCurrentModelCallState::Prepared,
    );
    assert_eq!(in_flight_snapshot.turns().len(), 1);
    assert_running_current_model_call(
        in_flight_snapshot.turns()[0].state(),
        in_flight.attempt,
        in_flight.call,
        ProcessCurrentModelCallState::InFlight,
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24 / INV-032: the production dispatcher offers one exact next event before
/// advancing the locked durable prefix. Consumer retry and an injected deferred
/// commit failure after the offer both roll the prefix back, so restart offers
/// the same cursor again before the later committed event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_inv032_dispatcher_redelivers_after_cursor_commit_failure_in_order()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first_session = insert_outbox_session_fixture(&pool, 0xe17).await?;
    let second_session = insert_outbox_session_fixture(&pool, 0xe18).await?;
    let mut first_transaction = pool.begin().await?;
    append_session_created_test_event(&mut first_transaction, first_session).await?;
    first_transaction.commit().await?;
    let mut second_transaction = pool.begin().await?;
    append_session_created_test_event(&mut second_transaction, second_session).await?;
    second_transaction.commit().await?;
    sqlx::query(
        "CREATE FUNCTION fail_test_outbox_delivery_commit()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $$
         BEGIN
             RAISE EXCEPTION 'injected delivery cursor commit failure'
                 USING ERRCODE = '40001';
         END;
         $$",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE CONSTRAINT TRIGGER zz_test_fail_outbox_delivery_commit
         AFTER UPDATE ON outbox_delivery_state
         DEFERRABLE INITIALLY DEFERRED
         FOR EACH ROW
         EXECUTE FUNCTION fail_test_outbox_delivery_commit()",
    )
    .execute(&pool)
    .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    let offered = Arc::new(Mutex::new(Vec::new()));
    let retry_offer = Arc::clone(&offered);
    assert_eq!(
        dispatcher
            .dispatch_next(move |event| {
                retry_offer
                    .lock()
                    .expect("offer log lock")
                    .push((event.sequence(), event.session().into_uuid()));
                OutboxDeliveryDecision::Retry
            })
            .await?,
        OutboxDispatchOutcome::Retry { sequence: 1 }
    );
    let first_offer = Arc::clone(&offered);
    assert!(matches!(
        dispatcher
            .dispatch_next(move |event| {
                first_offer
                    .lock()
                    .expect("offer log lock")
                    .push((event.sequence(), event.session().into_uuid()));
                OutboxDeliveryDecision::Delivered
            })
            .await,
        Err(OutboxDispatchError::Database(_))
    ));
    assert_eq!(
        sqlx::query_scalar::<_, Decimal>(
            "SELECT delivered_through
               FROM outbox_delivery_state
              WHERE singleton",
        )
        .fetch_one(&pool)
        .await?,
        Decimal::ZERO
    );

    sqlx::query(
        "DROP TRIGGER zz_test_fail_outbox_delivery_commit
            ON outbox_delivery_state",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DROP FUNCTION fail_test_outbox_delivery_commit()")
        .execute(&pool)
        .await?;

    let first_redelivery = Arc::clone(&offered);
    assert_eq!(
        dispatcher
            .dispatch_next(move |event| {
                first_redelivery
                    .lock()
                    .expect("offer log lock")
                    .push((event.sequence(), event.session().into_uuid()));
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );
    let second_delivery = Arc::clone(&offered);
    assert_eq!(
        dispatcher
            .dispatch_next(move |event| {
                second_delivery
                    .lock()
                    .expect("offer log lock")
                    .push((event.sequence(), event.session().into_uuid()));
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 2 }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Idle
    );
    assert_eq!(
        offered.lock().expect("offer log lock").as_slice(),
        [
            (1, first_session),
            (1, first_session),
            (1, first_session),
            (2, second_session)
        ]
    );
    assert_eq!(
        sqlx::query_scalar::<_, Decimal>(
            "SELECT delivered_through
               FROM outbox_delivery_state
              WHERE singleton",
        )
        .fetch_one(&pool)
        .await?,
        Decimal::from(2)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S10 / INV-002: storage independently rejects a restored tool response whose
/// request inventory exceeds the bounded domain vocabulary.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_inv002_tool_round_storage_rejects_more_than_32_requests() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let error = sqlx::query(
        "INSERT INTO tool_round
            (producing_model_call_id, session_id, turn_id, boundary_kind,
             boundary_frontier_id, response_part_count, request_count)
         VALUES ($1, $2, $3, 'continuing', $4, 33, 33)",
    )
    .bind(Uuid::from_u128(1))
    .bind(Uuid::from_u128(2))
    .bind(Uuid::from_u128(3))
    .bind(Uuid::from_u128(4))
    .execute(&pool)
    .await
    .expect_err("the request-count constraint rejects the thirty-third request");

    assert_eq!(
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("tool_round_counts_bounded")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24 / INV-032: an allocator cursor beyond the delivered prefix requires its
/// exact committed header; dispatcher idle is reserved for equal cursors.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_inv032_dispatcher_reports_a_missing_committed_header() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    sqlx::query(
        "ALTER TABLE outbox_sequence_state
         DISABLE TRIGGER outbox_sequence_requires_event",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE outbox_sequence_state
            SET last_sequence = 1,
                last_allocation_xid = pg_current_xact_id()
          WHERE singleton",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_sequence_state
         ENABLE TRIGGER outbox_sequence_requires_event",
    )
    .execute(&pool)
    .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert!(matches!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::MissingCommittedEventHeader
        ))
    ));
    assert_eq!(
        sqlx::query_scalar::<_, Decimal>(
            "SELECT delivered_through
               FROM outbox_delivery_state
              WHERE singleton",
        )
        .fetch_one(&pool)
        .await?,
        Decimal::ZERO
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24 / INV-032: a header restored ahead of the allocator cursor is durable
/// corruption and is never offered to the consumer.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_inv032_dispatcher_rejects_a_header_beyond_the_allocator() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = insert_outbox_session_fixture(&pool, 0xe1a).await?;
    let mut producer = pool.begin().await?;
    append_session_created_test_event(&mut producer, session).await?;
    producer.commit().await?;

    sqlx::query(
        "ALTER TABLE outbox_sequence_state
         DISABLE TRIGGER outbox_sequence_requires_event",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE outbox_sequence_state
            SET last_sequence = 0,
                last_allocation_xid = NULL
          WHERE singleton",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_sequence_state
         ENABLE TRIGGER outbox_sequence_requires_event",
    )
    .execute(&pool)
    .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("an unallocated header must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::EventBeyondAllocatedSequence
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24 / INV-032: a restored header above both the allocator and the exact next
/// slot is corruption rather than an idle outbox.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_inv032_dispatcher_rejects_a_noncontiguous_header_beyond_the_allocator()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first_session = insert_outbox_session_fixture(&pool, 0xe1b).await?;
    let second_session = insert_outbox_session_fixture(&pool, 0xe1c).await?;
    let mut first_producer = pool.begin().await?;
    append_session_created_test_event(&mut first_producer, first_session).await?;
    first_producer.commit().await?;
    let mut second_producer = pool.begin().await?;
    append_session_created_test_event(&mut second_producer, second_session).await?;
    second_producer.commit().await?;

    sqlx::query("ALTER TABLE session_created_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE outbox_sequence_state
         DISABLE TRIGGER outbox_sequence_requires_event",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DELETE FROM session_created_outbox_event WHERE event_sequence = 1")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM outbox_event WHERE event_sequence = 1")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE outbox_sequence_state
            SET last_sequence = 0,
                last_allocation_xid = NULL
          WHERE singleton",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_created_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE outbox_sequence_state
         ENABLE TRIGGER outbox_sequence_requires_event",
    )
    .execute(&pool)
    .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("an unallocated header must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::EventBeyondAllocatedSequence
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24 / INV-032: exhausted delivery still validates the allocator singleton
/// rather than silently polling forever on missing durable state.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_inv032_dispatcher_validates_the_allocator_at_exhaustion() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    sqlx::query(
        "ALTER TABLE outbox_delivery_state
         DISABLE TRIGGER outbox_delivery_advances_prefix",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE outbox_delivery_state
            SET delivered_through = 18446744073709551615,
                last_delivery_xid = pg_current_xact_id()
          WHERE singleton",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_delivery_state
         ENABLE TRIGGER outbox_delivery_advances_prefix",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_sequence_state
         DISABLE TRIGGER outbox_sequence_state_cannot_be_deleted",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DELETE FROM outbox_sequence_state WHERE singleton")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE outbox_sequence_state
         ENABLE TRIGGER outbox_sequence_state_cannot_be_deleted",
    )
    .execute(&pool)
    .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("exhausted delivery cannot offer an event"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::MissingSequenceState
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24 / INV-032: independently valid same-session terminal identifiers do not
/// form a dispatchable event unless they all describe the event's exact turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_inv032_dispatcher_rejects_crosswired_terminal_correlations()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = Uuid::from_u128(0x7e1);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x3e0, 0x7e1, direct(0x8e1)))
        .await?;
    let inputs = SubmitInputRepository::new(pool.clone());

    let first_turn = Uuid::from_u128(0xae1);
    inputs
        .handle(
            start_input(
                0x3e1,
                0x7e1,
                "first failed turn",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9e1)),
            Some(TurnId::from_uuid(first_turn)),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session,
            origin_entry: Uuid::from_u128(0xce1),
            starting_frontier: Uuid::from_u128(0xde1),
            initial_attempt: Uuid::from_u128(0xbe1),
        },
    )
    .await?;
    let mut first_scan = StartupScanService::new(
        FixedStartupScanIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xee1))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xfe1))],
        ),
        PostgresStartupScanRepository::new(pool.clone()),
    );
    assert_eq!(first_scan.execute().await?.recovered_turn_count(), 1);

    let second_turn = Uuid::from_u128(0xae2);
    inputs
        .handle(
            start_input(
                0x3e2,
                0x7e1,
                "second failed turn",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9e2)),
            Some(TurnId::from_uuid(second_turn)),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session,
            origin_entry: Uuid::from_u128(0xce2),
            starting_frontier: Uuid::from_u128(0xde2),
            initial_attempt: Uuid::from_u128(0xbe2),
        },
    )
    .await?;
    let mut second_scan = StartupScanService::new(
        FixedStartupScanIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xee2))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xfe2))],
        ),
        PostgresStartupScanRepository::new(pool.clone()),
    );
    assert_eq!(second_scan.execute().await?.recovered_turn_count(), 1);

    let failures: Vec<(Decimal, Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT event_sequence, turn_id, failure_entry_id, terminal_frontier_id
           FROM turn_failed_outbox_event
          WHERE session_id = $1
          ORDER BY event_sequence",
    )
    .bind(session)
    .fetch_all(&pool)
    .await?;
    let [first, second] = failures.as_slice() else {
        return Err(std::io::Error::other("fixture did not produce two failures").into());
    };
    assert_eq!(first.1, first_turn);
    assert_eq!(second.1, second_turn);

    sqlx::query(
        "ALTER TABLE turn_failed_outbox_event
         DISABLE TRIGGER turn_failed_outbox_event_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DELETE FROM turn_failed_outbox_event WHERE event_sequence = $1")
        .bind(second.0)
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE turn_failed_outbox_event
            SET failure_entry_id = $1,
                terminal_frontier_id = $2
          WHERE event_sequence = $3",
    )
    .bind(second.2)
    .bind(second.3)
    .bind(first.0)
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE turn_failed_outbox_event
         ENABLE TRIGGER turn_failed_outbox_event_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_delivery_state
         DISABLE TRIGGER outbox_delivery_advances_prefix",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE outbox_delivery_state
            SET delivered_through = $1 - 1,
                last_delivery_xid = pg_current_xact_id()
          WHERE singleton",
    )
    .bind(first.0)
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_delivery_state
         ENABLE TRIGGER outbox_delivery_advances_prefix",
    )
    .execute(&pool)
    .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("a cross-wired terminal event must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidTerminalEventCorrelation
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24 / INV-032: the dispatcher observes the allocator and candidate header in
/// one statement snapshot, so an uncommitted allocation is idle rather than
/// false committed-header corruption.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_inv032_dispatcher_treats_an_uncommitted_allocation_as_idle()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = insert_outbox_session_fixture(&pool, 0xe19).await?;
    let mut producer = pool.begin().await?;
    let sequence = append_session_created_test_event(&mut producer, session).await?;
    let dispatcher = OutboxDispatcher::new(pool.clone());

    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Idle
    );
    producer.commit().await?;
    assert_eq!(sequence, Decimal::ONE);
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );

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

    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE hub_fence_state CASCADE").await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE outbox_sequence_state CASCADE").await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE outbox_delivery_state CASCADE").await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE outbox_event CASCADE").await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE session_created_outbox_event CASCADE")
        .await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE input_accepted_outbox_event CASCADE")
        .await?;
    assert_outbox_truncate_rejected(
        &pool,
        "TRUNCATE TABLE goal_turn_retired_outbox_event CASCADE",
    )
    .await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE turn_activated_outbox_event CASCADE")
        .await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE turn_failed_outbox_event CASCADE")
        .await?;
    assert_outbox_truncate_rejected(
        &pool,
        "TRUNCATE TABLE model_call_transition_outbox_event CASCADE",
    )
    .await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE turn_completed_outbox_event CASCADE")
        .await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE turn_refused_outbox_event CASCADE")
        .await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE turn_cancelled_outbox_event CASCADE")
        .await?;
    assert_outbox_truncate_rejected(
        &pool,
        "TRUNCATE TABLE turn_reconciliation_required_outbox_event CASCADE",
    )
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

    let repository = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let creation = prepared(0xe31, 0xe41, direct(0xe51));
    let command_id = creation.command().command_id().into_uuid();
    let session_id = creation.applied_result().session().into_uuid();
    let error = repository
        .handle(creation.clone())
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
        repository.handle(creation.clone()).await?,
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
    let repository = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let creation = prepared(0xe32, 0xe42, direct(0xe52));

    assert_eq!(
        repository.handle(creation.clone()).await?,
        CreateSessionHandlingOutcome::Applied(creation.applied_result())
    );
    assert_eq!(
        repository.handle(creation.clone()).await?,
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

/// S01 / INV-012 / INV-032: acceptance and activation append their complete
/// typed process transitions in the same commits, and command replay emits no
/// duplicate before the dispatcher advances the exact ordered prefix.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv012_inv032_scheduling_transitions_dispatch_in_commit_order()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0xe61));
    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0xe62));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xe63));
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xe64));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            0xe60,
            0xe61,
            ModelSelectionRequest::Direct(signalbox_domain::DirectModelSelection::from_uuid(
                Uuid::from_u128(0xe65),
            )),
        ))
        .await?;
    let command = start_input(
        0xe66,
        0xe61,
        "durable process input",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let repository = SubmitInputRepository::new(pool.clone());
    let recorded = repository
        .handle(command.clone(), accepted_input, Some(turn))
        .await?;
    assert_eq!(
        repository
            .handle(command, accepted_input, Some(turn))
            .await?,
        recorded
    );
    let activated = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(0xe67),
            starting_frontier: Uuid::from_u128(0xe68),
            initial_attempt: attempt.into_uuid(),
        },
    )
    .await?;
    assert_eq!(activated.turn(), turn);

    let dispatcher = OutboxDispatcher::new(pool.clone());
    let mut created = None;
    assert_eq!(
        dispatcher
            .dispatch_next(|event| {
                created = Some(event.clone());
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );
    let created = created.expect("the session creation event was offered");
    assert_eq!(created.session(), session);
    assert!(matches!(
        created.kind(),
        DispatchedOutboxEventKind::SessionCreated
    ));

    let mut accepted = None;
    assert_eq!(
        dispatcher
            .dispatch_next(|event| {
                accepted = Some(event.clone());
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 2 }
    );
    let accepted = accepted.expect("the input acceptance event was offered");
    assert_eq!(accepted.session(), session);
    assert_eq!(
        accepted.kind(),
        &DispatchedOutboxEventKind::InputAccepted {
            accepted_input,
            turn,
            acceptance_position: SessionInputPosition::first(),
            content: "durable process input".to_owned(),
        }
    );

    let mut activation = None;
    assert_eq!(
        dispatcher
            .dispatch_next(|event| {
                activation = Some(event.clone());
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 3 }
    );
    let activation = activation.expect("the turn activation event was offered");
    assert_eq!(activation.session(), session);
    assert_eq!(
        activation.kind(),
        &DispatchedOutboxEventKind::TurnActivated {
            turn,
            current_attempt: attempt,
        }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Idle
    );

    let durable_counts: (i64, i64, Decimal) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM input_accepted_outbox_event
              WHERE accepted_input_id = $1),
            (SELECT count(*) FROM turn_activated_outbox_event
              WHERE current_attempt_id = $2),
            (SELECT delivered_through FROM outbox_delivery_state
              WHERE singleton)",
    )
    .bind(accepted_input.into_uuid())
    .bind(attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_counts, (1, 1, Decimal::from(3)));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-032: an activation remains dispatchable after continuation while
/// its exact initial attempt and the lifecycle's current or terminal attempt
/// remain authoritative; cross-wired lifecycle provenance fails closed.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv032_turn_activation_dispatch_requires_authoritative_attempt()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0xe81));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xe82));
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xe83));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0xe80, 0xe81, direct(0xe84)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0xe85,
                0xe81,
                "activation correlation",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xe86)),
            Some(turn),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(0xe87),
            starting_frontier: Uuid::from_u128(0xe88),
            initial_attempt: attempt.into_uuid(),
        },
    )
    .await?;
    let mut startup = StartupScanService::new(
        FixedStartupScanIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xe89))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xe8a))],
        ),
        PostgresStartupScanRepository::new(pool.clone()),
    );
    assert_eq!(startup.execute().await?.recovered_turn_count(), 1);

    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 2 }
    );
    let mut activation = None;
    assert_eq!(
        dispatcher
            .dispatch_next(|event| {
                activation = Some(event.clone());
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 3 }
    );
    assert_eq!(
        activation
            .expect("the retained terminal attempt authorizes dispatch")
            .kind(),
        &DispatchedOutboxEventKind::TurnActivated {
            turn,
            current_attempt: attempt,
        }
    );

    sqlx::query(
        "ALTER TABLE turn_lifecycle
            DROP CONSTRAINT turn_lifecycle_terminal_attempt_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET terminal_attempt_id = $1
          WHERE turn_id = $2",
    )
    .bind(Uuid::from_u128(0xe8b))
    .bind(turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE outbox_delivery_state
         DISABLE TRIGGER outbox_delivery_advances_prefix",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE outbox_delivery_state
            SET delivered_through = 2,
                last_delivery_xid = pg_current_xact_id()
          WHERE singleton",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_delivery_state
         ENABLE TRIGGER outbox_delivery_advances_prefix",
    )
    .execute(&pool)
    .await?;

    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("cross-wired activation must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidLifecycleEventCorrelation
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-032: historical Prepared and InFlight transition records remain
/// dispatchable after advancement, but a terminal record must carry the
/// authoritative call's exact terminal disposition.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv032_terminal_model_call_dispatch_requires_exact_disposition()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xe90;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Ambiguous);
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Ambiguous(AmbiguousModelCallTurnIdentities::new(
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22)),
            )),
            |_| panic!("Ambiguous creates no pending-steering successors"),
        )
        .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    for sequence in 1..=3 {
        assert_eq!(
            dispatcher
                .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
                .await?,
            OutboxDispatchOutcome::Delivered { sequence }
        );
    }
    let mut prepared_transition = None;
    assert_eq!(
        dispatcher
            .dispatch_next(|event| {
                prepared_transition = Some(event.clone());
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 4 }
    );
    assert_eq!(
        prepared_transition
            .expect("historical Prepared transition is offered")
            .kind(),
        &DispatchedOutboxEventKind::ModelCallTransition {
            turn: fixture.turn,
            call: fixture.call,
            state: DispatchedModelCallState::Prepared,
        }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|event| {
                assert_eq!(
                    event.kind(),
                    &DispatchedOutboxEventKind::ModelCallTransition {
                        turn: fixture.turn,
                        call: fixture.call,
                        state: DispatchedModelCallState::InFlight,
                    }
                );
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 5 }
    );

    sqlx::query("ALTER TABLE model_call_transition_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE model_call_transition_outbox_event
            SET terminal_disposition_kind = 'cancelled'
          WHERE model_call_id = $1
            AND call_state_kind = 'terminal'",
    )
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE model_call_transition_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("cross-wired terminal transition must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidTerminalEventCorrelation
        ))
    ));

    let authoritative: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(authoritative, ("terminal".into(), Some("ambiguous".into())));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-032: a stored nonterminal model-call transition cannot be ahead
/// of the authoritative monotonic call state.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv032_model_call_dispatch_rejects_an_unreached_transition()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0xe98, false).await?;
    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 2 }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 3 }
    );

    sqlx::query("ALTER TABLE model_call_transition_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE model_call_transition_outbox_event
            SET call_state_kind = 'in_flight'
          WHERE model_call_id = $1
            AND call_state_kind = 'prepared'",
    )
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE model_call_transition_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("an unreached transition must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidModelCallState
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-032: a completed-turn event is dispatchable only while the
/// lifecycle's terminal attempt retains a completion-compatible disposition.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv032_completed_dispatch_requires_exact_terminal_attempt()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xea0;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Completed {
            assistant_text: vec![
                AssistantText::try_new(String::from("completed response"))
                    .expect("fixture assistant text is admitted"),
            ],
        });
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 22,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 23)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 24)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    let sequence = sqlx::query_scalar(
        "SELECT event_sequence
           FROM turn_completed_outbox_event
          WHERE turn_id = $1",
    )
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    corrupt_ended_attempt_disposition(&pool, fixture.attempt, "known_failure").await?;
    rewind_outbox_delivery_before(&pool, sequence).await?;

    assert!(matches!(
        OutboxDispatcher::new(pool.clone())
            .dispatch_next(|_| panic!("a completion with a mismatched attempt must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidTerminalEventCorrelation
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-032: a refused-turn event is dispatchable only while the
/// lifecycle's terminal attempt retains a refusal-compatible disposition.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv032_refused_dispatch_requires_exact_terminal_attempt() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xeb0;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Refused);
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Refused(RefusedModelCallTurnIdentities::new(
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    let sequence = sqlx::query_scalar(
        "SELECT event_sequence
           FROM turn_refused_outbox_event
          WHERE turn_id = $1",
    )
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    corrupt_ended_attempt_disposition(&pool, fixture.attempt, "turn_completed").await?;
    rewind_outbox_delivery_before(&pool, sequence).await?;

    assert!(matches!(
        OutboxDispatcher::new(pool.clone())
            .dispatch_next(|_| panic!("a refusal with a mismatched attempt must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidTerminalEventCorrelation
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S04 / S07 / INV-032 / INV-037: a reconciliation-required event is
/// dispatchable only while its terminal attempt retains exact ambiguity and
/// interrupt provenance.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_inv032_reconciliation_dispatch_requires_exact_terminal_attempt()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xec0;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 19,
                seed + 1,
                "stop before ambiguous result",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 20)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 21))),
        )
        .await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Ambiguous);
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Ambiguous(AmbiguousModelCallTurnIdentities::new(
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22)),
            )),
            |_| panic!("Ambiguous creates no pending-steering successors"),
        )
        .await?;

    let sequence = sqlx::query_scalar(
        "SELECT event_sequence
           FROM turn_reconciliation_required_outbox_event
          WHERE turn_id = $1",
    )
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    corrupt_ended_attempt_disposition(&pool, fixture.attempt, "cancelled").await?;
    rewind_outbox_delivery_before(&pool, sequence).await?;

    assert!(matches!(
        OutboxDispatcher::new(pool.clone())
            .dispatch_next(|_| {
                panic!("reconciliation with a mismatched attempt must not be offered")
            })
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidTerminalEventCorrelation
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-012 / INV-032: an accepted-input event is dispatchable only when
/// its content still matches the immutable accepting command.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv012_inv032_dispatcher_rejects_crosswired_accepted_content()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0xe72));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xe73));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0xe70, 0xe71, direct(0xe74)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0xe75,
                0xe71,
                "authoritative command content",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            accepted_input,
            Some(turn),
        )
        .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );

    sqlx::query("ALTER TABLE accepted_input DISABLE TRIGGER accepted_input_is_append_only")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE accepted_input
            SET content_text = 'cross-wired accepted content'
          WHERE accepted_input_id = $1",
    )
    .bind(accepted_input.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE accepted_input ENABLE TRIGGER accepted_input_is_append_only")
        .execute(&pool)
        .await?;

    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("cross-wired accepted content must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::MissingTypedRecord
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S34 / INV-008 / INV-012 / INV-046: a session system prompt lives on the
/// immutable defaults epoch. Creation stores it, the loaded current session
/// and process defaults read return it, replacement installs a promptless
/// successor without rewriting the prompted epoch, replay preserves the exact
/// recorded payloads, and model-call preparation reads the prompt through the
/// calling turn's frozen epoch rather than the current pointer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s34_inv008_inv012_inv046_system_prompt_rides_the_frozen_defaults_epoch()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0xa41));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xa42));
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xa43));
    let call = ModelCallId::from_uuid(Uuid::from_u128(0xa44));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0xa45));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(0xa46));
    let prompt = SessionSystemPrompt::try_new(String::from("exact session instructions"))
        .expect("test prompt is admissible");
    let prompted_defaults = SessionConfigurationDefaults::complete(
        ModelSelectionRequest::Direct(selection),
        signalbox_domain::DangerousToolAutoApproval::Disabled,
        Some(prompt.clone()),
    );
    let creation = CreateSession::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xa47)),
        SessionCreationProvenance::new(
            SessionCreationCause::UserInitiated,
            TranscriptAncestry::None,
        ),
        prompted_defaults.clone(),
    )
    .prepare(session)
    .expect("user-initiated creation without ancestry is preparable");
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    create_repository.handle(creation.clone()).await?;

    assert_eq!(
        create_repository.handle(creation.clone()).await?,
        CreateSessionHandlingOutcome::Applied(creation.applied_result())
    );
    let promptless_reuse = CreateSession::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xa47)),
        SessionCreationProvenance::new(
            SessionCreationCause::UserInitiated,
            TranscriptAncestry::None,
        ),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
    )
    .prepare(SessionId::from_uuid(Uuid::from_u128(0xa60)))
    .expect("user-initiated creation without ancestry is preparable");
    assert_eq!(
        create_repository.handle(promptless_reuse).await?,
        CreateSessionHandlingOutcome::ConflictingReuse {
            command_id: creation.command().command_id(),
        }
    );

    let loaded = SessionRepository::new(pool.clone())
        .load_session(session)
        .await?
        .expect("the prompted session exists");
    assert_eq!(
        loaded.current_configuration_defaults().defaults(),
        &prompted_defaults
    );

    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0xa48,
                0xa41,
                "prompted-epoch request",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xa49)),
            Some(turn),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(0xa4a),
            starting_frontier: Uuid::from_u128(0xa4b),
            initial_attempt: attempt.into_uuid(),
        },
    )
    .await?;
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one prompted fixture target forms a catalog");
    let call_repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let checkpoint = call_repository
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xa4c)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xa4d)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xa4e)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xa4f)),
                    TurnId::from_uuid(Uuid::from_u128(0xa50)),
                )
            },
        )
        .await?;
    let PrepareInitialModelCallOutcome::Checkpointed(checkpointed) = checkpoint else {
        panic!("the prompted initial call must checkpoint");
    };
    assert_eq!(checkpointed, call);

    // Replace the defaults with a promptless successor before the prepared
    // call resumes: the call still binds the origin's frozen prompted epoch.
    let promptless_defaults =
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection));
    let defaults_repository = ReplaceSessionDefaultsRepository::new(pool.clone());

    // A caller whose protocol cannot state the prompt member is refused
    // atomically under the compare-and-set lock while the current epoch
    // carries a prompt, and nothing — not even the command identity — is
    // recorded.
    let unstated = ReplaceSessionDefaults::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xa59)),
        session,
        SessionConfigurationDefaultsVersion::try_from_u64(1).expect("positive version"),
        promptless_defaults.clone(),
    );
    assert_eq!(
        defaults_repository
            .handle_where_prompt_member(unstated, PromptMemberStatement::Unstated)
            .await?,
        ReplaceSessionDefaultsHandlingOutcome::PromptRequiresStatedMember
    );
    let unstated_claimed: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM durable_command WHERE command_id = $1)")
            .bind(Uuid::from_u128(0xa59))
            .fetch_one(&pool)
            .await?;
    assert!(!unstated_claimed);

    let replacement = ReplaceSessionDefaults::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xa51)),
        session,
        SessionConfigurationDefaultsVersion::try_from_u64(1).expect("positive version"),
        promptless_defaults.clone(),
    );
    let ReplaceSessionDefaultsHandlingOutcome::Applied(applied) =
        defaults_repository.handle(replacement).await?
    else {
        panic!("the promptless replacement must apply");
    };
    assert_eq!(applied.installed().defaults(), &promptless_defaults);
    let prompted_reuse = ReplaceSessionDefaults::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xa51)),
        session,
        SessionConfigurationDefaultsVersion::try_from_u64(1).expect("positive version"),
        prompted_defaults.clone(),
    );
    assert_eq!(
        defaults_repository.handle(prompted_reuse).await?,
        ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse {
            command_id: DurableCommandId::from_uuid(Uuid::from_u128(0xa51)),
        }
    );

    let PrepareInitialModelCallOutcome::Ready { system_prompt, .. } = call_repository
        .prepare_initial_call(
            session,
            ModelCallId::from_uuid(Uuid::from_u128(0xa52)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xa53)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xa54)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xa55)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xa56)),
                    TurnId::from_uuid(Uuid::from_u128(0xa57)),
                )
            },
        )
        .await?
    else {
        panic!("the checkpointed prompted call must resume as ready");
    };
    assert_eq!(system_prompt.as_ref(), Some(&prompt));

    // The process defaults read selects the current promptless epoch, the
    // exact named prompted epoch, and types both absences.
    let read = ProcessReadRepository::new(pool.clone());
    let ProcessSessionDefaultsRead::Read(current) =
        read.read_session_defaults(session, None).await?
    else {
        panic!("the current defaults epoch must read");
    };
    assert_eq!(current.version(), applied.installed().version());
    assert_eq!(current.defaults(), &promptless_defaults);
    let ProcessSessionDefaultsRead::Read(named) = read
        .read_session_defaults(
            session,
            SessionConfigurationDefaultsVersion::try_from_u64(1),
        )
        .await?
    else {
        panic!("the named prompted epoch must read");
    };
    assert_eq!(
        named.version(),
        SessionConfigurationDefaultsVersion::first()
    );
    assert_eq!(named.defaults(), &prompted_defaults);
    assert_eq!(
        read.read_session_defaults(
            session,
            SessionConfigurationDefaultsVersion::try_from_u64(9),
        )
        .await?,
        ProcessSessionDefaultsRead::VersionNotFound
    );
    assert_eq!(
        read.read_session_defaults(SessionId::from_uuid(Uuid::from_u128(0xa5f)), None)
            .await?,
        ProcessSessionDefaultsRead::SessionNotFound
    );

    // Schema bounds: an installed epoch's prompt column admits at most
    // 1,048,576 UTF-8 bytes and never empty text, and epochs stay immutable.
    let oversized = "y".repeat(1_048_577);
    let oversized_insert = sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind, direct_model_selection_id,
             model_alias_id, dangerous_tool_auto_approval, system_prompt)
         VALUES ($1, 99, 'direct', $2, NULL, 'disabled', $3)",
    )
    .bind(session.into_uuid())
    .bind(selection.into_uuid())
    .bind(&oversized)
    .execute(&pool)
    .await
    .expect_err("an over-bound stored prompt is rejected");
    assert_eq!(
        oversized_insert
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    let empty_insert = sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind, direct_model_selection_id,
             model_alias_id, dangerous_tool_auto_approval, system_prompt)
         VALUES ($1, 99, 'direct', $2, NULL, 'disabled', '')",
    )
    .bind(session.into_uuid())
    .bind(selection.into_uuid())
    .execute(&pool)
    .await
    .expect_err("an empty stored prompt is rejected");
    assert_eq!(
        empty_insert
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    let rewrite = sqlx::query(
        "UPDATE session_defaults_version
         SET system_prompt = 'rewritten'
         WHERE session_id = $1 AND version = 1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("defaults epochs are append-only");
    assert_eq!(
        rewrite
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    // Command/defaults agreement: an applied replacement receipt whose prompt
    // digest disagrees with the installed epoch cannot commit.
    let mut disagreeing = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'replace_session_defaults', 3, now())",
    )
    .bind(Uuid::from_u128(0xa58))
    .execute(&mut *disagreeing)
    .await?;
    sqlx::query(
        "INSERT INTO replace_session_defaults_command
            (command_id, command_kind, storage_version, session_id,
             expected_current_version, model_selection_kind,
             direct_model_selection_id, model_alias_id,
             dangerous_tool_auto_approval, system_prompt, result_kind,
             rejection_kind, result_session_id, result_installed_version,
             result_expected_version, result_current_version)
         VALUES ($1, 'replace_session_defaults', 3, $2, 1, 'direct', $3, NULL,
                 'disabled', 'digest disagreement', 'applied', NULL, $2, 2,
                 NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xa58))
    .bind(session.into_uuid())
    .bind(selection.into_uuid())
    .execute(&mut *disagreeing)
    .await?;
    let disagreement = disagreeing
        .commit()
        .await
        .expect_err("a prompt-digest disagreement cannot commit");
    let sqlx::Error::Database(disagreement_error) = &disagreement else {
        panic!("unexpected digest-disagreement failure: {disagreement:?}");
    };
    assert_eq!(disagreement_error.code().as_deref(), Some("23503"));

    // A session that lost its current pointer fails a named historical read
    // closed as corruption rather than serving the surviving epoch.
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
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;
    let missing_pointer = read
        .read_session_defaults(
            session,
            SessionConfigurationDefaultsVersion::try_from_u64(1),
        )
        .await
        .expect_err("a named read must fail closed without a current pointer");
    let ProcessReadError::Corruption(ProcessReadCorruption::Missing(missing_field)) =
        missing_pointer
    else {
        panic!("the pointerless named read must be typed corruption");
    };
    assert_eq!(missing_field, "current defaults pointer");

    // A surviving pointer that names a missing epoch is equally corruption
    // for a named read of a different, existing epoch.
    sqlx::query(
        "ALTER TABLE session_current_defaults
         DROP CONSTRAINT session_current_defaults_version_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ($1, 77)",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;
    let dangling_pointer = read
        .read_session_defaults(
            session,
            SessionConfigurationDefaultsVersion::try_from_u64(1),
        )
        .await
        .expect_err("a named read must fail closed on a dangling current pointer");
    let ProcessReadError::Corruption(ProcessReadCorruption::Missing(dangling_field)) =
        dangling_pointer
    else {
        panic!("the dangling-pointer named read must be typed corruption");
    };
    assert_eq!(dangling_field, "current defaults epoch");

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / S03 / S08 / INV-009 / INV-014: the operation counted before
/// activation is the exact no-steering Prepared call committed with that
/// activation; steering accepted afterward remains pending for a later call.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_s03_s08_inv009_inv014_counted_activation_checkpoints_exact_call_before_steering()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0xcd01));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0xcd02));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(0xcd03));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            0xcd04,
            0xcd01,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    let origin = AcceptedInputId::from_uuid(Uuid::from_u128(0xcd05));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xcd06));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0xcd07,
                0xcd01,
                "counted origin",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            origin,
            Some(turn),
        )
        .await?;

    let activation = StartEligibleTurnRepository::new(pool.clone());
    let preview = activation
        .preview(
            session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcd08)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcd09)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xcd0a)),
                TurnAttemptId::from_uuid(Uuid::from_u128(0xcd0b)),
            ),
        )
        .await?
        .expect("the queued origin has one exact activation preview");
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one fixture target forms a catalog");
    let model_calls =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let counted_call = ModelCallId::from_uuid(Uuid::from_u128(0xcd0c));
    let counted_operation = model_calls
        .preview_activation_operation(preview.prepared(), counted_call)
        .await?
        .render(Box::new([]))?;
    let counted_entries = counted_operation
        .request()
        .frontier_entries()
        .map(signalbox_domain::SemanticTranscriptEntry::reference)
        .collect::<Vec<_>>();

    let committed = activation
        .commit_counted_preview(preview, counted_call, &model_calls)
        .await?;
    let CommitActivationPreviewOutcome::Activated(activated) = committed else {
        panic!("the unchanged counted activation must commit");
    };
    assert_eq!(activated.turn(), turn);

    let steering = input_with_delivery(
        0xcd0d,
        0xcd01,
        "later steering",
        DeliveryRequest::NextSafePoint {
            expected_active_turn: turn,
        },
    );
    let steering_outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            steering,
            AcceptedInputId::from_uuid(Uuid::from_u128(0xcd0e)),
            None,
        )
        .await?;
    assert!(matches!(
        steering_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));

    let ready = model_calls
        .prepare_initial_call(
            session,
            ModelCallId::from_uuid(Uuid::from_u128(0xcd0f)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcd10)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xcd11)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xcd12)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcd13)),
                    TurnId::from_uuid(Uuid::from_u128(0xcd14)),
                )
            },
        )
        .await?;
    let PrepareInitialModelCallOutcome::Ready { request, .. } = ready else {
        panic!("the atomically checkpointed counted call must resume Prepared");
    };
    assert_eq!(request.call().id(), counted_call);
    let prepared_entries = request
        .frontier_entries()
        .map(signalbox_domain::SemanticTranscriptEntry::reference)
        .collect::<Vec<_>>();
    assert_eq!(prepared_entries, counted_entries);
    let pending_steering: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM accepted_input
          WHERE session_id = $1
            AND disposition_kind = 'pending_steering'",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(pending_steering, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S03 / INV-015: deferred compaction evidence accepts successor ranges in
/// model-visible order even when the retained suffix physically precedes the
/// prior summary, while reverse correlation rejects an orphan summary.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s03_inv015_context_compaction_constraints_use_projected_successor_order()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0xcc01));
    let session_uuid = session.into_uuid();
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0xcc02));
    let mut create_service = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
    );
    create_service
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0xcc03)),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
        )?)
        .await?;

    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0xcc04));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xcc05));
    let mut submit_service = SubmitInputService::new(
        FixedSubmitInputIds::new([accepted_input], [turn]),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    submit_service
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0xcc06)),
            session,
            UserContent::try_text("synthetic compaction source".to_owned())
                .expect("fixture user content is valid"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        )?)
        .await?;

    let origin_entry = Uuid::from_u128(0xcc07);
    let initial_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xcc08));
    let mut activation_service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(origin_entry)],
            [initial_frontier],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xcc09))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    activation_service.execute(session).await?;

    let retained_suffix = Uuid::from_u128(0xcc0a);
    let root_source = Uuid::from_u128(0xcc0b);
    let mut startup = StartupScanService::new(
        FixedStartupScanIds::new(
            [SemanticTranscriptEntryId::from_uuid(retained_suffix)],
            [ContextFrontierId::from_uuid(root_source)],
        ),
        PostgresStartupScanRepository::new(pool.clone()),
    );
    startup.execute().await?;

    let root_call = Uuid::from_u128(0xcc0c);
    let root_summary = Uuid::from_u128(0xcc0d);
    let root_result = Uuid::from_u128(0xcc0e);
    let root_compaction = Uuid::from_u128(0xcc0f);
    let target = Uuid::from_u128(0xcc10);
    let mut root_transaction = pool.begin().await?;
    insert_completed_context_compaction_call(
        &mut root_transaction,
        root_call,
        session_uuid,
        selection.into_uuid(),
        target,
        root_source,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             context_summary_value, context_summary_producing_call_id,
             context_summary_first_source_session_id,
             context_summary_first_entry_id,
             context_summary_through_source_session_id,
             context_summary_through_entry_id)
         VALUES ($1, $2, 'context_summary', 'synthetic root summary', $3,
                 $1, $4, $1, $4)",
    )
    .bind(session_uuid)
    .bind(root_summary)
    .bind(root_call)
    .bind(origin_entry)
    .execute(&mut *root_transaction)
    .await?;
    insert_frontier(
        &mut root_transaction,
        session_uuid,
        root_result,
        Decimal::from(3),
        &[
            (Decimal::ONE, session_uuid, origin_entry),
            (Decimal::from(2), session_uuid, retained_suffix),
            (Decimal::from(3), session_uuid, root_summary),
        ],
    )
    .await?;
    sqlx::query(
        "INSERT INTO context_compaction
            (context_compaction_id, session_id, predecessor_compaction_id,
             source_frontier_id, result_frontier_id, producing_call_id,
             first_source_session_id, first_entry_id,
             through_source_session_id, through_entry_id, summary_entry_id)
         VALUES ($1, $2, NULL, $3, $4, $5, $2, $6, $2, $6, $7)",
    )
    .bind(root_compaction)
    .bind(session_uuid)
    .bind(root_source)
    .bind(root_result)
    .bind(root_call)
    .bind(origin_entry)
    .bind(root_summary)
    .execute(&mut *root_transaction)
    .await?;
    root_transaction.commit().await?;

    let successor_call = Uuid::from_u128(0xcc11);
    let successor_summary = Uuid::from_u128(0xcc12);
    let successor_result = Uuid::from_u128(0xcc13);
    let successor_compaction = Uuid::from_u128(0xcc14);
    let mut successor_transaction = pool.begin().await?;
    insert_completed_context_compaction_call(
        &mut successor_transaction,
        successor_call,
        session_uuid,
        selection.into_uuid(),
        target,
        root_result,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             context_summary_value, context_summary_producing_call_id,
             context_summary_first_source_session_id,
             context_summary_first_entry_id,
             context_summary_through_source_session_id,
             context_summary_through_entry_id)
         VALUES ($1, $2, 'context_summary', 'synthetic successor summary', $3,
                 $1, $4, $1, $5)",
    )
    .bind(session_uuid)
    .bind(successor_summary)
    .bind(successor_call)
    .bind(root_summary)
    .bind(retained_suffix)
    .execute(&mut *successor_transaction)
    .await?;
    insert_frontier(
        &mut successor_transaction,
        session_uuid,
        successor_result,
        Decimal::from(4),
        &[
            (Decimal::ONE, session_uuid, origin_entry),
            (Decimal::from(2), session_uuid, retained_suffix),
            (Decimal::from(3), session_uuid, root_summary),
            (Decimal::from(4), session_uuid, successor_summary),
        ],
    )
    .await?;
    sqlx::query(
        "INSERT INTO context_compaction
            (context_compaction_id, session_id, predecessor_compaction_id,
             source_frontier_id, result_frontier_id, producing_call_id,
             first_source_session_id, first_entry_id,
             through_source_session_id, through_entry_id, summary_entry_id)
         VALUES ($1, $2, $3, $4, $5, $6, $2, $7, $2, $8, $9)",
    )
    .bind(successor_compaction)
    .bind(session_uuid)
    .bind(root_compaction)
    .bind(root_result)
    .bind(successor_result)
    .bind(successor_call)
    .bind(root_summary)
    .bind(retained_suffix)
    .bind(successor_summary)
    .execute(&mut *successor_transaction)
    .await?;
    successor_transaction.commit().await?;

    let malformed_summary = Uuid::from_u128(0xcc17);
    let malformed_error = sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             context_summary_value, context_summary_producing_call_id,
             context_summary_first_source_session_id,
             context_summary_first_entry_id,
             context_summary_through_source_session_id,
             context_summary_through_entry_id,
             model_identity_defaults_version,
             model_identity_direct_selection_id)
         VALUES ($1, $2, 'context_summary', 'synthetic malformed summary', $3,
                 $1, $4, $1, $4, 1, $5)",
    )
    .bind(session_uuid)
    .bind(malformed_summary)
    .bind(successor_call)
    .bind(successor_summary)
    .bind(selection.into_uuid())
    .execute(&pool)
    .await
    .expect_err("summary payloads cannot carry model-identity fields");
    assert_eq!(
        malformed_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("semantic_transcript_entry_payload_shape")
    );

    let orphan_call = Uuid::from_u128(0xcc15);
    let orphan_summary = Uuid::from_u128(0xcc16);
    let mut orphan_transaction = pool.begin().await?;
    insert_completed_context_compaction_call(
        &mut orphan_transaction,
        orphan_call,
        session_uuid,
        selection.into_uuid(),
        target,
        successor_result,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             context_summary_value, context_summary_producing_call_id,
             context_summary_first_source_session_id,
             context_summary_first_entry_id,
             context_summary_through_source_session_id,
             context_summary_through_entry_id)
         VALUES ($1, $2, 'context_summary', 'synthetic orphan summary', $3,
                 $1, $4, $1, $4)",
    )
    .bind(session_uuid)
    .bind(orphan_summary)
    .bind(orphan_call)
    .bind(successor_summary)
    .execute(&mut *orphan_transaction)
    .await?;
    let orphan_error = orphan_transaction
        .commit()
        .await
        .expect_err("a summary without its exact compaction cannot commit");

    assert_eq!(
        orphan_error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ConcurrentPlanAppendDisposition {
    Appended,
    DuplicateAttempt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlanRepositoryErrorKind {
    InvalidAppendProvenance,
    InvalidEventSequence,
}

#[derive(Debug, sqlx::FromRow)]
struct PlanStorageSnapshot {
    event_count: i64,
    head_ordinal: Decimal,
}

static NEXT_PLAN_FIXTURE_SEED: AtomicU64 = AtomicU64::new(0xd100);
const PLAN_FIXTURE_SEED_STRIDE: u64 = 0x200;

fn plan_text(value: &str) -> PlanText {
    PlanText::try_new(String::from(value)).expect("the plan text fixture is valid")
}

fn create_plan_arguments(text: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "kind": "create",
        "text": text,
    }))
    .expect("the plan create arguments fixture serializes")
}

fn revise_plan_arguments(entry: PlanEntryId, text: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "entry_id": entry.as_u64(),
        "kind": "revise",
        "text": text,
    }))
    .expect("the plan revision arguments fixture serializes")
}

async fn authorize_plan_write(
    pool: &PgPool,
    arguments: &str,
) -> Result<(SessionId, PlanEventProvenance), Box<dyn Error>> {
    let seed =
        u128::from(NEXT_PLAN_FIXTURE_SEED.fetch_add(PLAN_FIXTURE_SEED_STRIDE, Ordering::Relaxed));
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(pool, seed, "plan_write", arguments).await?;
    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    tool_repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xd1)),
        )
        .await?;
    let attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xd2));
    tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            attempt,
            ToolEffectClass::ExternalEffect,
        )
        .await?
        .expect("the approved plan-write fixture prepares its physical attempt");
    let authorized = tool_repository
        .authorize_attempt(fixture.session, fixture.turn, attempt)
        .await?;
    Ok((
        fixture.session,
        PlanEventProvenance::from_invocation(authorized.correlation()),
    ))
}

fn expect_appended(outcome: PlanAppendOutcome) -> PlanEvent {
    match outcome {
        PlanAppendOutcome::Appended(event) => event,
        PlanAppendOutcome::Rejected(rejection) => {
            panic!("the plan append fixture was unexpectedly rejected: {rejection:?}")
        }
    }
}

fn plan_repository_error_kind(error: SessionPlanRepositoryError) -> PlanRepositoryErrorKind {
    match error {
        SessionPlanRepositoryError::InvalidAppendProvenance => {
            PlanRepositoryErrorKind::InvalidAppendProvenance
        }
        SessionPlanRepositoryError::Corruption(SessionPlanCorruption::InvalidEventSequence) => {
            PlanRepositoryErrorKind::InvalidEventSequence
        }
        other => panic!("unexpected plan repository error: {other:?}"),
    }
}

fn concurrent_append_disposition(
    result: Result<PlanAppendOutcome, SessionPlanRepositoryError>,
) -> ConcurrentPlanAppendDisposition {
    match result {
        Ok(PlanAppendOutcome::Appended(_)) => ConcurrentPlanAppendDisposition::Appended,
        Err(SessionPlanRepositoryError::DuplicateAppendAttempt) => {
            ConcurrentPlanAppendDisposition::DuplicateAttempt
        }
        Ok(PlanAppendOutcome::Rejected(rejection)) => {
            panic!("the competing append was unexpectedly rejected: {rejection:?}")
        }
        Err(error) => panic!("the competing append failed unexpectedly: {error:?}"),
    }
}

/// The first authoritative append advances the certified head and round-trips
/// through both the current projection and chronological history.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_append_and_read_round_trip_through_postgres() -> Result<(), Box<dyn Error>> {
    const REQUESTED_HISTORY_LIMIT: usize = 10;
    const EXPECTED_ENTRY_COUNT: usize = 1;
    const CREATED_TEXT: &str = "persist the durable plan";

    let (container, pool, _database_url) = migrated_postgres().await?;
    let arguments = create_plan_arguments(CREATED_TEXT);
    let (session, provenance) = authorize_plan_write(&pool, &arguments).await?;
    let repository = SessionPlanRepository::new(pool.clone());
    let event = expect_appended(
        repository
            .append(PlanAppendRequest::new(
                provenance,
                PlanEventDraft::Create {
                    text: plan_text(CREATED_TEXT),
                },
            ))
            .await?,
    );
    let page = repository
        .read(PlanReadRequest::new(
            session,
            None,
            Some(REQUESTED_HISTORY_LIMIT),
        ))
        .await?;
    let history = page
        .history()
        .expect("the requested plan history is returned");

    assert_eq!(page.completeness(), PlanPageCompleteness::Complete);
    let entry = page
        .entries()
        .first()
        .expect("the created entry is projected");

    assert_eq!(page.entries().len(), EXPECTED_ENTRY_COUNT);
    assert_eq!(entry.id().as_u64(), event.ordinal().as_u64());
    assert_eq!(entry.text().as_str(), CREATED_TEXT);
    assert_eq!(entry.status(), PlanStatus::Pending);
    assert_eq!(history.events(), std::slice::from_ref(&event));
    assert_eq!(history.completeness(), PlanPageCompleteness::Complete);

    pool.close().await;
    drop(container);
    Ok(())
}

/// A direct repository caller cannot turn a missing owning session into a
/// retryable database failure before provenance authentication.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_append_classifies_missing_session_as_invalid_provenance()
-> Result<(), Box<dyn Error>> {
    const CREATED_TEXT: &str = "refuse a missing session";
    const FIXTURE_SEED: u128 = 0xd000;

    let (container, pool, _database_url) = migrated_postgres().await?;
    let correlation = signalbox_domain::ToolAttemptDispatchCorrelation::reconstitute(
        signalbox_domain::ToolAttemptDispatchCorrelationReconstitutionInput {
            session: SessionId::from_uuid(Uuid::from_u128(FIXTURE_SEED)),
            turn: TurnId::from_uuid(Uuid::from_u128(FIXTURE_SEED + 1)),
            issuing_attempt: TurnAttemptId::from_uuid(Uuid::from_u128(FIXTURE_SEED + 2)),
            request: ToolRequestId::from_uuid(Uuid::from_u128(FIXTURE_SEED + 3)),
            attempt: ToolAttemptId::from_uuid(Uuid::from_u128(FIXTURE_SEED + 4)),
            generation: signalbox_domain::ToolDispatchGeneration::first(),
        },
    );
    let repository = SessionPlanRepository::new(pool.clone());
    let error = repository
        .append(PlanAppendRequest::new(
            PlanEventProvenance::from_invocation(correlation),
            PlanEventDraft::Create {
                text: plan_text(CREATED_TEXT),
            },
        ))
        .await
        .expect_err("the absent owning session rejects the append");

    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::InvalidAppendProvenance
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Durable provenance that no longer proves physical dispatch fails closed
/// before either current or requested-history evidence can be exposed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_read_rejects_prepared_attempt_provenance() -> Result<(), Box<dyn Error>> {
    const CREATED_TEXT: &str = "authenticate the dispatched attempt";
    const HISTORY_LIMIT: usize = 10;

    let (container, pool, _database_url) = migrated_postgres().await?;
    let arguments = create_plan_arguments(CREATED_TEXT);
    let (session, provenance) = authorize_plan_write(&pool, &arguments).await?;
    let repository = SessionPlanRepository::new(pool.clone());
    expect_appended(
        repository
            .append(PlanAppendRequest::new(
                provenance,
                PlanEventDraft::Create {
                    text: plan_text(CREATED_TEXT),
                },
            ))
            .await?,
    );

    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE tool_attempt SET state_kind = 'prepared' WHERE attempt_id = $1")
        .bind(provenance.correlation().attempt().into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;

    let authorized: bool = sqlx::query_scalar(
        "SELECT session_plan_event_has_authority(event)
           FROM session_plan_event AS event
          WHERE event.session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    let error = repository
        .read(PlanReadRequest::new(session, None, Some(HISTORY_LIMIT)))
        .await
        .expect_err("prepared provenance cannot authenticate current or history evidence");

    assert!(!authorized);
    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::InvalidEventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A revision naming no creation event returns its typed rejection without an
/// append or ordinal allocation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_append_rejects_unknown_entry() -> Result<(), Box<dyn Error>> {
    const MISSING_ENTRY_ID: u64 = 7;
    const REQUESTED_TEXT: &str = "replace a missing step";

    let (container, pool, _database_url) = migrated_postgres().await?;
    let missing_entry =
        PlanEntryId::try_from_u64(MISSING_ENTRY_ID).expect("the missing entry fixture is positive");
    let arguments = revise_plan_arguments(missing_entry, REQUESTED_TEXT);
    let (_, provenance) = authorize_plan_write(&pool, &arguments).await?;
    let repository = SessionPlanRepository::new(pool.clone());
    let rejection = repository
        .append(PlanAppendRequest::new(
            provenance,
            PlanEventDraft::Revise {
                entry: missing_entry,
                text: plan_text(REQUESTED_TEXT),
            },
        ))
        .await?;

    assert_eq!(
        rejection,
        PlanAppendOutcome::Rejected(PlanAppendRejection::UnknownEntry {
            entry: missing_entry,
        })
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Arguments that differ from the durable plan-write request cannot authorize
/// an append even when the physical attempt itself is in flight.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_append_rejects_untrusted_request() -> Result<(), Box<dyn Error>> {
    const MISSING_ENTRY_ID: u64 = 7;
    const REQUESTED_TEXT: &str = "replace a missing step";
    const MISMATCHED_TEXT: &str = "different request payload";

    let (container, pool, _database_url) = migrated_postgres().await?;
    let missing_entry =
        PlanEntryId::try_from_u64(MISSING_ENTRY_ID).expect("the missing entry fixture is positive");
    let arguments = revise_plan_arguments(missing_entry, REQUESTED_TEXT);
    let (_, provenance) = authorize_plan_write(&pool, &arguments).await?;
    let repository = SessionPlanRepository::new(pool.clone());
    let authority_error = repository
        .append(PlanAppendRequest::new(
            provenance,
            PlanEventDraft::Revise {
                entry: missing_entry,
                text: plan_text(MISMATCHED_TEXT),
            },
        ))
        .await
        .expect_err("mismatched request arguments cannot authorize an append");

    assert_eq!(
        plan_repository_error_kind(authority_error),
        PlanRepositoryErrorKind::InvalidAppendProvenance
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A physically present but malformed head is not projected as an honest empty
/// plan when required-column and trigger defenses are deliberately bypassed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_read_rejects_present_head_with_null_ordinal() -> Result<(), Box<dyn Error>> {
    const CREATED_TEXT: &str = "detect a corrupt plan head";

    let (container, pool, _database_url) = migrated_postgres().await?;
    let arguments = create_plan_arguments(CREATED_TEXT);
    let (session, provenance) = authorize_plan_write(&pool, &arguments).await?;
    let repository = SessionPlanRepository::new(pool.clone());
    expect_appended(
        repository
            .append(PlanAppendRequest::new(
                provenance,
                PlanEventDraft::Create {
                    text: plan_text(CREATED_TEXT),
                },
            ))
            .await?,
    );
    sqlx::query("ALTER TABLE session_plan_head DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE session_plan_head ALTER COLUMN event_ordinal DROP NOT NULL")
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE session_plan_head SET event_ordinal = NULL WHERE session_id = $1")
        .bind(session.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE session_plan_head ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let corruption = repository
        .read(PlanReadRequest::new(session, None, None))
        .await
        .expect_err("a present malformed plan head fails closed");

    assert_eq!(
        plan_repository_error_kind(corruption),
        PlanRepositoryErrorKind::InvalidEventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Competing submission of one physical plan-write attempt serializes to one
/// append and one typed duplicate-attempt failure without advancing twice.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_competing_append_uses_one_ordinal() -> Result<(), Box<dyn Error>> {
    const CREATED_TEXT: &str = "append once under contention";

    let (container, pool, _database_url) = migrated_postgres().await?;
    let arguments = create_plan_arguments(CREATED_TEXT);
    let (session, provenance) = authorize_plan_write(&pool, &arguments).await?;
    let request = PlanAppendRequest::new(
        provenance,
        PlanEventDraft::Create {
            text: plan_text(CREATED_TEXT),
        },
    );
    let first_repository = SessionPlanRepository::new(pool.clone());
    let second_repository = SessionPlanRepository::new(pool.clone());
    let (first, second) = tokio::join!(
        first_repository.append(request.clone()),
        second_repository.append(request),
    );
    let dispositions = HashSet::from([
        concurrent_append_disposition(first),
        concurrent_append_disposition(second),
    ]);
    let snapshot = sqlx::query_as::<_, PlanStorageSnapshot>(
        "SELECT count(event.event_ordinal) AS event_count,
                head.event_ordinal AS head_ordinal
           FROM session_plan_event AS event
           JOIN session_plan_head AS head ON head.session_id = event.session_id
          WHERE event.session_id = $1
          GROUP BY head.event_ordinal",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(
        dispositions,
        HashSet::from([
            ConcurrentPlanAppendDisposition::Appended,
            ConcurrentPlanAppendDisposition::DuplicateAttempt,
        ])
    );
    assert_eq!(snapshot.event_count, 1);
    assert_eq!(snapshot.head_ordinal, Decimal::ONE);

    pool.close().await;
    drop(container);
    Ok(())
}
