//! Feature-gated PostgreSQL coverage for migrations, durable invariants, and repository composition.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics, explicit fixture expectations, and impossible fixture branches; the workspace gate remains active for production targets"
)]

#[path = "../support/mod.rs"]
mod support;

mod approval_decisions;
mod attention;
mod convergence_sweep;
mod delegated_result_rereads;
mod delegation_schema;
mod delegation_transactions;
mod frontier_validation;
mod hub_fence;
mod lifecycle_measurement;
mod lifecycle_metrics;
mod model_call_execution_and_recovery;
mod model_call_usage_and_interrupts;
mod model_credentials_and_tool_batches;
mod outbox_dispatch_and_process_read;
mod ownership_seam_grants;
mod restart_recovery_and_submit;
mod search;
mod session_creation_and_submit;
mod session_deadline;
mod session_lifecycle;
mod session_lifecycle_commands;
mod session_live;
mod session_plan;
mod session_timeline;
mod tool_round_lifecycle;
mod turn_activation;
mod turn_liveness;
mod usage;
mod workspace_instruction_authority;
mod workspace_instruction_migration;
mod workspace_instructions;

use std::{
    collections::{BTreeSet, HashSet, VecDeque},
    error::Error,
    num::NonZeroU64,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_application::{
    ApprovalJudgeCompletionIdentities, AttachmentPreparationFailure, AuthorizeModelCallOutcome,
    AuthorizeModelCallTransaction, AutomaticReconciliationFailureKind,
    AutomaticReconciliationOperation, AutomaticReconciliationOutcome, ClassifyOperatorFailure,
    CommitModelCallObservationTransaction, CompiledTool, CompiledToolCatalog,
    CorrelatedDurableChildWait, CreateSessionError, CreateSessionOutcome, CreateSessionRequest,
    CreateSessionService, EligibilityNudge, EligibilityNudgeOutcome, EligibilitySweep,
    InProcessAttemptDispatchGate, LoadSessionService, ModelCallAuthorizationReread,
    ModelCallCredentialReference, ModelCallExecutionError, ModelCallExecutionIdGenerator,
    ModelCallExecutionOutcome, ModelCallExecutionService, ModelCallObservationCommitOutcome,
    ModelConversationMessage, OperatorFailureClass, PreparedModelCallFailureCause,
    PromptMemberStatement, ReplaceSessionDefaultsOutcome, ReplaceSessionDefaultsRequest,
    ReplaceSessionDefaultsService, RetainedModelCallObservationStatus,
    RetainedPreparedFailureStatus, ScriptedModelCallProvider, ScriptedModelCallStep,
    SessionIdGenerator, StartEligibleTurnIdGenerator, StartEligibleTurnOutcome,
    StartEligibleTurnService, StartupScanIdGenerator, StartupScanService,
    StartupScanSessionOutcome, SubmitInputIdGenerator, SubmitInputOutcome, SubmitInputRequest,
    SubmitInputService, ToolAttemptAuthorizationOutcome, ToolAttemptAuthorizationStatus,
    ToolCatalog, ToolDefinition, ToolInputSchema, ToolPreauthorization,
};
use signalbox_blob_store::{BlobObjectKey, BlobStoreName, ExpectedBlob};
use signalbox_domain::{
    AcceptedInputId, AcceptedInputStartingLineage, AcceptedInputTurnActivationIdentities,
    AcceptedInputTurnFailureIdentities, ActivatedAcceptedInputTurn, ActiveTurnPhase,
    AmbiguousModelCallTurnIdentities, AssistantResponsePart, AssistantText,
    AttachmentDisplayFilename, AttachmentKind, AuthorizedModelCall, BlobDigest,
    CancelledModelCallTurnIdentities, CompletedModelCallIdentities, ContextCompactionId,
    ContextCompactionTokenUsage, ContextFrontierId, CorrelatedModelCallTerminalObservation,
    CreateSession, CurrentToolAttemptState, CurrentTurnAttemptState, DecideToolRequest,
    DecideToolRequestResult, DeclaredMediaType, DelegateApprovalRecommendation,
    DelegationAwaitRequest, DelegationContent, DelegationMessageDirection, DelegationMessageId,
    DelegationMessageRequest, DelegationWaitMode, DeliveryRequest, DescendantTerminationScope,
    DirectModelSelection, DurableCommandId, FailedModelCallTurnIdentities, FastMode,
    FastModeOverlay, FastModeSupport, FrozenModelSelection, Goal, GoalCommandRejection,
    GoalCommandResult, GoalModelProvenance, GoalReport, GoalStatement, GoalUserAction,
    GoalUserCommand, GoalUserProvenance, InitialToolApproval, ModelAlias, ModelCallId,
    ModelCallTerminalIdentities, ModelCallTerminalObservation, ModelCallTerminalOutcome,
    ModelCapabilities, ModelCapabilityCatalog, ModelCapabilityDefinition, ModelSelectionOverride,
    ModelSelectionRequest, ModelSettingsOverlay, ModelSettingsPrecedence, ModelTargetCatalog,
    ModelTargetDefinition, NormalizedToolArguments, OverrideDeniedToolRequest,
    OverrideDeniedToolRequestRejectedResult, OverrideDeniedToolRequestResult,
    PerInputConfigurationChoices, PhysicalCancellationModelCallTurnIdentities,
    PreparedCreateSession, PreparedModelCallRequest, ProviderModelCallFailureCause,
    ProviderModelIdentity, ProviderReportedTokenUsage, ReasoningLevel, RecordedUserOverride,
    RefusedModelCallTurnIdentities, ReplaceSessionDefaults, ReplaceSessionDefaultsRejectedResult,
    ReplaceSessionDefaultsResult, ResolvedProviderTarget, SemanticTranscriptEntryId,
    SemanticTranscriptEntryRef, SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
    SessionCreationCause, SessionCreationProvenance, SessionId, SessionInputPosition,
    SessionOwnership, SessionPlacement, SessionPlacementPath, SessionSystemPrompt,
    SessionTemplateContentDigest, SessionTemplateName, SessionTemplateProvenance, SettingOverlay,
    StoppedToolResponsePartIdentity, StoppedToolRoundModelCallIdentities, SubmitInput,
    SubmitInputAppliedResult, SubmitInputReconstitutionFailure, SubmitInputRejectedResult,
    SubmitInputResult, ToolApprovalDecider, ToolApprovalDecision, ToolApprovalResolution,
    ToolAttemptCrashOutcome, ToolAttemptEnd, ToolAttemptId, ToolAttemptObservation,
    ToolBatchExecutionFailure, ToolCallProposal, ToolDecisionRationale, ToolDecisionSource,
    ToolDenialReason, ToolDispatchAuthority, ToolEffectClass, ToolExecutionError,
    ToolExecutionErrorDetail, ToolExecutionErrorKind, ToolName, ToolPermissionDefault,
    ToolRequestId, ToolResponsePartIdentity, ToolResultContent, ToolResultText,
    ToolRoundModelCallIdentities, ToolUsingAssistantResponse, TranscriptAncestry, TurnAttemptId,
    TurnConfigurationProvenance, TurnId, TurnTerminalCause, UserContent, UserContentPart,
};
use signalbox_persistence::{
    ModelCredentialFamilyCatalog,
    approval_judge::{
        AuthorizeApprovalJudgeOutcome, AuthorizedApprovalJudge, CompleteApprovalJudgeOutcome,
        FailedApprovalJudgeDisposition, PrepareApprovalJudgeOutcome, PreparedApprovalJudge,
    },
    automatic_reconciliation::{
        AutomaticReconciliationRepositoryError, PostgresAutomaticReconciliationRepository,
        RECONCILIATION_ACQUIRE_WAIT, RECONCILIATION_LOCK_WAIT, reconciliation_deadline,
    },
    blob::{BlobCatalogRepository, BlobReplicaRecord, BlobStoreBindingRecord},
    context_compaction::{
        ContextCompactionRepository, PrepareContextCompactionOutcome,
        PrepareContextCompactionRequest,
    },
    create_session::{
        CreateSessionCorruption, CreateSessionHandlingOutcome, CreateSessionRepository,
        CreateSessionRepositoryError,
    },
    create_session_from_imported_frontier::{
        ImportedSessionRepository, ImportedSessionRepositoryError,
    },
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels,
    goal::{GoalCommandHandlingOutcome, GoalRepository, GoalTransitionOutcome},
    goal_turn::GoalTurnCandidates,
    local_test_connection_options, migrate,
    model_execution::{
        CredentialPoolRuntimeAction, CredentialPoolRuntimeMember, CredentialPoolRuntimePolicy,
        ModelCallCorruption, ModelCallIdentityCollision, ModelCallRepositoryError,
        PostgresModelCallRepository, PrepareInitialModelCallOutcome, ToolContinuationUsageLimit,
    },
    outbox::{
        DispatchedDelegationOutcome, DispatchedDelegationPolicy, DispatchedDelegationProvenance,
        DispatchedDelegationReason, DispatchedDelegationUpdate, DispatchedDelegationWaitMode,
        DispatchedDelegationWake, DispatchedInjectionOutcome, DispatchedModelCallState,
        DispatchedOutboxEvent, DispatchedOutboxEventKind, DispatchedReconciliationOperation,
        DispatchedSessionCreation, DispatchedToolBatchState, DispatchedTurnTerminalDisposition,
        OutboxConsumer, OutboxConsumerReader, OutboxCorruption, OutboxDeliveryDecision,
        OutboxDispatchError, OutboxDispatchOutcome, OutboxDispatcher,
    },
    plan::{SessionPlanCorruption, SessionPlanRepository, SessionPlanRepositoryError},
    process_read::{
        ProcessCurrentModelCallState, ProcessFailedModelCallDisposition,
        ProcessModelCallInputTokenSemantics, ProcessModelCallRecoveryPrecondition,
        ProcessModelCallUsageProvenance, ProcessModelSelection,
        ProcessProviderModelCallFailureCause, ProcessReadCorruption, ProcessReadError,
        ProcessReadRepository, ProcessReconciliationOperation, ProcessSessionDefaultsRead,
        ProcessToolApproval, ProcessTranscriptEntry, ProcessTranscriptSnapshot, ProcessTurnState,
    },
    replace_session_defaults::{
        ReplaceSessionDefaultsCorruption, ReplaceSessionDefaultsHandlingOutcome,
        ReplaceSessionDefaultsRejectionOnlyOutcome, ReplaceSessionDefaultsRepository,
        ReplaceSessionDefaultsRepositoryError,
    },
    scheduler::PostgresEligibilitySweep,
    session::{SessionCorruption, SessionRepository, SessionRepositoryError},
    session_credentials::{
        SessionCredentialPin, SessionModelCredential, current_session_credential,
    },
    session_delegation::{
        DelegationOperationRejection, DelegationRequestExecutionState, ProcessDelegationOutcome,
        ProcessDelegationRequestRejection, RecordDelegationMessageOutcome,
        RecordDelegationWaitOutcome, RecordedDelegationMessage, RecordedDelegationWait,
        SessionDelegationCorruption, SessionDelegationRepository, SessionDelegationRepositoryError,
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
    workspace_instructions::CountedActivationInstructionEvidence,
};
use signalbox_tools_plan::{
    PlanAppendOutcome, PlanAppendRejection, PlanAppendRequest, PlanDependencyCycle, PlanEntryId,
    PlanEvent, PlanEventDraft, PlanEventKind, PlanEventProvenance, PlanPageCompleteness,
    PlanReadRequest, PlanReadiness, PlanStatus, PlanText,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

use support::{blocked_backends_reached, blocked_backends_reached_on};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_integration";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const APPROVAL_FIXTURE_SEED: u128 = 0x7e00;
const APPROVAL_JUDGE_SEED: u128 = 0x7e50;
const APPROVAL_COMMAND_SEED: u128 = 0x7e80;
const APPROVAL_NEXT_ATTEMPT_SEED: u128 = 0x7e81;
const APPROVAL_TOOL_NAME: &str = "current_time";
const APPROVAL_ARGUMENTS: &str = "{}";
const APPROVAL_PROPOSAL: &[(&str, &str)] = &[(APPROVAL_TOOL_NAME, APPROVAL_ARGUMENTS)];
const APPROVAL_RECOMMENDATION: &str = "approve";
const APPROVAL_DENIAL: &str = "deny";
const APPROVAL_JUDGE_CREDENTIAL: &str = "fixture-credential";
const APPROVAL_JUDGE_RATIONALE: &str = "fixture rationale";
const APPROVAL_JUDGE_ESTIMATED_PROVENANCE: &str = "estimated";
const APPROVAL_DELEGATE_SOURCE: &str = "delegate";
const APPROVAL_GOAL_STATEMENT: &str = "finish the commissioned approval task";

fn ready_approval_judge(outcome: PrepareApprovalJudgeOutcome) -> PreparedApprovalJudge {
    match outcome {
        PrepareApprovalJudgeOutcome::Ready(prepared) => *prepared,
        PrepareApprovalJudgeOutcome::NoWork
        | PrepareApprovalJudgeOutcome::InFlightAfterRestart(_) => {
            panic!("the delegated fixture prepares a fresh judge call")
        }
    }
}

fn authorized_approval_judge(outcome: AuthorizeApprovalJudgeOutcome) -> AuthorizedApprovalJudge {
    match outcome {
        AuthorizeApprovalJudgeOutcome::Authorized(authorization) => *authorization,
        AuthorizeApprovalJudgeOutcome::NoSend => {
            panic!("the fresh judge authorization permits one send")
        }
    }
}

fn applied_tool_decision(
    prepared: &signalbox_domain::PreparedDecideToolRequest,
) -> &signalbox_domain::DecideToolRequestAppliedResult {
    match prepared.result() {
        DecideToolRequestResult::Applied(applied) => applied,
        DecideToolRequestResult::Rejected(_) => {
            panic!("the escalated delegated request admits a user decision")
        }
    }
}

fn test_session_credential_pin() -> signalbox_persistence::SessionCredentialPin {
    signalbox_persistence::SessionCredentialPin::try_new(vec![
        signalbox_persistence::SessionModelCredential::new(
            "test-model-family",
            "test-model-primary",
        ),
    ])
    .expect("test credential pin is valid")
}

const RAW_DELEGATED_TASK: &str = "inspect delegated work";
const RAW_DELEGATED_MESSAGE: &str = "delegated status";
const DELEGATION_OUTBOX_FIXTURE_SEED: u128 = 0xd600;
const DELEGATION_HISTORY_FIXTURE_SEED: u128 = 0xd610;
const DELEGATION_SPAWN_PURPOSE_FIXTURE_SEED: u128 = 0xd620;
const DELEGATION_MESSAGE_PURPOSE_FIXTURE_SEED: u128 = 0xd630;
const DELEGATION_CASCADE_SOURCE_FIXTURE_SEED: u128 = 0xd640;
const DELEGATION_CASCADE_TARGET_FIXTURE_SEED: u128 = 0xd650;
const DELEGATION_RELATION_FIXTURE_SEED: u128 = 0xd700;
const DELEGATION_WAIT_FIXTURE_SEED: u128 = 0xd710;
const DELEGATION_LIFECYCLE_FIXTURE_SEED: u128 = 0xd720;
const DELEGATION_MESSAGE_UPDATE_FIXTURE_SEED: u128 = 0xd730;
const DELEGATION_RESULT_UPDATE_FIXTURE_SEED: u128 = 0xd740;
const DELEGATION_MESSAGE_WAKE_FIXTURE_SEED: u128 = 0xd750;
const DELEGATION_RESULT_WAKE_FIXTURE_SEED: u128 = 0xd760;
const DELEGATION_CHILD_STREAM_FIXTURE_SEED: u128 = 0xd770;
const DELEGATION_PARENT_STREAM_FIXTURE_SEED: u128 = 0xd780;
const DELEGATION_DUPLICATE_MESSAGE_FIXTURE_SEED: u128 = 0xd790;
const DELEGATION_REVERSE_INSERT_FIXTURE_SEED: u128 = 0xd7a0;
const DELEGATION_REPOSITORY_BACKGROUND_WAIT_SEED: u128 = 0x10000;
const DELEGATION_REPOSITORY_FOREGROUND_WAIT_SEED: u128 = 0x12000;
const DELEGATION_REPOSITORY_SECOND_BACKGROUND_WAIT_SEED: u128 = 0x14000;
const DELEGATION_REPOSITORY_MESSAGE_SEED: u128 = 0x16000;
const DELEGATION_REPOSITORY_MESSAGE_RACE_SECOND_SEED: u128 = 0x18000;
const DELEGATION_REPOSITORY_PREPARED_WAIT_SEED: u128 = 0x1a000;
const DELEGATION_REPOSITORY_APPROVED_WAIT_SEED: u128 = 0x1c000;
const DELEGATION_OUTBOX_COMMAND_ID: u128 = 0xdc00;
const DELEGATION_LIFECYCLE_COMMAND_ID: u128 = 0xdd10;
const DELEGATION_CASCADE_ROOT_COMMAND_ID: u128 = 0xe640;
const DELEGATION_WAIT_ONLY_OUTCOME_ORDINAL: i16 = 2;
const DELEGATION_AFTER_MESSAGE_OUTCOME_ORDINAL: i16 = 3;

struct RawDelegationPurposes<'a> {
    spawn_arguments: &'a str,
    message_arguments: &'a str,
    wait_mode: &'a str,
}

#[derive(Clone, Copy)]
struct RawDelegationFixture {
    parent: SessionId,
    parent_turn: TurnId,
    parent_attempt: TurnAttemptId,
    child: SessionId,
    initial_turn: TurnId,
    initial_semantic_entry: SemanticTranscriptEntryId,
    spawning_request: ToolRequestId,
    awaiting_request: ToolRequestId,
    message_request: ToolRequestId,
    message_id: Uuid,
}

#[derive(Clone, Copy)]
struct RawMessageRoute {
    stream: SessionId,
    sender: SessionId,
    recipient: SessionId,
}

async fn prepare_raw_delegation(
    pool: &PgPool,
    seed: u128,
    purposes: RawDelegationPurposes<'_>,
) -> Result<RawDelegationFixture, Box<dyn Error>> {
    let child = SessionId::from_uuid(Uuid::from_u128(seed + 0x200));
    let await_arguments = serde_json::json!({
        "child_session_id": child.as_uuid().to_string(),
        "mode": purposes.wait_mode,
    })
    .to_string();
    let (parent, _repository, _observation, requests) = checkpoint_confirmed_tool_batch(
        pool,
        seed,
        &[
            ("spawn_session", purposes.spawn_arguments),
            ("await_session", await_arguments.as_str()),
            ("send_session_message", purposes.message_arguments),
        ],
    )
    .await?;
    let [spawning_request, awaiting_request, message_request]: [ToolRequestId; 3] = requests
        .try_into()
        .expect("delegation fixture prepares exactly spawn, await, and message requests");
    let fixture = RawDelegationFixture {
        parent: parent.session,
        parent_turn: parent.turn,
        parent_attempt: parent.attempt,
        child,
        initial_turn: TurnId::from_uuid(Uuid::from_u128(seed + 0x201)),
        initial_semantic_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x202)),
        spawning_request,
        awaiting_request,
        message_request,
        message_id: Uuid::from_u128(seed + 0x400),
    };
    insert_raw_delegation_tool_receipts(pool, fixture, seed).await?;
    Ok(fixture)
}

async fn insert_raw_delegation_tool_receipts(
    pool: &PgPool,
    fixture: RawDelegationFixture,
    seed: u128,
) -> Result<(), sqlx::Error> {
    let spawn_result = serde_json::json!({
        "result": "session_spawned",
        "tool_request_id": fixture.spawning_request.as_uuid().to_string(),
        "child_session_id": fixture.child.as_uuid().to_string(),
        "relationship": { "kind": "background" },
    })
    .to_string();
    let await_result = serde_json::json!({
        "result": "session_await_registered",
        "tool_request_id": fixture.awaiting_request.as_uuid().to_string(),
        "child_session_id": fixture.child.as_uuid().to_string(),
        "mode": "background",
    })
    .to_string();
    let message_result = serde_json::json!({
        "result": "session_message_sent",
        "tool_request_id": fixture.message_request.as_uuid().to_string(),
        "message_id": fixture.message_id.to_string(),
        "direction": "parent_to_child",
        "ordinal": 2,
        "delivery_sequence": 1,
    })
    .to_string();
    let mut transaction = pool.begin().await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO tool_attempt
            (attempt_id, request_id, session_id, turn_id,
             issuing_turn_attempt_id, effect_class, dispatch_generation,
             state_kind, terminal_disposition_kind, result_content_kind,
             result_text)
         VALUES
            ($1, $2, $7, $8, $9, 'external_effect', 1,
             'terminal', 'completed', 'text', $10),
            ($3, $4, $7, $8, $9, 'effect_free', 1,
             'terminal', 'completed', 'text', $11),
            ($5, $6, $7, $8, $9, 'external_effect', 1,
             'terminal', 'completed', 'text', $12)",
    )
    .bind(Uuid::from_u128(seed + 0x300))
    .bind(fixture.spawning_request.into_uuid())
    .bind(Uuid::from_u128(seed + 0x301))
    .bind(fixture.awaiting_request.into_uuid())
    .bind(Uuid::from_u128(seed + 0x302))
    .bind(fixture.message_request.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(fixture.parent_turn.into_uuid())
    .bind(fixture.parent_attempt.into_uuid())
    .bind(spawn_result)
    .bind(await_result)
    .bind(message_result)
    .execute(&mut *transaction)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await
}

async fn prepare_canonical_raw_delegation(
    pool: &PgPool,
    seed: u128,
) -> Result<RawDelegationFixture, Box<dyn Error>> {
    let spawn_arguments = serde_json::json!({
        "relationship": { "kind": "background" },
        "task": RAW_DELEGATED_TASK,
    })
    .to_string();
    let child = SessionId::from_uuid(Uuid::from_u128(seed + 0x200));
    let message_arguments = serde_json::json!({
        "content": RAW_DELEGATED_MESSAGE,
        "peer_session_id": child.as_uuid().to_string(),
    })
    .to_string();
    prepare_raw_delegation(
        pool,
        seed,
        RawDelegationPurposes {
            spawn_arguments: &spawn_arguments,
            message_arguments: &message_arguments,
            wait_mode: "background",
        },
    )
    .await
}

async fn insert_raw_delegation(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO session
            (session_id, creation_cause, ancestry_kind, spawning_tool_request_id)
         VALUES ($1, 'delegated', 'none', $2)",
    )
    .bind(fixture.child.into_uuid())
    .bind(fixture.spawning_request.into_uuid())
    .execute(&mut *connection)
    .await?;
    // Placement owns the delegated default. The parent fixture is pathless, so
    // this test-only creation record preserves that exact existing placement.
    sqlx::query(
        "INSERT INTO session_placement_event
            (session_id, version, prior_version, event_kind, placement_path,
             root_global_read_intent, provenance_command_id, recorded_at)
         SELECT $1, 1, NULL, 'created', placement_path,
                root_global_read_intent, provenance_command_id,
                transaction_timestamp()
           FROM session_placement_event
          WHERE session_id = $2 AND version = 1",
    )
    .bind(fixture.child.into_uuid())
    .bind(fixture.parent.into_uuid())
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_placement(session_id, current_version)
         VALUES ($1, 1)",
    )
    .bind(fixture.child.into_uuid())
    .execute(&mut *connection)
    .await?;
    // Delegated creation is owned in production.
    insert_raw_session_lifecycle(&mut *connection, fixture.child.into_uuid(), true).await?;
    sqlx::query("INSERT INTO session_scheduler(session_id) VALUES ($1)")
        .bind(fixture.child.into_uuid())
        .execute(&mut *connection)
        .await?;
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind, direct_model_selection_id,
             model_alias_id, dangerous_tool_auto_approval, system_prompt)
         SELECT $1, 1, defaults.model_selection_kind,
                defaults.direct_model_selection_id, defaults.model_alias_id,
                defaults.dangerous_tool_auto_approval, defaults.system_prompt
           FROM turn_origin_effective_model_configuration($2, $3) AS frozen
           JOIN session_defaults_version AS defaults
             ON defaults.session_id = $3
            AND defaults.version = frozen.defaults_version",
    )
    .bind(fixture.child.into_uuid())
    .bind(fixture.parent_turn.into_uuid())
    .bind(fixture.parent.into_uuid())
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_defaults(session_id, current_version)
         VALUES ($1, 1)",
    )
    .bind(fixture.child.into_uuid())
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation
            (spawning_tool_request_id, parent_session_id, parent_turn_id,
             child_session_id, policy_kind)
         VALUES ($1, $2, $3, $4, 'background')",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(fixture.parent_turn.into_uuid())
    .bind(fixture.child.into_uuid())
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "WITH lifecycle AS (
            INSERT INTO turn_lifecycle
                (turn_id, session_id, origin_kind, origin_accepted_input_id,
                 acceptance_position, state_kind)
            VALUES ($1, $2, 'delegation', NULL, 1, 'queued')
            RETURNING turn_id
         ), semantic_entry AS (
            INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 delegated_task_spawning_tool_request_id)
            VALUES ($2, $7, 'delegated_task', $3)
            RETURNING semantic_entry_id
         )
         INSERT INTO session_delegation_initial_task
            (spawning_tool_request_id, child_session_id, turn_id, semantic_entry_id,
             admission_position, defaults_version,
             requested_model_kind, requested_direct_model_selection_id,
             frozen_model_kind, frozen_direct_model_selection_id, task_content)
         SELECT $3, $2, lifecycle.turn_id, semantic_entry.semantic_entry_id, 1, 1,
                'direct', frozen.direct_selection_id,
                'direct', frozen.direct_selection_id, $4
           FROM lifecycle
           CROSS JOIN semantic_entry
           CROSS JOIN turn_origin_effective_model_configuration($5, $6) AS frozen",
    )
    .bind(fixture.initial_turn.into_uuid())
    .bind(fixture.child.into_uuid())
    .bind(fixture.spawning_request.into_uuid())
    .bind(RAW_DELEGATED_TASK)
    .bind(fixture.parent_turn.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(fixture.initial_semantic_entry.into_uuid())
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             provenance_kind, provenance_session_id, provenance_turn_id,
             provenance_tool_request_id)
         VALUES ($1, 1, 'spawned', 'tool_request', $2, $3, $1)",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(fixture.parent_turn.into_uuid())
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn insert_raw_wait(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO session_delegation_wait
            (awaiting_tool_request_id, spawning_tool_request_id,
             parent_session_id, parent_turn_id, child_session_id, wait_mode)
         VALUES ($1, $2, $3, $4, $5, 'background')",
    )
    .bind(fixture.awaiting_request.into_uuid())
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(fixture.parent_turn.into_uuid())
    .bind(fixture.child.into_uuid())
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn insert_raw_wait_and_message(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
) -> Result<(), sqlx::Error> {
    insert_raw_wait(connection, fixture).await?;
    insert_raw_message(connection, fixture, "parent_to_child", fixture.child).await?;
    Ok(())
}

async fn insert_raw_failed_outcome(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
    turn: TurnId,
    event_ordinal: i16,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, reason_kind, provenance_kind,
             provenance_session_id, provenance_turn_id)
         VALUES ($1, $4, 'outcome_recorded', 'child_failed',
                 'child_execution_failed', 'child_turn', $2, $3)",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.child.into_uuid())
    .bind(turn.into_uuid())
    .bind(event_ordinal)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_child_result
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, content_text)
         VALUES ($1, $2, 'outcome_recorded', 'child_failed', NULL)",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(event_ordinal)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "WITH pending AS (
            INSERT INTO session_pending_delivery
                (recipient_session_id, delivery_sequence, delivery_kind)
            VALUES ($1, 1, 'background_result')
         )
         INSERT INTO session_child_result_delivery
            (awaiting_tool_request_id, spawning_tool_request_id,
             parent_session_id, delivery_sequence, delivery_kind)
         VALUES ($2, $3, $1, 1, 'background_result')",
    )
    .bind(fixture.parent.into_uuid())
    .bind(fixture.awaiting_request.into_uuid())
    .bind(fixture.spawning_request.into_uuid())
    .execute(&mut *connection)
    .await?;
    Ok(())
}

struct RawDelegationUpdate<'a> {
    session: SessionId,
    kind: &'a str,
    awaiting_request: Option<Uuid>,
    event_ordinal: Option<i64>,
    event_kind: Option<&'a str>,
    result_request: Option<Uuid>,
    message_id: Option<Uuid>,
}

async fn append_raw_delegation_update(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
    update: RawDelegationUpdate<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event(event_kind, storage_version, session_id)
            VALUES ('delegation_update', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_update_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             update_kind, spawning_tool_request_id, child_session_id,
             policy_kind, on_parent_stopped, on_parent_cancelled,
             awaiting_tool_request_id, wait_mode,
             delegation_event_ordinal, delegation_event_kind,
             outcome_kind, reason_kind, provenance_kind,
             provenance_session_id, provenance_turn_id, provenance_command_id,
             result_spawning_request_id, message_id,
             sender_session_id, recipient_session_id, message_ordinal,
             content_text)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $2, $3,
                CASE WHEN $2 = 'session_message' THEN NULL ELSE $9 END,
                CASE WHEN $2 = 'child_spawned' THEN 'background' END,
                NULL, NULL, $4,
                CASE WHEN $2 = 'child_waiting' THEN 'background' END,
                $5, $6,
                CASE WHEN $2 IN (
                    'child_lifecycle_disposition', 'child_result'
                ) THEN 'child_failed' END,
                CASE WHEN $2 IN (
                    'child_lifecycle_disposition', 'child_result'
                ) THEN 'child_execution_failed' END,
                CASE WHEN $2 IN (
                    'child_lifecycle_disposition', 'child_result'
                ) THEN 'child_turn' END,
                CASE WHEN $2 IN (
                    'child_lifecycle_disposition', 'child_result'
                ) THEN $9 END,
                CASE WHEN $2 IN (
                    'child_lifecycle_disposition', 'child_result'
                ) THEN $10 END,
                NULL, $7, $8,
                CASE WHEN $2 = 'session_message' THEN $11 END,
                CASE WHEN $2 = 'session_message' THEN $9 END,
                CASE WHEN $2 = 'session_message' THEN 2 END,
                CASE WHEN $2 = 'session_message' THEN $12 END
           FROM header",
    )
    .bind(update.session.into_uuid())
    .bind(update.kind)
    .bind(fixture.spawning_request.into_uuid())
    .bind(update.awaiting_request)
    .bind(update.event_ordinal)
    .bind(update.event_kind)
    .bind(update.result_request)
    .bind(update.message_id)
    .bind(fixture.child.into_uuid())
    .bind(fixture.initial_turn.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(RAW_DELEGATED_MESSAGE)
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_raw_parent_lifecycle_update(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
    command_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "ALTER TABLE durable_command
         DISABLE TRIGGER durable_command_requires_typed_record",
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'goal', 1, transaction_timestamp(), 'operator')",
    )
    .bind(command_id)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "ALTER TABLE durable_command
         ENABLE TRIGGER durable_command_requires_typed_record",
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "WITH event AS (
            INSERT INTO session_delegation_event
                (spawning_tool_request_id, event_ordinal, event_kind,
                 outcome_kind, reason_kind, provenance_kind,
                 provenance_session_id, provenance_turn_id,
                 provenance_command_id)
            VALUES ($1, 4, 'outcome_recorded', 'already_terminal',
                    'parent_stopped_parent_and_descendants',
                    'parent_turn_command', $2, $3, $4)
            RETURNING event_ordinal, event_kind, outcome_kind, reason_kind,
                      provenance_kind, provenance_session_id,
                      provenance_turn_id, provenance_command_id
         ), header AS (
            INSERT INTO delegation_outbox_event(event_kind, storage_version, session_id)
            VALUES ('delegation_update', 1, $2)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_update_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             update_kind, spawning_tool_request_id, child_session_id,
             delegation_event_ordinal, delegation_event_kind,
             outcome_kind, reason_kind, provenance_kind,
             provenance_session_id, provenance_turn_id,
             provenance_command_id)
         SELECT header.event_sequence, header.event_kind,
                header.storage_version, header.session_id,
                'child_lifecycle_disposition', $1, $5,
                event.event_ordinal, event.event_kind,
                event.outcome_kind, event.reason_kind, event.provenance_kind,
                event.provenance_session_id, event.provenance_turn_id,
                event.provenance_command_id
           FROM header CROSS JOIN event",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(fixture.parent_turn.into_uuid())
    .bind(command_id)
    .bind(fixture.child.into_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

async fn insert_raw_parent_lifecycle_without_update(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
    command_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "ALTER TABLE durable_command
         DISABLE TRIGGER durable_command_requires_typed_record",
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'goal', 1, transaction_timestamp(), 'operator')",
    )
    .bind(command_id)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "ALTER TABLE durable_command
         ENABLE TRIGGER durable_command_requires_typed_record",
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, reason_kind, provenance_kind,
             provenance_session_id, provenance_turn_id,
             provenance_command_id)
         VALUES ($1, 2, 'outcome_recorded', 'continue_running',
                 'parent_stopped_parent_and_descendants',
                 'parent_turn_command', $2, $3, $4)",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(fixture.parent_turn.into_uuid())
    .bind(command_id)
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_raw_result_wake(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event(event_kind, storage_version, session_id)
            VALUES ('delegation_wake', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_wake_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             spawning_tool_request_id, subject_kind,
             result_spawning_request_id, message_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $2, 'result', $2, NULL FROM header",
    )
    .bind(fixture.parent.into_uuid())
    .bind(fixture.spawning_request.into_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_raw_message_wake(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
    recipient: SessionId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event(event_kind, storage_version, session_id)
            VALUES ('delegation_wake', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_wake_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             spawning_tool_request_id, subject_kind,
             result_spawning_request_id, message_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $2, 'message', NULL, $3 FROM header",
    )
    .bind(recipient.into_uuid())
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.message_id)
    .execute(connection)
    .await?;
    Ok(())
}

async fn insert_raw_delegation_with_update(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
) -> Result<(), sqlx::Error> {
    insert_raw_delegation(connection, fixture).await?;
    append_raw_delegation_update(
        connection,
        fixture,
        RawDelegationUpdate {
            session: fixture.parent,
            kind: "child_spawned",
            awaiting_request: None,
            event_ordinal: Some(1),
            event_kind: Some("spawned"),
            result_request: None,
            message_id: None,
        },
    )
    .await
}

async fn prepare_delegation_repository_fixture(
    pool: &PgPool,
    seed: u128,
    wait_mode: &str,
) -> Result<RawDelegationFixture, Box<dyn Error>> {
    let spawn_arguments = serde_json::json!({
        "relationship": { "kind": "background" },
        "task": RAW_DELEGATED_TASK,
    })
    .to_string();
    let child = SessionId::from_uuid(Uuid::from_u128(seed + 0x200));
    let message_arguments = serde_json::json!({
        "content": RAW_DELEGATED_MESSAGE,
        "peer_session_id": child.as_uuid().to_string(),
    })
    .to_string();
    let await_arguments = serde_json::json!({
        "child_session_id": child.as_uuid().to_string(),
        "mode": wait_mode,
    })
    .to_string();
    let (parent, _repository, _observation, requests) = checkpoint_tool_batch_with_approval(
        pool,
        seed,
        &[
            ("spawn_session", spawn_arguments.as_str()),
            ("await_session", await_arguments.as_str()),
            ("send_session_message", message_arguments.as_str()),
        ],
        InitialToolApproval::PolicyAuto,
    )
    .await?;
    let fixture = RawDelegationFixture {
        parent: parent.session,
        parent_turn: parent.turn,
        parent_attempt: TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xc1)),
        child,
        initial_turn: TurnId::from_uuid(Uuid::from_u128(seed + 0x201)),
        initial_semantic_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x202)),
        spawning_request: requests[0],
        awaiting_request: requests[1],
        message_request: requests[2],
        message_id: Uuid::from_u128(seed + 0x400),
    };
    insert_raw_delegation_tool_receipts(pool, fixture, seed).await?;
    let mut transaction = pool.begin().await?;
    insert_raw_delegation_with_update(&mut transaction, fixture).await?;
    transaction.commit().await?;
    Ok(fixture)
}

async fn repository_wait_dispatch(
    pool: &PgPool,
    fixture: RawDelegationFixture,
    seed: u128,
) -> Result<ToolDispatchAuthority, Box<dyn Error>> {
    prepare_repository_wait_attempt(pool, fixture, seed).await?;
    PostgresToolLoopRepository::new(pool.clone())
        .authorize_attempt(
            fixture.parent,
            fixture.parent_turn,
            ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0x301)),
        )
        .await
        .map_err(Into::into)
}

async fn prepare_repository_wait_attempt(
    pool: &PgPool,
    fixture: RawDelegationFixture,
    seed: u128,
) -> Result<(), Box<dyn Error>> {
    let message_attempt = Uuid::from_u128(seed + 0x302);
    let wait_attempt = Uuid::from_u128(seed + 0x301);
    let mut transaction = pool.begin().await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("DELETE FROM tool_attempt WHERE attempt_id IN ($1, $2)")
        .bind(wait_attempt)
        .bind(message_attempt)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    repository
        .prepare_next_attempt(
            fixture.parent,
            fixture.parent_turn,
            ToolAttemptId::from_uuid(wait_attempt),
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the wait fixture prepares its next attempt");
    Ok(())
}

async fn remove_repository_pending_attempts(pool: &PgPool, seed: u128) -> Result<(), sqlx::Error> {
    let wait_attempt = Uuid::from_u128(seed + 0x301);
    let message_attempt = Uuid::from_u128(seed + 0x302);
    let mut transaction = pool.begin().await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("DELETE FROM tool_attempt WHERE attempt_id IN ($1, $2)")
        .bind(wait_attempt)
        .bind(message_attempt)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await
}

async fn repository_message_dispatch(
    pool: &PgPool,
    fixture: RawDelegationFixture,
    seed: u128,
) -> Result<ToolDispatchAuthority, Box<dyn Error>> {
    let message_attempt = Uuid::from_u128(seed + 0x302);
    let mut transaction = pool.begin().await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("DELETE FROM tool_attempt WHERE attempt_id = $1")
        .bind(message_attempt)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    repository
        .prepare_next_attempt(
            fixture.parent,
            fixture.parent_turn,
            ToolAttemptId::from_uuid(message_attempt),
            ToolEffectClass::ExternalEffect,
        )
        .await?
        .expect("the message fixture prepares its next attempt");
    repository
        .authorize_attempt(
            fixture.parent,
            fixture.parent_turn,
            ToolAttemptId::from_uuid(message_attempt),
        )
        .await
        .map_err(Into::into)
}

fn recorded_wait(outcome: RecordDelegationWaitOutcome) -> RecordedDelegationWait {
    match outcome {
        RecordDelegationWaitOutcome::Recorded(recorded) => recorded,
        RecordDelegationWaitOutcome::Rejected(rejection) => {
            panic!("fixture wait was rejected: {rejection:?}")
        }
        RecordDelegationWaitOutcome::DurablyRejected(rejection) => {
            panic!("fixture wait was durably rejected: {rejection:?}")
        }
    }
}

fn process_wait(
    outcome: ProcessDelegationOutcome<(DelegationAwaitRequest, RecordedDelegationWait)>,
) -> (DelegationAwaitRequest, RecordedDelegationWait) {
    match outcome {
        ProcessDelegationOutcome::Applied(recorded) => recorded,
        ProcessDelegationOutcome::InvalidRequest | ProcessDelegationOutcome::Rejected(_) => {
            panic!("the exact stored await request reconstitutes")
        }
    }
}

fn process_message(
    outcome: ProcessDelegationOutcome<(DelegationMessageRequest, Box<RecordedDelegationMessage>)>,
) -> (DelegationMessageRequest, Box<RecordedDelegationMessage>) {
    match outcome {
        ProcessDelegationOutcome::Applied(recorded) => recorded,
        ProcessDelegationOutcome::InvalidRequest | ProcessDelegationOutcome::Rejected(_) => {
            panic!("the exact stored message request reconstitutes")
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum MessageRaceDisposition {
    IdentityCollision,
    Recorded,
}

fn message_race_disposition(outcome: RecordDelegationMessageOutcome) -> MessageRaceDisposition {
    match outcome {
        RecordDelegationMessageOutcome::Recorded(_) => MessageRaceDisposition::Recorded,
        RecordDelegationMessageOutcome::Rejected(
            DelegationOperationRejection::MessageIdentityCollision,
        ) => MessageRaceDisposition::IdentityCollision,
        RecordDelegationMessageOutcome::Rejected(
            DelegationOperationRejection::RelationshipNotFound,
        ) => panic!("message race lost its relationship"),
        RecordDelegationMessageOutcome::Rejected(DelegationOperationRejection::StaleDispatch {
            ..
        }) => {
            panic!("message race lost its dispatch")
        }
        RecordDelegationMessageOutcome::Rejected(
            DelegationOperationRejection::DeliverySequenceExhausted,
        ) => panic!("message race exhausted its delivery sequence"),
        RecordDelegationMessageOutcome::Rejected(DelegationOperationRejection::Transition {
            ..
        }) => {
            panic!("message race reached an invalid transition")
        }
        RecordDelegationMessageOutcome::DurablyRejected(_) => {
            panic!("issued message race cannot observe a process-owned durable rejection")
        }
    }
}

fn delegation_corruption(error: SessionDelegationRepositoryError) -> SessionDelegationCorruption {
    match error {
        SessionDelegationRepositoryError::Corruption(corruption) => corruption,
        SessionDelegationRepositoryError::Database(_) => {
            panic!("expected typed delegation corruption, found database failure")
        }
        SessionDelegationRepositoryError::CommitAmbiguous(_) => {
            panic!("expected typed delegation corruption, found commit ambiguity")
        }
        SessionDelegationRepositoryError::ToolLoop(_) => {
            panic!("expected typed delegation corruption, found tool-loop failure")
        }
        SessionDelegationRepositoryError::InvalidTransition(_) => {
            panic!("expected typed delegation corruption, found invalid transition")
        }
    }
}

#[derive(sqlx::FromRow)]
struct BackgroundWaitAtomicityEvidence {
    wait_count: i64,
    update_count: i64,
    completed_attempt_count: i64,
    result_text: String,
}

#[derive(sqlx::FromRow)]
struct ForegroundWaitAtomicityEvidence {
    active_phase: String,
    current_attempt: Option<Uuid>,
    attempt_state: String,
    terminal_disposition: String,
    issuing_disposition: String,
    update_count: i64,
}

#[derive(sqlx::FromRow)]
struct MessageAtomicityEvidence {
    event_count: i64,
    message_count: i64,
    delivery_count: i64,
    update_count: i64,
    wake_count: i64,
    completed_attempt_count: i64,
}

#[derive(sqlx::FromRow)]
struct DelegatedResultMaterializationEvidence {
    outcome_kind: String,
    content_text: Option<String>,
    reason_kind: String,
    provenance_kind: String,
    terminal_disposition_kind: String,
    parent_update_count: i64,
    parent_wake_count: i64,
}

async fn insert_raw_wait_and_message_with_delivery(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
) -> Result<(), sqlx::Error> {
    insert_raw_wait_with_update(connection, fixture).await?;
    insert_raw_message(connection, fixture, "parent_to_child", fixture.child).await?;
    append_raw_delegation_update(
        connection,
        fixture,
        RawDelegationUpdate {
            session: fixture.child,
            kind: "session_message",
            awaiting_request: None,
            event_ordinal: None,
            event_kind: None,
            result_request: None,
            message_id: Some(fixture.message_id),
        },
    )
    .await?;
    append_raw_message_wake(connection, fixture, fixture.child).await
}

async fn insert_raw_wait_with_update(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
) -> Result<(), sqlx::Error> {
    insert_raw_wait(connection, fixture).await?;
    append_raw_delegation_update(
        connection,
        fixture,
        RawDelegationUpdate {
            session: fixture.parent,
            kind: "child_waiting",
            awaiting_request: Some(fixture.awaiting_request.into_uuid()),
            event_ordinal: None,
            event_kind: None,
            result_request: None,
            message_id: None,
        },
    )
    .await
}

async fn insert_raw_message(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
    direction: &str,
    recipient: SessionId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH event AS (
            INSERT INTO session_delegation_event
                (spawning_tool_request_id, event_ordinal, event_kind,
                 provenance_kind, provenance_session_id, provenance_turn_id,
                 provenance_tool_request_id)
            VALUES ($1, 2, 'message_delivered', 'tool_request', $2, $3, $4)
            RETURNING spawning_tool_request_id, event_ordinal, event_kind
         )
         INSERT INTO session_message
            (message_id, spawning_tool_request_id, event_ordinal,
             event_kind, direction, content_text)
         SELECT $5, spawning_tool_request_id, event_ordinal, event_kind, $6, $7
           FROM event",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(fixture.parent_turn.into_uuid())
    .bind(fixture.message_request.into_uuid())
    .bind(fixture.message_id)
    .bind(direction)
    .bind(RAW_DELEGATED_MESSAGE)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "WITH pending AS (
            INSERT INTO session_pending_delivery
                (recipient_session_id, delivery_sequence, delivery_kind)
            VALUES ($1, 1, 'message')
         )
         INSERT INTO session_message_delivery
            (message_id, spawning_tool_request_id, recipient_session_id,
             delivery_sequence, delivery_kind)
         VALUES ($2, $3, $1, 1, 'message')",
    )
    .bind(recipient.into_uuid())
    .bind(fixture.message_id)
    .bind(fixture.spawning_request.into_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_raw_message_update(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
    route: RawMessageRoute,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event(event_kind, storage_version, session_id)
            VALUES ('delegation_update', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_update_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             update_kind, spawning_tool_request_id, message_id,
             sender_session_id, recipient_session_id, message_ordinal,
             content_text)
         SELECT event_sequence, event_kind, storage_version, session_id,
                'session_message', $2, $3, $4, $5, 2, $6
           FROM header",
    )
    .bind(route.stream.into_uuid())
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.message_id)
    .bind(route.sender.into_uuid())
    .bind(route.recipient.into_uuid())
    .bind(RAW_DELEGATED_MESSAGE)
    .execute(connection)
    .await?;
    Ok(())
}

fn constraint_name(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(|error| error.constraint())
}

async fn prepared_complete_delegation_outbox(
    seed: u128,
) -> Result<(ContainerAsync<Postgres>, PgPool, RawDelegationFixture), Box<dyn Error>> {
    let spawn_arguments = serde_json::json!({
        "relationship": { "kind": "background" },
        "task": RAW_DELEGATED_TASK,
    })
    .to_string();
    let child = SessionId::from_uuid(Uuid::from_u128(seed + 0x200));
    let message_arguments = serde_json::json!({
        "content": RAW_DELEGATED_MESSAGE,
        "peer_session_id": child.as_uuid().to_string(),
    })
    .to_string();
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = prepare_raw_delegation(
        &pool,
        seed,
        RawDelegationPurposes {
            spawn_arguments: &spawn_arguments,
            message_arguments: &message_arguments,
            wait_mode: "background",
        },
    )
    .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "ALTER TABLE session_delegation_event
         DISABLE TRIGGER session_delegation_event_requires_payload",
    )
    .execute(&mut *transaction)
    .await?;
    insert_raw_delegation(&mut transaction, fixture).await?;
    insert_raw_wait_and_message(&mut transaction, fixture).await?;
    insert_raw_failed_outcome(
        &mut transaction,
        fixture,
        fixture.initial_turn,
        DELEGATION_AFTER_MESSAGE_OUTCOME_ORDINAL,
    )
    .await?;
    append_raw_delegation_update(
        &mut transaction,
        fixture,
        RawDelegationUpdate {
            session: fixture.parent,
            kind: "child_spawned",
            awaiting_request: None,
            event_ordinal: Some(1),
            event_kind: Some("spawned"),
            result_request: None,
            message_id: None,
        },
    )
    .await?;
    append_raw_delegation_update(
        &mut transaction,
        fixture,
        RawDelegationUpdate {
            session: fixture.parent,
            kind: "child_waiting",
            awaiting_request: Some(fixture.awaiting_request.into_uuid()),
            event_ordinal: None,
            event_kind: None,
            result_request: None,
            message_id: None,
        },
    )
    .await?;
    append_raw_parent_lifecycle_update(
        &mut transaction,
        fixture,
        Uuid::from_u128(DELEGATION_OUTBOX_COMMAND_ID),
    )
    .await?;
    append_raw_delegation_update(
        &mut transaction,
        fixture,
        RawDelegationUpdate {
            session: fixture.parent,
            kind: "child_result",
            awaiting_request: None,
            event_ordinal: None,
            event_kind: None,
            result_request: Some(fixture.spawning_request.into_uuid()),
            message_id: None,
        },
    )
    .await?;
    append_raw_delegation_update(
        &mut transaction,
        fixture,
        RawDelegationUpdate {
            session: fixture.child,
            kind: "session_message",
            awaiting_request: None,
            event_ordinal: None,
            event_kind: None,
            result_request: None,
            message_id: Some(fixture.message_id),
        },
    )
    .await?;
    append_raw_message_wake(&mut transaction, fixture, fixture.child).await?;
    append_raw_result_wake(&mut transaction, fixture).await?;
    transaction.commit().await?;
    Ok((container, pool, fixture))
}

async fn prepared_recipient_delivery_fixture(
    seed: u128,
) -> Result<(ContainerAsync<Postgres>, PgPool, RawDelegationFixture), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = prepare_canonical_raw_delegation(&pool, seed).await?;
    let mut base = pool.begin().await?;
    insert_raw_delegation_with_update(&mut base, fixture).await?;
    base.commit().await?;
    Ok((container, pool, fixture))
}

async fn prepared_delegation_with_wait(
    seed: u128,
) -> Result<(ContainerAsync<Postgres>, PgPool, RawDelegationFixture), Box<dyn Error>> {
    let (container, pool, fixture) = prepared_recipient_delivery_fixture(seed).await?;
    let mut setup = pool.begin().await?;
    insert_raw_wait_with_update(&mut setup, fixture).await?;
    setup.commit().await?;
    Ok((container, pool, fixture))
}

fn model_credential_reference() -> ModelCallCredentialReference {
    ModelCallCredentialReference::new("fixture-provider-primary")
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
        None,
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
    let (_, _, _, _, _, provider, _, _, _, _) = service.into_parts();
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
        } => (
            *accepted_input,
            content
                .single_text()
                .expect("the fixture has exactly one text part")
                .as_str(),
        ),
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
        } => (
            *accepted_input,
            *turn,
            content
                .single_text()
                .expect("fixture process content is one text part")
                .as_str(),
        ),
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

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct ApprovalJudgeDurableState {
    prepared_judge_exists: bool,
    decision_exists: bool,
    active_wait_exists: bool,
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct ApprovalJudgeDecisionDurableState {
    prepared_judge_exists: bool,
    decision_exists: bool,
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct AutomaticApprovalEventState {
    decision_exists: bool,
    decided_event_exists: bool,
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct AppliedApprovalJudgeProjection {
    judge_state: String,
    recommendation: String,
    decision_source: String,
    delegate_model_selection_id: Uuid,
    delegate_model_call_id: Uuid,
    rationale: String,
    active_phase: String,
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct DeniedApprovalJudgeProjection {
    judge_state: String,
    recommendation: String,
    decision_kind: String,
    decision_source: String,
    denial_reason: Option<String>,
    rationale: String,
    active_phase: String,
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct EscalatedApprovalJudgeProjection {
    judge_state: String,
    recommendation: String,
    decision_exists: bool,
    active_phase: String,
    approval_tool_request_id: Uuid,
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
            DispatchedOutboxEventKind::TurnTerminal {
                turn,
                disposition: DispatchedTurnTerminalDisposition::ReconciliationRequired {
                    operation: DispatchedReconciliationOperation::ToolAttempt(attempt),
                    ..
                },
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

async fn record_empty_instruction_manifest(
    pool: &PgPool,
    session: SessionId,
) -> Result<(), Box<dyn Error>> {
    let turn = TurnId::from_uuid(
        sqlx::query_scalar::<_, Uuid>(
            "SELECT turn_id FROM turn_lifecycle WHERE session_id = $1 AND state_kind = 'active'",
        )
        .bind(session.into_uuid())
        .fetch_one(pool)
        .await?,
    );
    let snapshot = signalbox_application::discover_workspace_instructions(Vec::new());
    let manifest = signalbox_domain::TurnInstructionManifest::empty_turn_start(
        signalbox_domain::TurnInstructionManifestId::from_uuid(turn.into_uuid()),
        session,
        turn,
    );
    let outcome =
        signalbox_persistence::workspace_instructions::WorkspaceInstructionRepository::new(
            pool.clone(),
        )
        .record_turn_start(
            signalbox_domain::InstructionDiscoveryId::from_uuid(turn.into_uuid()),
            manifest,
            &snapshot,
            || unreachable!("an empty discovery needs no bundle identity"),
        )
        .await?;
    assert!(!matches!(
        outcome,
        signalbox_persistence::workspace_instructions::RecordTurnInstructionSnapshotOutcome::TurnUnavailable
    ));
    Ok(())
}

async fn unmigrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool, String), Box<dyn Error>>
{
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
             credential_reference, usage_input_includes_cache_tokens, state_kind)
         VALUES ($1, $2, $3, $4, $5, 'fixture-compaction-profile', false, 'prepared')",
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
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'compact_session', 1, transaction_timestamp(), 'operator')",
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
             credential_reference, usage_input_includes_cache_tokens, state_kind)
         VALUES ($1, $2, $3, $4, $5, 'synthetic-compaction-credential',
                 true, 'prepared')",
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
         SET state_kind = 'in_flight', in_flight_at = clock_timestamp()
         WHERE model_call_id = $1",
    )
    .bind(call)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE context_compaction_model_call
         SET state_kind = 'terminal', terminal_at = clock_timestamp(),
             terminal_disposition_kind = 'completed',
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
    record_empty_instruction_manifest(pool, SessionId::from_uuid(activation.session)).await?;
    match *activated {
        signalbox_domain::ActivatedTurn::Accepted(activated) => Ok(Box::new(activated)),
        signalbox_domain::ActivatedTurn::Delegated(_) => {
            panic!("accepted-input fixture activated a delegated turn")
        }
    }
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
    command_id: DurableCommandId,
    session: SessionId,
    active_origin_input: AcceptedInputId,
    delivery: DeliveryRequest,
    turn: Option<u128>,
) -> Result<(SubmitInputRepositoryError, i64), Box<dyn Error>> {
    let command = input_with_delivery(
        command_id.into_uuid().as_u128(),
        session.into_uuid().as_u128(),
        "colliding active origin",
        delivery,
    );
    let error = repository
        .handle(
            command,
            active_origin_input,
            turn.map(|value| TurnId::from_uuid(Uuid::from_u128(value))),
        )
        .await
        .expect_err("new acceptance cannot reuse the active origin identity");
    let claimed = sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
        .bind(command_id.into_uuid())
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
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        SessionConfigurationDefaults::new(selection),
    )
    .prepare(SessionId::from_uuid(Uuid::from_u128(session)))
    .expect("user-initiated creation without ancestry is preparable")
}

fn prepared_with_low_reasoning(
    command: u128,
    session: u128,
    selection: DirectModelSelection,
) -> PreparedCreateSession {
    let precedence = ModelSettingsPrecedence::new(
        ModelSettingsOverlay::inherit_all(),
        ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::Low),
            FastModeOverlay::Inherit,
            SettingOverlay::Inherit,
        ),
        ModelSettingsOverlay::inherit_all(),
        ModelSettingsOverlay::inherit_all(),
    );
    let settings = ModelCapabilities::new(
        BTreeSet::from([ReasoningLevel::Low]),
        FastModeSupport::Unsupported,
        BTreeSet::new(),
    )
    .validate_precedence(selection, precedence)
    .expect("the fixture capability admits low reasoning");
    let defaults = SessionConfigurationDefaults::complete_with_model_settings(
        ModelSelectionRequest::Direct(selection),
        signalbox_domain::DangerousToolAutoApproval::Disabled,
        None,
        settings,
    )
    .expect("the fixture settings belong to the direct selection");
    CreateSession::new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        defaults,
    )
    .prepare(SessionId::from_uuid(Uuid::from_u128(session)))
    .expect("user-initiated creation without ancestry is preparable")
}

#[derive(Clone, Copy, Debug)]
struct RecordedSettingsReplacement {
    session: SessionId,
    command: DurableCommandId,
}

async fn record_settings_replacement_fixture(
    pool: &PgPool,
    seed: u128,
) -> Result<RecordedSettingsReplacement, Box<dyn Error>> {
    let session = SessionId::from_uuid(Uuid::from_u128(seed + 1));
    let initial_selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 2));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared_with_low_reasoning(
            seed + 3,
            seed + 1,
            initial_selection,
        ))
        .await?;
    let installed_selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 4));
    let caller_settings = ModelSettingsOverlay::new(
        SettingOverlay::ProviderDefault,
        FastModeOverlay::Inherit,
        SettingOverlay::Inherit,
    );
    let installed_settings = ModelCapabilities::new(
        BTreeSet::new(),
        FastModeSupport::Unsupported,
        BTreeSet::new(),
    )
    .validate_precedence(
        installed_selection,
        ModelSettingsPrecedence::new(
            ModelSettingsOverlay::inherit_all(),
            caller_settings,
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        ),
    )
    .expect("the provider-default fixture is valid for the replacement");
    let installed_defaults = SessionConfigurationDefaults::complete_with_model_settings(
        ModelSelectionRequest::Direct(installed_selection),
        signalbox_domain::DangerousToolAutoApproval::Disabled,
        None,
        installed_settings,
    )
    .expect("the replacement settings belong to the direct selection");
    let command = DurableCommandId::from_uuid(Uuid::from_u128(seed + 5));
    let replacement = ReplaceSessionDefaults::with_model_settings(
        command,
        session,
        SessionConfigurationDefaultsVersion::try_from_u64(1)
            .expect("the fixture version is positive"),
        installed_defaults,
        caller_settings,
    );
    ReplaceSessionDefaultsRepository::new(pool.clone())
        .handle(replacement)
        .await?;
    Ok(RecordedSettingsReplacement { session, command })
}

async fn record_settings_replacement(
    pool: &PgPool,
    seed: u128,
) -> Result<SessionId, Box<dyn Error>> {
    Ok(record_settings_replacement_fixture(pool, seed)
        .await?
        .session)
}

async fn append_session_created_test_event(
    connection: &mut PgConnection,
    session: Uuid,
) -> Result<Decimal, sqlx::Error> {
    let sequence = sqlx::query_scalar(
        "INSERT INTO outbox_event
            (event_kind, storage_version, session_id)
         VALUES ('session_created', 2, $1)
         RETURNING event_sequence",
    )
    .bind(session)
    .fetch_one(&mut *connection)
    .await?;

    sqlx::query(
        "INSERT INTO session_created_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             creation_cause, owned)
         VALUES ($1, 'session_created', 2, $2, 'interactive', false)",
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

type CancellationDispatch = (
    SessionId,
    TurnId,
    SemanticTranscriptEntryId,
    ContextFrontierId,
);

async fn drain_cancellation_dispatches(
    pool: &PgPool,
) -> Result<Vec<CancellationDispatch>, OutboxDispatchError> {
    let mut cancellations = Vec::new();
    drain_outbox(pool, |event| {
        let (
            Some(session),
            DispatchedOutboxEventKind::TurnTerminal {
                turn,
                disposition:
                    DispatchedTurnTerminalDisposition::Cancelled {
                        cancellation_entry,
                        terminal_frontier,
                    },
            },
        ) = (event.session(), event.kind())
        else {
            return;
        };
        cancellations.push((session, *turn, *cancellation_entry, *terminal_frontier));
    })
    .await?;
    Ok(cancellations)
}

type ReconciliationDispatch = (
    SessionId,
    TurnId,
    DispatchedReconciliationOperation,
    ContextFrontierId,
);

async fn drain_reconciliation_dispatches(
    pool: &PgPool,
) -> Result<Vec<ReconciliationDispatch>, OutboxDispatchError> {
    let mut reconciliations = Vec::new();
    drain_outbox(pool, |event| {
        let (
            Some(session),
            DispatchedOutboxEventKind::TurnTerminal {
                turn,
                disposition:
                    DispatchedTurnTerminalDisposition::ReconciliationRequired {
                        operation,
                        terminal_frontier,
                    },
            },
        ) = (event.session(), event.kind())
        else {
            return;
        };
        reconciliations.push((session, *turn, *operation, *terminal_frontier));
    })
    .await?;
    Ok(reconciliations)
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
        "ALTER TABLE outbox_consumer_cursor
         DISABLE TRIGGER outbox_consumer_cursor_advances_prefix",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE outbox_consumer_cursor
            SET delivered_through = $1 - 1,
                last_delivery_xid = pg_current_xact_id()
          WHERE consumer_name = 'process_protocol'",
    )
    .bind(sequence)
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_consumer_cursor
         ENABLE TRIGGER outbox_consumer_cursor_advances_prefix",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Inserts the complete pre-outbox session record family for allocator tests.
///
/// The command and model identities derive from the one session seed.
/// Gives one raw-SQL session fixture the lifecycle row every session owns.
///
/// `session` carries a deferred foreign key to its satellite, so a fixture
/// that inserts a session row by statement owes the same row a creation path
/// writes — including the ownership its creation cause establishes, since an
/// owned fixture that recorded itself unmonitored would run with a posture
/// production never produces.
async fn insert_raw_session_lifecycle(
    connection: &mut sqlx::PgConnection,
    session: Uuid,
    owned: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO session_lifecycle
            (session_id, state_kind, owned, start_gate_held, actor_kind)
         VALUES ($1, 'created', $2, false, 'operator')",
    )
    .bind(session)
    .bind(owned)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_ownership_event
            (session_id, event_ordinal, transition_kind, owned_after, actor_kind)
         VALUES ($1, 1, $2, $3, 'operator')",
    )
    .bind(session)
    .bind(if owned {
        "created_owned"
    } else {
        "created_unmonitored"
    })
    .bind(owned)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn insert_outbox_session_fixture(
    pool: &PgPool,
    session_seed: u128,
) -> Result<Uuid, sqlx::Error> {
    insert_outbox_session_fixture_with_creation_cause(pool, session_seed, "interactive").await
}

/// Seeds the outbox session fixture with an explicit `creation_cause`.
///
/// `202608110001_user_role_storage_vocabulary` renamed the stored value, so a
/// fixture seeding a pool held before it must write the retired spelling: the
/// `CHECK` in force there admits nothing else, and the insert fails with
/// `23514` before the migration under test runs.
async fn insert_outbox_session_fixture_with_creation_cause(
    pool: &PgPool,
    session_seed: u128,
    creation_cause: &str,
) -> Result<Uuid, sqlx::Error> {
    let session = Uuid::from_u128(session_seed);
    let command = Uuid::from_u128(session_seed ^ 0x1000);
    let model = outbox_session_fixture_model_selection(session_seed);
    let mut transaction = pool.begin().await?;

    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'create_session', 1, transaction_timestamp(), 'operator')",
    )
    .bind(command)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES ($1, $2, 'none')",
    )
    .bind(session)
    .bind(creation_cause)
    .execute(&mut *transaction)
    .await?;
    insert_raw_session_lifecycle(&mut transaction, session, false).await?;
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
             result_kind, created_session_id, start_gate, ownership)
         VALUES (
            $1, 'create_session', 1,
            $4, 'none', 1,
            'direct', $2, NULL,
            'applied', $3, 'open', 'unmonitored'
         )",
    )
    .bind(command)
    .bind(model.into_uuid())
    .bind(session)
    .bind(creation_cause)
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

/// Attachment byte ceiling the restart fixture admits, well above its one-byte blob.
// numeric-bound: tunable - bounds fixture attachment admission
const FIXTURE_ATTACHMENT_MAXIMUM_BYTES: u64 = 1_024;

/// Builds one submit input whose content optionally carries a blob attachment.
///
/// The attachment part travels with the parent command so persistence writes
/// both in the creating transaction, which content-part immutability requires.
fn start_input_with_attachment(
    command: u128,
    session: u128,
    content: &str,
    expected: u64,
    model: ModelSelectionOverride,
    attachment: Option<BlobDigest>,
) -> SubmitInput {
    let mut parts =
        vec![UserContentPart::try_text(content.to_owned()).expect("test content is admitted")];
    if let Some(digest) = attachment {
        parts.push(UserContentPart::Attachment {
            digest,
            kind: AttachmentKind::File,
            media_type: DeclaredMediaType::try_new(String::from("application/octet-stream"))
                .expect("the fixture media type is admitted"),
            display_filename: Some(
                AttachmentDisplayFilename::try_new(String::from("fixture.bin"))
                    .expect("the fixture basename is admitted"),
            ),
        });
    }
    SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        SessionId::from_uuid(Uuid::from_u128(session)),
        UserContent::try_parts(parts).expect("the fixture parts form admitted content"),
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

fn user_content(value: &str) -> UserContent {
    UserContent::try_text(value.to_owned()).expect("test content is admitted")
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
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         SELECT $1, command_kind, storage_version, transaction_timestamp(), 'operator'
           FROM durable_command
          WHERE command_id = $2",
    )
    .bind(command_id)
    .bind(source_command_id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             delivery_kind, descendant_scope,
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
             delivery_kind, descendant_scope,
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
    sqlx::query(
        "INSERT INTO submit_input_command_content_part
            (command_id, position, part_kind, text_value, blob_digest,
             attachment_kind, declared_media_type, display_filename)
         SELECT $1, position, part_kind, text_value, blob_digest,
                attachment_kind, declared_media_type, display_filename
           FROM submit_input_command_content_part
          WHERE command_id = $2",
    )
    .bind(command_id)
    .bind(source_command_id)
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
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         SELECT $1, command_kind, storage_version, transaction_timestamp(), 'operator'
           FROM durable_command
          WHERE command_id = $2",
    )
    .bind(command_id)
    .bind(source_command_id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             delivery_kind, descendant_scope,
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
             delivery_kind, descendant_scope,
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
    sqlx::query(
        "INSERT INTO submit_input_command_content_part
            (command_id, position, part_kind, text_value, blob_digest,
             attachment_kind, declared_media_type, display_filename)
         SELECT $1, position, part_kind, text_value, blob_digest,
                attachment_kind, declared_media_type, display_filename
           FROM submit_input_command_content_part
          WHERE command_id = $2",
    )
    .bind(command_id)
    .bind(source_command_id)
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
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         SELECT $1, command_kind, storage_version, transaction_timestamp(), 'operator'
           FROM durable_command
          WHERE command_id = $2",
    )
    .bind(command_id)
    .bind(source_command_id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             delivery_kind, descendant_scope,
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
             'interrupt', 'parent_alone',
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
    sqlx::query(
        "INSERT INTO submit_input_command_content_part
            (command_id, position, part_kind, text_value, blob_digest,
             attachment_kind, declared_media_type, display_filename)
         SELECT $1, position, part_kind, text_value, blob_digest,
                attachment_kind, declared_media_type, display_filename
           FROM submit_input_command_content_part
          WHERE command_id = $2",
    )
    .bind(command_id)
    .bind(source_command_id)
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

#[track_caller]
fn applied_session(outcome: CreateSessionOutcome) -> SessionId {
    let CreateSessionOutcome::Applied(applied) = outcome else {
        panic!("fixture session creation must apply")
    };
    applied.session()
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

    fn next_closure_decision_command_id(&mut self) -> DurableCommandId {
        DurableCommandId::from_uuid(next_test_submit_uuid())
    }

    fn next_closure_turn_attempt_id(&mut self) -> TurnAttemptId {
        TurnAttemptId::from_uuid(next_test_submit_uuid())
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
    checkpoint_restart_model_call_with_attachment(pool, seed, authorize, None).await
}

async fn checkpoint_restart_model_call_with_attachment(
    pool: &PgPool,
    seed: u128,
    authorize: bool,
    attachment: Option<BlobDigest>,
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
    let submit_repository = match attachment {
        Some(_) => SubmitInputRepository::new(pool.clone())
            .with_attachment_maximum_bytes(FIXTURE_ATTACHMENT_MAXIMUM_BYTES),
        None => SubmitInputRepository::new(pool.clone()),
    };
    submit_repository
        .handle(
            start_input_with_attachment(
                seed + 8,
                seed + 1,
                "restart-classification request",
                1,
                ModelSelectionOverride::UseSessionDefault,
                attachment,
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
    authorize_checkpointed_model_call_with_attachment(pool, seed, None).await
}

async fn authorize_checkpointed_model_call_with_attachment(
    pool: &PgPool,
    seed: u128,
    attachment: Option<BlobDigest>,
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        AuthorizedModelCall,
    ),
    Box<dyn Error>,
> {
    let fixture =
        checkpoint_restart_model_call_with_attachment(pool, seed, false, attachment).await?;
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
    checkpoint_confirmed_tool_round_with_attachment(pool, seed, tool_name, arguments, None).await
}

async fn checkpoint_confirmed_tool_round_with_attachment(
    pool: &PgPool,
    seed: u128,
    tool_name: &str,
    arguments: &str,
    attachment: Option<BlobDigest>,
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
        checkpoint_confirmed_tool_batch_with_attachment(
            pool,
            seed,
            &[(tool_name, arguments)],
            attachment,
        )
        .await?;
    let [request] = requests.as_slice() else {
        panic!("the single-proposal fixture returns one request")
    };
    Ok((fixture, repository, observation, *request))
}

async fn checkpoint_confirmed_tool_round_with_usage(
    pool: &PgPool,
    seed: u128,
    tool_name: &str,
    arguments: &str,
    usage: ProviderReportedTokenUsage,
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
        checkpoint_tool_batch_with_approval_and_usage(
            pool,
            seed,
            &[(tool_name, arguments)],
            InitialToolApproval::Confirm,
            usage,
        )
        .await?;
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

async fn checkpoint_confirmed_tool_batch_with_attachment(
    pool: &PgPool,
    seed: u128,
    proposals: &[(&str, &str)],
    attachment: Option<BlobDigest>,
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        CorrelatedModelCallTerminalObservation,
        Vec<signalbox_domain::ToolRequestId>,
    ),
    Box<dyn Error>,
> {
    checkpoint_tool_batch_with_approval_and_attachment(
        pool,
        seed,
        proposals,
        InitialToolApproval::Confirm,
        attachment,
    )
    .await
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
    checkpoint_tool_batch_with_approval_and_attachment(
        pool,
        seed,
        proposals,
        initial_approval,
        None,
    )
    .await
}

async fn checkpoint_suppressed_tool_round(
    pool: &PgPool,
    seed: u128,
    tool_name: &str,
) -> Result<(RestartModelCallFixture, signalbox_domain::ToolRequestId), Box<dyn Error>> {
    let (fixture, model_repository, authorized) =
        authorize_checkpointed_model_call(pool, seed).await?;
    let request = signalbox_domain::ToolRequestId::from_uuid(Uuid::from_u128(seed + 0x40));
    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            ToolCallProposal::suppressed(
                ToolName::try_new(String::from(tool_name)).expect("valid fixture tool name"),
            ),
        )])
        .expect("the suppressed proposal forms one inert tool response");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools { response });
    let outcome = model_repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![ToolResponsePartIdentity::tool_call(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x80)),
                    request,
                    InitialToolApproval::RuntimeSafetyDeny,
                )],
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xc0)),
                Some(TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xc1))),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    let ModelCallTerminalOutcome::ToolRound(round) = outcome else {
        panic!("the suppressed fixture reaches an automatically denied tool round")
    };
    assert!(matches!(
        round.next_phase(),
        ActiveTurnPhase::Running { .. }
    ));
    Ok((fixture, request))
}

async fn checkpoint_tool_batch_with_approval_and_attachment(
    pool: &PgPool,
    seed: u128,
    proposals: &[(&str, &str)],
    initial_approval: InitialToolApproval,
    attachment: Option<BlobDigest>,
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        CorrelatedModelCallTerminalObservation,
        Vec<signalbox_domain::ToolRequestId>,
    ),
    Box<dyn Error>,
> {
    checkpoint_tool_batch_with_approval_and_usage_and_attachment(
        pool,
        seed,
        proposals,
        initial_approval,
        ProviderReportedTokenUsage::unreported(),
        attachment,
    )
    .await
}

async fn checkpoint_tool_batch_with_approval_and_usage(
    pool: &PgPool,
    seed: u128,
    proposals: &[(&str, &str)],
    initial_approval: InitialToolApproval,
    usage: ProviderReportedTokenUsage,
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        CorrelatedModelCallTerminalObservation,
        Vec<signalbox_domain::ToolRequestId>,
    ),
    Box<dyn Error>,
> {
    checkpoint_tool_batch_with_approval_and_usage_and_attachment(
        pool,
        seed,
        proposals,
        initial_approval,
        usage,
        None,
    )
    .await
}

async fn checkpoint_tool_batch_with_approval_and_usage_and_attachment(
    pool: &PgPool,
    seed: u128,
    proposals: &[(&str, &str)],
    initial_approval: InitialToolApproval,
    usage: ProviderReportedTokenUsage,
    attachment: Option<BlobDigest>,
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
        authorize_checkpointed_model_call_with_attachment(pool, seed, attachment).await?;
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
        .bind_terminal_observation_with_usage(
            ModelCallTerminalObservation::CompletedWithTools { response },
            usage,
        );
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
    let ModelCallTerminalOutcome::ToolRound(round) = outcome else {
        panic!("the fixture reaches a tool round")
    };
    if initial_approval.requires_decision() {
        assert_eq!(
            round.next_phase(),
            &ActiveTurnPhase::AwaitingApproval {
                request: requests[0],
            }
        );
    }
    Ok((fixture, model_repository, observation, requests))
}

/// Commissions `APPROVAL_GOAL_STATEMENT` on an existing fixture session and
/// returns the exact statement it commissioned.
///
/// The commission schedules its own queued goal turn from the seed, which
/// leaves whatever turn the fixture already activated alone.
async fn commission_fixture_session_goal(
    pool: &PgPool,
    session: SessionId,
    seed: u128,
) -> Result<GoalStatement, Box<dyn Error>> {
    let statement = GoalStatement::try_new(String::from(APPROVAL_GOAL_STATEMENT))
        .expect("the fixture goal statement is admitted");
    let outcome = GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(seed)),
                session,
                GoalUserAction::Attach(statement.clone()),
            ),
            Some(GoalTurnCandidates::new(
                AcceptedInputId::from_uuid(Uuid::from_u128(seed + 1)),
                TurnId::from_uuid(Uuid::from_u128(seed + 2)),
            )),
            |_| None,
        )
        .await?;
    assert_goal_command_applied(outcome);
    Ok(statement)
}

/// Stops a fixture session's goal as a user stop scoped to that session alone.
async fn stop_fixture_session_goal(
    pool: &PgPool,
    session: SessionId,
    seed: u128,
) -> Result<(), Box<dyn Error>> {
    let outcome = GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(seed)),
                session,
                GoalUserAction::Stop {
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                },
            ),
            None,
            |_| None,
        )
        .await?;
    assert_goal_command_applied(outcome);
    Ok(())
}

/// Fails naming the outcome a fixture goal command produced instead of an
/// appended event, so a rejection or a reused identity is not mistaken for a
/// fixture that set the goal up.
#[track_caller]
fn assert_goal_command_applied(outcome: GoalCommandHandlingOutcome) {
    match outcome {
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(_)) => {}
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Rejected(rejection)) => {
            panic!("the fixture goal command was rejected: {rejection:?}")
        }
        GoalCommandHandlingOutcome::ConflictingReuse { command_id } => {
            panic!("the fixture goal command identity is already used: {command_id:?}")
        }
        GoalCommandHandlingOutcome::TargetBusy { session } => {
            panic!("the fixture goal command target is held by session: {session:?}")
        }
        GoalCommandHandlingOutcome::LineageMoved => {
            panic!("the fixture goal command expected a lineage head that had moved")
        }
    }
}

/// The fixture check helper above branches over the three command outcomes,
/// so its rejected classification carries its own test
/// (`docs/agents/testing-style.md` rule 16).
#[test]
fn assert_goal_command_applied_names_a_rejection() {
    let panic = std::panic::catch_unwind(|| {
        assert_goal_command_applied(GoalCommandHandlingOutcome::Recorded(
            GoalCommandResult::Rejected(GoalCommandRejection::SessionNotFound),
        ))
    })
    .expect_err("a rejected fixture goal command must fail its fixture");
    assert_eq!(
        panic.downcast_ref::<String>().map(String::as_str),
        Some("the fixture goal command was rejected: SessionNotFound")
    );
}

/// The fixture check helper above branches over the three command outcomes,
/// so its conflicting-reuse classification carries its own test
/// (`docs/agents/testing-style.md` rule 16).
#[test]
fn assert_goal_command_applied_names_a_conflicting_reuse() {
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(0x60a1));
    let panic = std::panic::catch_unwind(|| {
        assert_goal_command_applied(GoalCommandHandlingOutcome::ConflictingReuse { command_id })
    })
    .expect_err("a reused fixture goal command identity must fail its fixture");
    assert_eq!(
        panic.downcast_ref::<String>().map(String::as_str),
        Some(format!("the fixture goal command identity is already used: {command_id:?}").as_str())
    );
}

/// Fails naming the outcome a goal transition produced instead of an applied
/// event, so a refused or misrouted fixture transition is not mistaken for
/// one that landed.
#[track_caller]
fn assert_goal_transition_applied(outcome: &GoalTransitionOutcome) {
    match outcome {
        GoalTransitionOutcome::Applied(_) => {}
        GoalTransitionOutcome::GoalNotAttached => {
            panic!("the goal transition found no attached goal")
        }
        GoalTransitionOutcome::Rejected(error) => {
            panic!("the goal transition was rejected: {:?}", error.failure())
        }
        GoalTransitionOutcome::NotCurrentGoalTurn => {
            panic!("the goal transition named a turn outside the current goal generation")
        }
        GoalTransitionOutcome::SessionClosing => {
            panic!("the goal transition found a pending session closure")
        }
    }
}

/// A domain goal commissioned and stopped, whose refusal of a further
/// declaration supplies the check helper's rejected branch a real error.
fn stopped_fixture_goal() -> Goal {
    Goal::commission(
        SessionId::from_uuid(Uuid::from_u128(0x60b0)),
        GoalStatement::try_new(String::from("classify the check helper"))
            .expect("the fixture statement is admitted"),
        GoalUserProvenance::new(DurableCommandId::from_uuid(Uuid::from_u128(0x60b1))),
    )
    .stop(GoalUserProvenance::new(DurableCommandId::from_uuid(
        Uuid::from_u128(0x60b2),
    )))
    .expect("a pursuing fixture goal admits stopping")
}

/// The transition check helper above branches over the four outcomes, so its
/// missing-goal classification carries its own test
/// (`docs/agents/testing-style.md` rule 16).
#[test]
fn assert_goal_transition_applied_names_a_missing_goal() {
    let panic = std::panic::catch_unwind(|| {
        assert_goal_transition_applied(&GoalTransitionOutcome::GoalNotAttached)
    })
    .expect_err("a transition without an attached goal must fail its fixture");
    assert_eq!(
        panic.downcast_ref::<&str>().copied(),
        Some("the goal transition found no attached goal")
    );
}

/// The transition check helper above branches over the four outcomes, so its
/// rejected classification carries its own test
/// (`docs/agents/testing-style.md` rule 16).
#[test]
fn assert_goal_transition_applied_names_a_rejection() {
    let error = stopped_fixture_goal()
        .declare_achieved(
            GoalReport::try_new(String::from("nothing further to report"))
                .expect("the fixture report is admitted"),
            GoalModelProvenance::new(
                TurnId::from_uuid(Uuid::from_u128(0x60b3)),
                ToolRequestId::from_uuid(Uuid::from_u128(0x60b4)),
            ),
        )
        .expect_err("a stopped goal refuses a further declaration");
    let failure = error.failure();
    let panic = std::panic::catch_unwind(|| {
        assert_goal_transition_applied(&GoalTransitionOutcome::Rejected(error))
    })
    .expect_err("a rejected fixture transition must fail its fixture");
    assert_eq!(
        panic.downcast_ref::<String>().map(String::as_str),
        Some(format!("the goal transition was rejected: {failure:?}").as_str())
    );
}

/// The transition check helper above branches over the four outcomes, so its
/// non-current-turn classification carries its own test
/// (`docs/agents/testing-style.md` rule 16).
#[test]
fn assert_goal_transition_applied_names_a_non_current_turn() {
    let panic = std::panic::catch_unwind(|| {
        assert_goal_transition_applied(&GoalTransitionOutcome::NotCurrentGoalTurn)
    })
    .expect_err("a transition from a non-current turn must fail its fixture");
    assert_eq!(
        panic.downcast_ref::<&str>().copied(),
        Some("the goal transition named a turn outside the current goal generation")
    );
}

async fn insert_completed_judge(
    connection: &mut PgConnection,
    fixture: &RestartModelCallFixture,
    request: ToolRequestId,
    seed: u128,
    recommendation: &str,
    input_tokens: Option<Decimal>,
    usage_provenance: Option<&str>,
) -> Result<(Uuid, Uuid), sqlx::Error> {
    let (selection, call) = insert_prepared_judge(connection, fixture, request, seed).await?;
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
                recommendation_kind = $1, rationale = $2,
                input_tokens = $3,
                usage_provenance_kind = COALESCE($4, usage_provenance_kind)
          WHERE model_call_id = $5",
    )
    .bind(recommendation)
    .bind(APPROVAL_JUDGE_RATIONALE)
    .bind(input_tokens)
    .bind(usage_provenance)
    .bind(call)
    .execute(&mut *connection)
    .await?;
    Ok((selection, call))
}

async fn insert_prepared_judge(
    connection: &mut PgConnection,
    fixture: &RestartModelCallFixture,
    request: ToolRequestId,
    seed: u128,
) -> Result<(Uuid, Uuid), sqlx::Error> {
    let selection = Uuid::from_u128(seed + 1);
    let call = Uuid::from_u128(seed + 2);
    sqlx::query(
        "INSERT INTO tool_approval_judge_model_call
            (model_call_id, request_id, session_id, turn_id,
             direct_model_selection_id, resolved_provider_model_identity_id,
             credential_reference, state_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'prepared')",
    )
    .bind(call)
    .bind(request.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(selection)
    .bind(Uuid::from_u128(seed + 3))
    .bind(APPROVAL_JUDGE_CREDENTIAL)
    .execute(&mut *connection)
    .await?;
    Ok((selection, call))
}

async fn persist_delegated_denial_fixture(
    connection: &mut PgConnection,
    fixture: &RestartModelCallFixture,
    request: ToolRequestId,
    judge_seed: u128,
    continuation_attempt: TurnAttemptId,
    input_tokens: Option<Decimal>,
    usage_provenance: Option<&str>,
) -> Result<Uuid, sqlx::Error> {
    let (selection, judge_call) = insert_completed_judge(
        connection,
        fixture,
        request,
        judge_seed,
        "deny",
        input_tokens,
        usage_provenance,
    )
    .await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source, denial_reason,
             delegate_model_selection_id, delegate_model_call_id, rationale)
         VALUES ($1, 'deny', 'delegate', $2, $3, $4, $2)",
    )
    .bind(request.into_uuid())
    .bind(APPROVAL_JUDGE_RATIONALE)
    .bind(selection)
    .bind(judge_call)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id,
             continued_from_attempt_id, state_kind)
         VALUES ($1, $2, $3, $4, 'prepared')",
    )
    .bind(continuation_attempt.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.attempt.into_uuid())
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'running', current_attempt_id = $1,
                approval_tool_request_id = NULL
          WHERE turn_id = $2 AND session_id = $3
            AND state_kind = 'active'
            AND active_phase_kind = 'awaiting_tool_approval'
            AND approval_tool_request_id = $4
            AND active_tool_round_call_id = $5",
    )
    .bind(continuation_attempt.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(request.into_uuid())
    .bind(fixture.call.into_uuid())
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ('tool_approval_decided', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO tool_approval_decided_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, request_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $2, $3
           FROM header",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(request.into_uuid())
    .execute(&mut *connection)
    .await?;
    Ok(judge_call)
}

async fn insert_user_approval_decision_event(
    connection: &mut PgConnection,
    fixture: &RestartModelCallFixture,
    request: ToolRequestId,
    command: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'decide_tool_request', 1, transaction_timestamp(), 'operator')",
    )
    .bind(command)
    .execute(&mut *connection)
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
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source, user_command_id)
         VALUES ($1, 'approve', 'user_command', $2)",
    )
    .bind(request.into_uuid())
    .bind(command)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ('tool_approval_decided', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO tool_approval_decided_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, request_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $2, $3
           FROM header",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(request.into_uuid())
    .execute(&mut *connection)
    .await?;
    Ok(())
}

fn database_constraint(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(|error| error.constraint())
}

fn process_tool_approval(
    snapshot: &ProcessTranscriptSnapshot,
    request: ToolRequestId,
) -> Option<&ProcessToolApproval> {
    snapshot.entries().iter().find_map(|entry| match entry {
        ProcessTranscriptEntry::AssistantToolUse {
            request: entry_request,
            approval,
            ..
        } if *entry_request == request => approval.as_ref(),
        _ => None,
    })
}

/// The synthetic external-effect tool the ambiguity fixture proposes.
const AMBIGUITY_FIXTURE_TOOL: &str = "external-tool";

/// Declares the ambiguity fixture's tool the way a composed hub would.
///
/// The recovery path exercised below exists only for an external effect: the
/// application asks the catalog for the class, prepares an attempt of that
/// class, and the domain rejects an ambiguous observation against an
/// effect-free attempt outright. Naming the tool and separately handing
/// `ToolEffectClass::ExternalEffect` to the repository would freeze the class
/// on this test's own authority, leaving the fixture green for a pairing no
/// catalog declares. Declaring it once here and reading the class back through
/// the `ToolCatalog` port makes the declaration the single source of that
/// fact, so a declaration changed to `EffectFree` fails the recovery
/// assertions instead of quietly disagreeing with them.
///
/// The permission default has to agree with the fixture's durable approval
/// history for the same reason. `checkpoint_confirmed_tool_round` records an
/// `InitialToolApproval::Confirm` round closed by a user decision, and
/// `initial_tool_approval` maps an `Auto` declaration to `PolicyAuto`, so
/// declaring `Auto` here would describe a batch no application composes: the
/// recovery test would stay green while the auto-approved path it appeared to
/// cover was broken.
fn ambiguity_fixture_catalog() -> CompiledToolCatalog {
    let definition = ToolDefinition::new(
        ToolName::try_new(String::from(AMBIGUITY_FIXTURE_TOOL))
            .expect("the fixture tool name is admitted"),
        String::from("Synthetic external-effect tool for ambiguity fixtures"),
        ToolInputSchema::try_new(String::from(r#"{"type":"object"}"#))
            .expect("the fixture input schema is admitted"),
        ToolPermissionDefault::Confirm,
        ToolEffectClass::ExternalEffect,
    );
    CompiledToolCatalog::try_new([CompiledTool::new(
        definition,
        |_: &NormalizedToolArguments| -> Result<(), ToolExecutionErrorDetail> { Ok(()) },
    )])
    .expect("the single-tool fixture catalog is admitted")
}

/// What one dispatched event announces about the turn under test.
///
/// A named projection rather than a wildcard filter: the assertions below
/// claim an ambiguous external effect announces its proposal and its recovery
/// and *nothing definitive*, and that claim is only as good as the set of
/// kinds it considered. Adding a `DispatchedOutboxEventKind` variant makes the
/// match non-exhaustive and stops this crate compiling until the new kind is
/// classified, rather than letting a `_ => None` arm silently exclude a new
/// definitive announcement from the claim. Deliberately dependency-free, per
/// `docs/style.md`: the projection reads only the event and the two fixture
/// identities it is asked about.
#[derive(Clone, Debug, Eq, PartialEq)]
enum AmbiguityAnnouncement {
    /// A tool-batch presentation boundary for the batch under test.
    BatchTransition(DispatchedToolBatchState),
    /// The turn under test was announced as definitively resolved.
    DefinitiveTurnOutcome,
    /// Nothing that bears on the ambiguity claim.
    Unrelated,
}

fn announcement_for(
    kind: &DispatchedOutboxEventKind,
    fixture_turn: TurnId,
    fixture_call: ModelCallId,
) -> AmbiguityAnnouncement {
    match kind {
        DispatchedOutboxEventKind::ToolBatchTransition {
            turn,
            producing_call,
            state,
        } if *turn == fixture_turn && *producing_call == fixture_call => {
            AmbiguityAnnouncement::BatchTransition(*state)
        }
        // A turn announced completed, failed, refused, or cancelled has been
        // reported resolved one way or another; an effect that may or may not
        // have happened admits none of those. `TurnReconciliationRequired` is
        // the honest terminal announcement for ambiguity and is not definitive.
        DispatchedOutboxEventKind::TurnTerminal {
            turn,
            disposition:
                DispatchedTurnTerminalDisposition::Completed { .. }
                | DispatchedTurnTerminalDisposition::Failed { .. }
                | DispatchedTurnTerminalDisposition::Refused { .. }
                | DispatchedTurnTerminalDisposition::Cancelled { .. },
        } if *turn == fixture_turn => AmbiguityAnnouncement::DefinitiveTurnOutcome,
        DispatchedOutboxEventKind::ToolBatchTransition { .. }
        | DispatchedOutboxEventKind::TurnTerminal { .. }
        | DispatchedOutboxEventKind::SessionCreated(_)
        | DispatchedOutboxEventKind::SessionStateChanged(_)
        | DispatchedOutboxEventKind::SessionTerminal(_)
        | DispatchedOutboxEventKind::GoalChanged(_)
        | DispatchedOutboxEventKind::CommandSettled { .. }
        | DispatchedOutboxEventKind::InjectionSettled { .. }
        | DispatchedOutboxEventKind::SessionOwnershipChanged(_)
        | DispatchedOutboxEventKind::SessionModelSettingsChanged(_)
        | DispatchedOutboxEventKind::TurnModelSettingsResolved(_)
        | DispatchedOutboxEventKind::InputAccepted { .. }
        | DispatchedOutboxEventKind::TurnActivated { .. }
        | DispatchedOutboxEventKind::ModelCallTransition { .. }
        | DispatchedOutboxEventKind::ToolApprovalDecided { .. }
        | DispatchedOutboxEventKind::ContextCompacted { .. }
        | DispatchedOutboxEventKind::DelegationUpdate(_)
        | DispatchedOutboxEventKind::DelegationWake(_)
        | DispatchedOutboxEventKind::RunnerStateTransition { .. } => {
            AmbiguityAnnouncement::Unrelated
        }
    }
}

/// Role-aware identities for the classifier's straight-line cases.
///
/// The classifier reads only the turn and producing call it is asked about, so
/// its fixture needs distinct identities rather than particular ones. Minting
/// them by role keeps that fact visible and leaves no arbitrary hexadecimal in
/// the test body.
#[derive(Default)]
struct ClassifierFixtureIds {
    next: u128,
}

impl ClassifierFixtureIds {
    fn next_value(&mut self) -> Uuid {
        self.next += 1;
        Uuid::from_u128(0x9100 + self.next)
    }

    fn next_turn(&mut self) -> TurnId {
        TurnId::from_uuid(self.next_value())
    }

    fn next_call(&mut self) -> ModelCallId {
        ModelCallId::from_uuid(self.next_value())
    }

    fn next_tool_attempt(&mut self) -> ToolAttemptId {
        ToolAttemptId::from_uuid(self.next_value())
    }

    fn next_frontier(&mut self) -> ContextFrontierId {
        ContextFrontierId::from_uuid(self.next_value())
    }

    fn next_entry(&mut self) -> SemanticTranscriptEntryId {
        SemanticTranscriptEntryId::from_uuid(self.next_value())
    }
}

/// The batch states announced for `turn`/`call`, in dispatch order.
fn announced_batch_states(
    dispatched: &[DispatchedOutboxEventKind],
    turn: TurnId,
    call: ModelCallId,
) -> Vec<DispatchedToolBatchState> {
    dispatched
        .iter()
        .filter_map(|kind| match announcement_for(kind, turn, call) {
            AmbiguityAnnouncement::BatchTransition(state) => Some(state),
            AmbiguityAnnouncement::DefinitiveTurnOutcome | AmbiguityAnnouncement::Unrelated => None,
        })
        .collect()
}

/// The turns a dispatched batch announced as failed.
///
/// Matching over dispatched events is logic that `docs/agents/testing-style.md`
/// rule 2 keeps out of a test body, and reporting the turns rather than a bare
/// boolean lets a caller assert against the turn its fixture states (rule 6).
fn announced_failed_turns(dispatched: &[DispatchedOutboxEventKind]) -> Vec<TurnId> {
    dispatched
        .iter()
        .filter_map(|kind| match kind {
            DispatchedOutboxEventKind::TurnTerminal {
                turn,
                disposition: DispatchedTurnTerminalDisposition::Failed { .. },
            } => Some(*turn),
            _ => None,
        })
        .collect()
}

/// Whether anything announced `turn` as definitively resolved.
fn announces_a_definitive_turn_outcome(
    dispatched: &[DispatchedOutboxEventKind],
    turn: TurnId,
    call: ModelCallId,
) -> bool {
    dispatched.iter().any(|kind| {
        announcement_for(kind, turn, call) == AmbiguityAnnouncement::DefinitiveTurnOutcome
    })
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
            && content == &user_content(expected_content)
    ));
}

fn create_session_corruption(error: CreateSessionRepositoryError) -> CreateSessionCorruption {
    let CreateSessionRepositoryError::Corruption(corruption) = error else {
        panic!("the ordinary creation reader failure is durable corruption")
    };
    corruption
}

fn session_corruption(error: SessionRepositoryError) -> SessionCorruption {
    let SessionRepositoryError::Corruption(corruption) = error else {
        panic!("the current-session reader failure is durable corruption")
    };
    corruption
}

async fn activate_delegated_result_fixture(
    pool: &PgPool,
    seed: u128,
) -> Result<
    (
        SessionId,
        SessionId,
        TurnId,
        ToolRequestId,
        DirectModelSelection,
    ),
    Box<dyn Error>,
> {
    let parent = SessionId::from_uuid(Uuid::from_u128(seed + 1));
    let child = SessionId::from_uuid(Uuid::from_u128(seed + 2));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 3));
    let parent_turn = TurnId::from_uuid(Uuid::from_u128(seed + 4));
    let child_turn = TurnId::from_uuid(Uuid::from_u128(seed + 5));
    let spawning_request = ToolRequestId::from_uuid(Uuid::from_u128(seed + 6));
    let task_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 7));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(seed + 8, seed + 1, direct(seed + 3)))
        .await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(seed + 9, seed + 2, direct(seed + 3)))
        .await?;
    let mut fixture = pool.begin().await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_initial_task DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event DISABLE TRIGGER ALL;",
    )
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             provenance_kind, provenance_session_id, provenance_turn_id,
             provenance_tool_request_id)
         VALUES ($1, 1, 'spawned', 'tool_request', $2, $3, $1)",
    )
    .bind(spawning_request.into_uuid())
    .bind(parent.into_uuid())
    .bind(parent_turn.into_uuid())
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation
            (spawning_tool_request_id, parent_session_id, parent_turn_id,
             child_session_id, policy_kind)
         VALUES ($1, $2, $3, $4, 'background')",
    )
    .bind(spawning_request.into_uuid())
    .bind(parent.into_uuid())
    .bind(parent_turn.into_uuid())
    .bind(child.into_uuid())
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_kind, origin_accepted_input_id,
             acceptance_position, state_kind)
         VALUES ($1, $2, 'delegation', NULL, 1, 'queued')",
    )
    .bind(child_turn.into_uuid())
    .bind(child.into_uuid())
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_initial_task
            (spawning_tool_request_id, child_session_id, turn_id,
             semantic_entry_id, admission_position, defaults_version,
             requested_model_kind, requested_direct_model_selection_id,
             frozen_model_kind, frozen_direct_model_selection_id, task_content)
         VALUES ($1, $2, $3, $4, 1, 1, 'direct', $5, 'direct', $5, $6)",
    )
    .bind(spawning_request.into_uuid())
    .bind(child.into_uuid())
    .bind(child_turn.into_uuid())
    .bind(task_entry.into_uuid())
    .bind(selection.into_uuid())
    .bind("return the delegated result")
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             delegated_task_spawning_tool_request_id)
         VALUES ($1, $2, 'delegated_task', $3)",
    )
    .bind(child.into_uuid())
    .bind(task_entry.into_uuid())
    .bind(spawning_request.into_uuid())
    .execute(&mut *fixture)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_initial_task ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event ENABLE TRIGGER ALL;",
    )
    .execute(&mut *fixture)
    .await?;
    fixture.commit().await?;

    let activation = StartEligibleTurnRepository::new(pool.clone());
    let preview = activation
        .preview(
            child,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 10)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 11)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 12)),
                TurnAttemptId::from_uuid(Uuid::from_u128(seed + 13)),
            ),
        )
        .await?
        .expect("the delegated result fixture has one activation preview");
    let CommitActivationPreviewOutcome::Activated(_) = activation.commit_preview(preview).await?
    else {
        return Err("the delegated result fixture activation changed".into());
    };
    record_empty_instruction_manifest(pool, child).await?;
    Ok((parent, child, child_turn, spawning_request, selection))
}

struct AuthorizedDelegatedModelCallFixture {
    parent: SessionId,
    child: SessionId,
    spawning_request: ToolRequestId,
    repository: PostgresModelCallRepository,
    authorized: AuthorizedModelCall,
}

async fn authorize_delegated_model_call_fixture(
    pool: &PgPool,
    seed: u128,
) -> Result<AuthorizedDelegatedModelCallFixture, Box<dyn Error>> {
    let (parent, child, _turn, spawning_request, selection) =
        activate_delegated_result_fixture(pool, seed).await?;
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 20));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one delegated terminal fixture target forms a catalog");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let call = ModelCallId::from_uuid(Uuid::from_u128(seed + 21));
    assert!(matches!(
        repository
            .prepare_initial_call(
                child,
                call,
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 22)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 23)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 24)),
                |_| {
                    (
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 25)),
                        TurnId::from_uuid(Uuid::from_u128(seed + 26)),
                    )
                },
            )
            .await?,
        PrepareInitialModelCallOutcome::Checkpointed(checkpointed) if checkpointed == call
    ));
    let AuthorizeModelCallOutcome::Authorized(authorized) =
        repository.authorize_send(child, call).await?
    else {
        panic!("the delegated terminal fixture authorizes its exact call")
    };
    Ok(AuthorizedDelegatedModelCallFixture {
        parent,
        child,
        spawning_request,
        repository,
        authorized: *authorized,
    })
}

struct AuthorizedDelegatedSuccessorFixture {
    child: SessionId,
    selection: DirectModelSelection,
    repository: PostgresModelCallRepository,
    authorized: AuthorizedModelCall,
}

async fn authorize_delegated_successor_model_call_fixture(
    pool: &PgPool,
    seed: u128,
) -> Result<AuthorizedDelegatedSuccessorFixture, Box<dyn Error>> {
    let (_parent, child, _delegated_turn, _spawning_request, selection) =
        activate_delegated_result_fixture(pool, seed).await?;
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 20));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one delegated successor target forms a catalog");
    complete_text_turn(
        pool,
        child,
        targets.clone(),
        model_credential_reference(),
        seed + 0x100,
        "complete the delegated initial turn",
    )
    .await?;
    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 0x201));
    let submitted = SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 0x202,
                child.as_uuid().as_u128(),
                "continue after delegated completion",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x203)),
            Some(successor),
        )
        .await?;
    assert!(matches!(
        submitted,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    activate_earliest_queued_turn(
        pool,
        EarliestQueuedTurnActivation {
            session: child.into_uuid(),
            origin_entry: Uuid::from_u128(seed + 0x204),
            starting_frontier: Uuid::from_u128(seed + 0x205),
            initial_attempt: Uuid::from_u128(seed + 0x206),
        },
    )
    .await?;
    record_empty_instruction_manifest(pool, child).await?;

    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0x207));
    assert!(matches!(
        repository
            .prepare_initial_call(
                child,
                call,
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x208)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x209)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x20a)),
                |_| {
                    (
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x20b)),
                        TurnId::from_uuid(Uuid::from_u128(seed + 0x20c)),
                    )
                },
            )
            .await?,
        PrepareInitialModelCallOutcome::Checkpointed(checkpointed) if checkpointed == call
    ));
    let AuthorizeModelCallOutcome::Authorized(authorized) =
        repository.authorize_send(child, call).await?
    else {
        panic!("the accepted-input successor authorizes its exact call")
    };
    Ok(AuthorizedDelegatedSuccessorFixture {
        child,
        selection,
        repository,
        authorized: *authorized,
    })
}

async fn reclassify_successor_as_delegated_wake(
    pool: &PgPool,
    fixture: &AuthorizedDelegatedSuccessorFixture,
) -> Result<(), Box<dyn Error>> {
    let mut transaction = pool.begin().await?;
    sqlx::raw_sql(
        "ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL;
         ALTER TABLE session_pending_delivery DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_wake_turn_origin DISABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET origin_kind = 'delegation', origin_accepted_input_id = NULL
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(fixture.child.into_uuid())
    .bind(fixture.authorized.turn().into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_pending_delivery
            (recipient_session_id, delivery_sequence, delivery_kind)
         VALUES ($1, 1, 'background_result')",
    )
    .bind(fixture.child.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_wake_turn_origin
            (turn_id, recipient_session_id, admission_position,
             first_delivery_sequence, through_delivery_sequence,
             defaults_version, requested_model_kind,
             requested_direct_model_selection_id, frozen_model_kind,
             frozen_direct_model_selection_id)
         SELECT lifecycle.turn_id, lifecycle.session_id,
                lifecycle.acceptance_position, 1, 1, 1,
                'direct', $3, 'direct', $3
           FROM turn_lifecycle AS lifecycle
          WHERE lifecycle.session_id = $1 AND lifecycle.turn_id = $2",
    )
    .bind(fixture.child.into_uuid())
    .bind(fixture.authorized.turn().into_uuid())
    .bind(fixture.selection.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL;
         ALTER TABLE session_pending_delivery ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_wake_turn_origin ENABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

async fn attach_delegation_relationship_fixture(
    pool: &PgPool,
    child: SessionId,
    child_turn: TurnId,
    selection: DirectModelSelection,
    seed: u128,
) -> Result<(SessionId, ToolRequestId), Box<dyn Error>> {
    let parent = SessionId::from_uuid(Uuid::from_u128(seed + 1));
    let parent_turn = TurnId::from_uuid(Uuid::from_u128(seed + 2));
    let spawning_request = ToolRequestId::from_uuid(Uuid::from_u128(seed + 3));
    let task_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 4));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            seed + 5,
            seed + 1,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    let mut fixture = pool.begin().await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_initial_task DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event DISABLE TRIGGER ALL;",
    )
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             provenance_kind, provenance_session_id, provenance_turn_id,
             provenance_tool_request_id)
         VALUES ($1, 1, 'spawned', 'tool_request', $2, $3, $1)",
    )
    .bind(spawning_request.into_uuid())
    .bind(parent.into_uuid())
    .bind(parent_turn.into_uuid())
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation
            (spawning_tool_request_id, parent_session_id, parent_turn_id,
             child_session_id, policy_kind)
         VALUES ($1, $2, $3, $4, 'background')",
    )
    .bind(spawning_request.into_uuid())
    .bind(parent.into_uuid())
    .bind(parent_turn.into_uuid())
    .bind(child.into_uuid())
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_initial_task
            (spawning_tool_request_id, child_session_id, turn_id,
             semantic_entry_id, admission_position, defaults_version,
             requested_model_kind, requested_direct_model_selection_id,
             frozen_model_kind, frozen_direct_model_selection_id, task_content)
         VALUES ($1, $2, $3, $4, 1, 1, 'direct', $5, 'direct', $5, $6)",
    )
    .bind(spawning_request.into_uuid())
    .bind(child.into_uuid())
    .bind(child_turn.into_uuid())
    .bind(task_entry.into_uuid())
    .bind(selection.into_uuid())
    .bind("retain unresolved delegated ambiguity")
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             delegated_task_spawning_tool_request_id)
         VALUES ($1, $2, 'delegated_task', $3)",
    )
    .bind(child.into_uuid())
    .bind(task_entry.into_uuid())
    .bind(spawning_request.into_uuid())
    .execute(&mut *fixture)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_initial_task ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event ENABLE TRIGGER ALL;",
    )
    .execute(&mut *fixture)
    .await?;
    fixture.commit().await?;
    Ok((parent, spawning_request))
}

#[derive(Clone, Copy)]
struct DelegatedToolCrashFixture {
    parent: SessionId,
    child: SessionId,
    turn: TurnId,
    spawning_request: ToolRequestId,
}

async fn prepare_delegated_tool_crash_fixture(
    pool: &PgPool,
    seed: u128,
) -> Result<DelegatedToolCrashFixture, Box<dyn Error>> {
    let fixture = authorize_delegated_model_call_fixture(pool, seed).await?;
    let request = ToolRequestId::from_uuid(Uuid::from_u128(seed + 0x40));
    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            ToolCallProposal::new(
                ToolName::try_new(String::from("current_time")).expect("valid fixture tool name"),
                NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                    .expect("bounded fixture arguments"),
            ),
        )])
        .expect("the proposal forms a tool-using response");
    let observation = fixture
        .authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools { response });
    let outcome = fixture
        .repository
        .apply_terminal_observation(
            fixture.child,
            observation,
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![ToolResponsePartIdentity::tool_call(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x80)),
                    request,
                    InitialToolApproval::Confirm,
                )],
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xc0)),
                None,
            )),
            |_| panic!("the delegated fixture has no pending steering to reclassify"),
        )
        .await?;
    let ModelCallTerminalOutcome::ToolRound(_) = outcome else {
        panic!("the delegated fixture reaches a tool round")
    };
    let repository = PostgresToolLoopRepository::new(pool.clone());
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x100)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x101)),
        )
        .await?;
    let attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0x102));
    repository
        .prepare_next_attempt(
            fixture.child,
            fixture.authorized.turn(),
            attempt,
            ToolEffectClass::EffectFree,
        )
        .await?;
    repository
        .authorize_attempt(fixture.child, fixture.authorized.turn(), attempt)
        .await?;
    Ok(DelegatedToolCrashFixture {
        parent: fixture.parent,
        child: fixture.child,
        turn: fixture.authorized.turn(),
        spawning_request: fixture.spawning_request,
    })
}

struct DelegatedCapabilityFailureFixture {
    repository: PostgresModelCallRepository,
    child: SessionId,
    call: ModelCallId,
    spawning_request: ToolRequestId,
}

async fn delegated_capability_failure_fixture(
    pool: &PgPool,
    seed: u128,
) -> Result<DelegatedCapabilityFailureFixture, Box<dyn Error>> {
    let (_parent, child, _turn, spawning_request, selection) =
        activate_delegated_result_fixture(pool, seed).await?;
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 20));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one delegated capability target forms a catalog");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let call = ModelCallId::from_uuid(Uuid::from_u128(seed + 21));
    let prepared = repository
        .prepare_initial_call(
            child,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 22)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 23)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 24)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 25)),
                    TurnId::from_uuid(Uuid::from_u128(seed + 26)),
                )
            },
        )
        .await?;
    assert_eq!(prepared, PrepareInitialModelCallOutcome::Checkpointed(call));
    repository
        .fail_prepared_call(
            child,
            call,
            PreparedModelCallFailureCause::CapabilityKnownFailure,
            None,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 27)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 28)),
            ),
            |_| panic!("the delegated capability fixture has no steering"),
        )
        .await?;
    Ok(DelegatedCapabilityFailureFixture {
        repository,
        child,
        call,
        spawning_request,
    })
}

#[derive(Clone, Copy)]
enum DelegatedCapabilityResultDamage {
    InitialTask,
    Result,
    Update,
    UpdateHeaderKind,
    Wake,
    WakeHeaderKind,
}

async fn assert_delegated_capability_reread_rejects_damage(
    seed: u128,
    damage: DelegatedCapabilityResultDamage,
) -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = delegated_capability_failure_fixture(&pool, seed).await?;
    assert_eq!(
        fixture
            .repository
            .reread_prepared_failure(fixture.child, fixture.call, None)
            .await?,
        RetainedPreparedFailureStatus::AlreadyCommitted
    );
    match damage {
        DelegatedCapabilityResultDamage::InitialTask => {
            sqlx::query("ALTER TABLE session_delegation_initial_task DISABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
            sqlx::query(
                "DELETE FROM session_delegation_initial_task
                  WHERE spawning_tool_request_id = $1",
            )
            .bind(fixture.spawning_request.into_uuid())
            .execute(&pool)
            .await?;
            sqlx::query("ALTER TABLE session_delegation_initial_task ENABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
        }
        DelegatedCapabilityResultDamage::Result => {
            sqlx::query("ALTER TABLE session_child_result DISABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
            sqlx::query("DELETE FROM session_child_result WHERE spawning_tool_request_id = $1")
                .bind(fixture.spawning_request.into_uuid())
                .execute(&pool)
                .await?;
            sqlx::query("ALTER TABLE session_child_result ENABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
        }
        DelegatedCapabilityResultDamage::Update => {
            sqlx::query("ALTER TABLE delegation_update_outbox_event DISABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
            sqlx::query(
                "DELETE FROM delegation_update_outbox_event
                  WHERE update_kind = 'child_result'
                    AND result_spawning_request_id = $1",
            )
            .bind(fixture.spawning_request.into_uuid())
            .execute(&pool)
            .await?;
            sqlx::query("ALTER TABLE delegation_update_outbox_event ENABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
        }
        DelegatedCapabilityResultDamage::UpdateHeaderKind => {
            sqlx::query("ALTER TABLE delegation_outbox_event DISABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
            sqlx::query(
                "UPDATE delegation_outbox_event AS header
                    SET event_kind = 'delegation_wake'
                   FROM delegation_update_outbox_event AS parent_update
                  WHERE header.event_sequence = parent_update.event_sequence
                    AND parent_update.result_spawning_request_id = $1",
            )
            .bind(fixture.spawning_request.into_uuid())
            .execute(&pool)
            .await?;
            sqlx::query("ALTER TABLE delegation_outbox_event ENABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
        }
        DelegatedCapabilityResultDamage::Wake => {
            sqlx::query("ALTER TABLE delegation_wake_outbox_event DISABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
            sqlx::query(
                "DELETE FROM delegation_wake_outbox_event
                  WHERE subject_kind = 'result'
                    AND result_spawning_request_id = $1",
            )
            .bind(fixture.spawning_request.into_uuid())
            .execute(&pool)
            .await?;
            sqlx::query("ALTER TABLE delegation_wake_outbox_event ENABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
        }
        DelegatedCapabilityResultDamage::WakeHeaderKind => {
            sqlx::query("ALTER TABLE delegation_outbox_event DISABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
            sqlx::query(
                "UPDATE delegation_outbox_event AS header
                    SET event_kind = 'delegation_update'
                   FROM delegation_wake_outbox_event AS parent_wake
                  WHERE header.event_sequence = parent_wake.event_sequence
                    AND parent_wake.result_spawning_request_id = $1",
            )
            .bind(fixture.spawning_request.into_uuid())
            .execute(&pool)
            .await?;
            sqlx::query("ALTER TABLE delegation_outbox_event ENABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
        }
    }
    let error = fixture
        .repository
        .reread_prepared_failure(fixture.child, fixture.call, None)
        .await
        .expect_err("damaged delegated delivery cannot authenticate a capability failure");
    assert!(matches!(
        error,
        ModelCallRepositoryError::InvalidTransition(
            "retained prepared failure durable closure is incomplete"
        )
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[derive(Clone, Copy)]
enum DelegatedObservationDisposition {
    Completed,
    KnownFailed,
    Refused,
    Cancelled,
}

async fn assert_delegated_observation_reread_requires_result(
    seed: u128,
    disposition: DelegatedObservationDisposition,
) -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = authorize_delegated_model_call_fixture(&pool, seed).await?;
    let (observation, identities) = match disposition {
        DelegatedObservationDisposition::Completed => (
            fixture
                .authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::Completed {
                    assistant_text: vec![
                        AssistantText::try_new(String::from("authenticated delegated result"))
                            .expect("fixture delegated result is admitted"),
                    ],
                }),
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 30,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 31)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 32)),
            )),
        ),
        DelegatedObservationDisposition::KnownFailed => (
            fixture
                .authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed),
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 30)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 31)),
            )),
        ),
        DelegatedObservationDisposition::Refused => (
            fixture
                .authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::Refused),
            ModelCallTerminalIdentities::Refused(
                signalbox_domain::RefusedModelCallTurnIdentities::new(
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 30)),
                ),
            ),
        ),
        DelegatedObservationDisposition::Cancelled => (
            fixture
                .authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::Cancelled),
            ModelCallTerminalIdentities::PhysicalCancellation(
                PhysicalCancellationModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 30)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 31)),
                ),
            ),
        ),
    };
    fixture
        .repository
        .apply_terminal_observation(fixture.child, observation.clone(), identities, |_| {
            panic!("the delegated observation fixture has no steering")
        })
        .await?;
    assert_eq!(
        fixture
            .repository
            .reread_terminal_observation(fixture.child, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    sqlx::query("ALTER TABLE session_child_result DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM session_child_result WHERE spawning_tool_request_id = $1")
        .bind(fixture.spawning_request.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE session_child_result ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let error = fixture
        .repository
        .reread_terminal_observation(fixture.child, &observation)
        .await
        .expect_err("a delegated observation reread requires its child result closure");
    assert!(matches!(
        error,
        ModelCallRepositoryError::InvalidTransition(
            "retained observation delegated result closure changed"
        )
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[derive(Clone, Copy)]
enum DelegatedNonterminalObservation {
    CompletedWithTools,
    Ambiguous,
}

async fn assert_delegated_nonterminal_reread_rejects_result(
    seed: u128,
    kind: DelegatedNonterminalObservation,
) -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = authorize_delegated_model_call_fixture(&pool, seed).await?;
    let (observation, identities) = match kind {
        DelegatedNonterminalObservation::CompletedWithTools => {
            let request = ToolRequestId::from_uuid(Uuid::from_u128(seed + 30));
            let response =
                ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
                    ToolCallProposal::new(
                        ToolName::try_new(String::from("current_time"))
                            .expect("valid fixture tool name"),
                        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                            .expect("bounded fixture arguments"),
                    ),
                )])
                .expect("the proposal forms a tool-using response");
            (
                fixture
                    .authorized
                    .observation_correlation()
                    .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools {
                        response,
                    }),
                ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                    vec![ToolResponsePartIdentity::tool_call(
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 31)),
                        request,
                        InitialToolApproval::Confirm,
                    )],
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 32)),
                    None,
                )),
            )
        }
        DelegatedNonterminalObservation::Ambiguous => (
            fixture
                .authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::Ambiguous),
            ModelCallTerminalIdentities::Ambiguous(AmbiguousModelCallTurnIdentities::new(
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 30)),
            )),
        ),
    };
    fixture
        .repository
        .apply_terminal_observation(fixture.child, observation.clone(), identities, |_| {
            panic!("the delegated nonterminal fixture has no steering")
        })
        .await?;
    assert_eq!(
        fixture
            .repository
            .reread_terminal_observation(fixture.child, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    sqlx::query("ALTER TABLE session_child_result DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO session_child_result
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, content_text)
         VALUES ($1, 1, 'outcome_recorded', 'child_failed', NULL)",
    )
    .bind(fixture.spawning_request.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_child_result ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let error = fixture
        .repository
        .reread_terminal_observation(fixture.child, &observation)
        .await
        .expect_err("a nonterminal observation cannot retain delegated result evidence");

    assert!(matches!(
        error,
        ModelCallRepositoryError::InvalidTransition(
            "retained observation delegated result closure changed"
        )
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

fn delegated_tool_crash_scan_ids(seed: u128) -> FixedStartupScanIds {
    FixedStartupScanIds::new(
        [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
            seed + 0x110,
        ))],
        [ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x111))],
    )
}

fn delegated_tool_crash_failure_ids(seed: u128) -> AcceptedInputTurnFailureIdentities {
    AcceptedInputTurnFailureIdentities::new(
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x112)),
        ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x113)),
    )
}

async fn checkpoint_foreground_child_wait_without_result(
    pool: &PgPool,
    seed: u128,
) -> Result<(RestartModelCallFixture, ToolRequestId, ToolRequestId), Box<dyn Error>> {
    let (fixture, _, _, requests) = checkpoint_confirmed_tool_batch(
        pool,
        seed,
        &[("spawn_session", "{}"), ("await_session", "{}")],
    )
    .await?;
    let [spawning_request, awaiting_request] = requests.as_slice() else {
        panic!("the foreground fixture has spawn and await requests")
    };
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(seed + 0x100, seed + 0x101, direct(seed + 5)))
        .await?;
    let child = Uuid::from_u128(seed + 0x101);
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let issuing_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe0));
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0)),
                *spawning_request,
                ToolApprovalDecision::Approve,
            ),
            || panic!("the first approval does not start execution"),
        )
        .await?;
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd1)),
                *awaiting_request,
                ToolApprovalDecision::Approve,
            ),
            || issuing_attempt,
        )
        .await?;
    sqlx::raw_sql(
        "ALTER TABLE tool_attempt DISABLE TRIGGER ALL;
         ALTER TABLE turn_attempt DISABLE TRIGGER ALL;
         ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL;",
    )
    .execute(pool)
    .await?;
    let spawn_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe1));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            spawn_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the approved spawn request prepares one attempt");
    let authorized_spawn = repository
        .authorize_attempt(fixture.session, fixture.turn, spawn_attempt)
        .await?;
    repository
        .commit_observation(authorized_spawn.executor_fence().bind(
            ToolAttemptObservation::Completed {
                result: ToolResultContent::Text(
                    ToolResultText::try_new(child.to_string()).expect("bounded child identity"),
                ),
            },
        ))
        .await?;
    let await_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe2));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            await_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the approved await request prepares one attempt");

    sqlx::raw_sql(
        "ALTER TABLE session_delegation DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_wait DISABLE TRIGGER ALL;
         ALTER TABLE tool_attempt DISABLE TRIGGER ALL;
         ALTER TABLE turn_attempt DISABLE TRIGGER ALL;
         ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL;",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation
            (spawning_tool_request_id, parent_session_id, parent_turn_id,
             child_session_id, policy_kind)
         VALUES ($1, $2, $3, $4, 'background')",
    )
    .bind(spawning_request.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(child)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             provenance_kind, provenance_session_id, provenance_turn_id,
             provenance_tool_request_id)
         VALUES ($1, 1, 'spawned', 'tool_request', $2, $3, $1)",
    )
    .bind(spawning_request.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_wait
            (awaiting_tool_request_id, spawning_tool_request_id,
             parent_session_id, parent_turn_id, child_session_id, wait_mode)
         VALUES ($1, $2, $3, $4, $5, 'foreground')",
    )
    .bind(awaiting_request.into_uuid())
    .bind(spawning_request.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(child)
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'awaiting_child',
                wait_spawning_request_id = $1,
                wait_child_session_id = $2
          WHERE attempt_id = $3",
    )
    .bind(spawning_request.into_uuid())
    .bind(child)
    .bind(await_attempt.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1",
    )
    .bind(issuing_attempt.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_child', current_attempt_id = NULL,
                child_wait_request_id = $1
          WHERE turn_id = $2",
    )
    .bind(awaiting_request.into_uuid())
    .bind(fixture.turn.into_uuid())
    .execute(pool)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_wait ENABLE TRIGGER ALL;
         ALTER TABLE tool_attempt ENABLE TRIGGER ALL;
         ALTER TABLE turn_attempt ENABLE TRIGGER ALL;
         ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL;",
    )
    .execute(pool)
    .await?;

    Ok((fixture, *spawning_request, *awaiting_request))
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ConcurrentPlanAppendDisposition {
    Appended,
    DuplicateAttempt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlanRepositoryErrorKind {
    AppendProvenance,
    CurrentCreation,
    DependencyStatus,
    EventSequence,
    UntrustedProvenance,
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

fn status_plan_arguments(entry: PlanEntryId, status: PlanStatus) -> String {
    serde_json::to_string(&serde_json::json!({
        "entry_id": entry.as_u64(),
        "kind": "set_status",
        "status": status,
    }))
    .expect("the plan status arguments fixture serializes")
}

fn depends_plan_arguments(entry: PlanEntryId, dependency: PlanEntryId) -> String {
    serde_json::to_string(&serde_json::json!({
        "dependency_id": dependency.as_u64(),
        "entry_id": entry.as_u64(),
        "kind": "depends_on",
    }))
    .expect("the plan dependency arguments fixture serializes")
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

struct AuthorizedPlanWriteBatch {
    session: SessionId,
    turn: TurnId,
    next_attempt_seed: u128,
    repository: PostgresToolLoopRepository,
}

impl AuthorizedPlanWriteBatch {
    async fn authorize_next(&mut self) -> Result<ToolDispatchAuthority, Box<dyn Error>> {
        let attempt = ToolAttemptId::from_uuid(Uuid::from_u128(self.next_attempt_seed));
        self.next_attempt_seed += 1;
        self.repository
            .prepare_next_attempt(
                self.session,
                self.turn,
                attempt,
                ToolEffectClass::ExternalEffect,
            )
            .await?
            .expect("the next approved plan write prepares its physical attempt");
        Ok(self
            .repository
            .authorize_attempt(self.session, self.turn, attempt)
            .await?)
    }

    async fn finish(&self, authorized: ToolDispatchAuthority) -> Result<(), Box<dyn Error>> {
        self.repository
            .commit_observation(
                authorized
                    .executor_fence()
                    .bind(ToolAttemptObservation::Completed {
                        result: ToolResultContent::Text(
                            ToolResultText::try_new(String::from("plan event appended"))
                                .expect("the plan result fixture is bounded"),
                        ),
                    }),
            )
            .await?;
        Ok(())
    }
}

async fn authorize_plan_writes(
    pool: &PgPool,
    arguments: &[String],
) -> Result<(SessionId, AuthorizedPlanWriteBatch), Box<dyn Error>> {
    let seed =
        u128::from(NEXT_PLAN_FIXTURE_SEED.fetch_add(PLAN_FIXTURE_SEED_STRIDE, Ordering::Relaxed));
    let proposals = arguments
        .iter()
        .map(|arguments| ("plan_write", arguments.as_str()))
        .collect::<Vec<_>>();
    let (fixture, _, _, requests) = checkpoint_confirmed_tool_batch(pool, seed, &proposals).await?;
    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    for (index, request) in requests.iter().enumerate() {
        let offset = u128::try_from(index)?;
        tool_repository
            .decide(
                decide_tool_request(
                    DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0 + offset)),
                    *request,
                    ToolApprovalDecision::Approve,
                ),
                || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe0 + offset)),
            )
            .await?;
    }
    Ok((
        fixture.session,
        AuthorizedPlanWriteBatch {
            session: fixture.session,
            turn: fixture.turn,
            next_attempt_seed: seed + 0xf0,
            repository: tool_repository,
        },
    ))
}

async fn append_plan_write(
    batch: &mut AuthorizedPlanWriteBatch,
    repository: &SessionPlanRepository,
    draft: PlanEventDraft,
) -> Result<PlanEvent, Box<dyn Error>> {
    let authorized = batch.authorize_next().await?;
    let outcome = repository
        .append(PlanAppendRequest::new(
            PlanEventProvenance::from_invocation(authorized.correlation()),
            draft,
        ))
        .await?;
    batch.finish(authorized).await?;
    Ok(expect_appended(outcome))
}

fn expect_appended(outcome: PlanAppendOutcome) -> PlanEvent {
    match outcome {
        PlanAppendOutcome::Appended(event) => event,
        PlanAppendOutcome::Rejected(rejection) => {
            panic!("the plan append fixture was unexpectedly rejected: {rejection:?}")
        }
    }
}

fn expect_dependency_cycle(outcome: PlanAppendOutcome) -> PlanDependencyCycle {
    match outcome {
        PlanAppendOutcome::Rejected(PlanAppendRejection::DependencyCycle(cycle)) => cycle,
        PlanAppendOutcome::Appended(event) => {
            panic!("the cyclic dependency unexpectedly appended: {event:?}")
        }
        PlanAppendOutcome::Rejected(rejection) => {
            panic!("the cycle fixture received a different rejection: {rejection:?}")
        }
    }
}

fn plan_repository_error_kind(error: SessionPlanRepositoryError) -> PlanRepositoryErrorKind {
    match error {
        SessionPlanRepositoryError::InvalidAppendProvenance => {
            PlanRepositoryErrorKind::AppendProvenance
        }
        SessionPlanRepositoryError::Corruption(SessionPlanCorruption::InvalidEventPayload(
            "current creation",
        )) => PlanRepositoryErrorKind::CurrentCreation,
        SessionPlanRepositoryError::Corruption(SessionPlanCorruption::InvalidEventPayload(
            "dependency status",
        )) => PlanRepositoryErrorKind::DependencyStatus,
        SessionPlanRepositoryError::Corruption(SessionPlanCorruption::InvalidEventSequence) => {
            PlanRepositoryErrorKind::EventSequence
        }
        SessionPlanRepositoryError::Corruption(SessionPlanCorruption::UntrustedProvenance) => {
            PlanRepositoryErrorKind::UntrustedProvenance
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

const DEPENDENCY_PREREQUISITE_TEXT: &str = "finish the durable base";
const DEPENDENCY_DEPENDENT_TEXT: &str = "ship dependent work";
const EXPECTED_PLAN_MUTATED_ROW_COUNT: u64 = 1;
const SYNTHETIC_DEPENDENCY_ORDINAL_BASE: i64 = 100;
const SYNTHETIC_EVENT_ORDINAL_BASE: i64 = 200;

struct DependencyPlanFixture {
    session: SessionId,
    batch: AuthorizedPlanWriteBatch,
    repository: SessionPlanRepository,
    prerequisite: PlanEntryId,
    dependent: PlanEntryId,
}

async fn dependency_plan_fixture(
    pool: &PgPool,
    mut trailing_arguments: Vec<String>,
) -> Result<DependencyPlanFixture, Box<dyn Error>> {
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let mut arguments = vec![
        create_plan_arguments(DEPENDENCY_PREREQUISITE_TEXT),
        create_plan_arguments(DEPENDENCY_DEPENDENT_TEXT),
        depends_plan_arguments(dependent, prerequisite),
    ];
    arguments.append(&mut trailing_arguments);
    let (session, mut batch) = authorize_plan_writes(pool, &arguments).await?;
    let repository = SessionPlanRepository::new(pool.clone());
    append_plan_write(
        &mut batch,
        &repository,
        PlanEventDraft::Create {
            text: plan_text(DEPENDENCY_PREREQUISITE_TEXT),
        },
    )
    .await?;
    append_plan_write(
        &mut batch,
        &repository,
        PlanEventDraft::Create {
            text: plan_text(DEPENDENCY_DEPENDENT_TEXT),
        },
    )
    .await?;
    append_plan_write(
        &mut batch,
        &repository,
        PlanEventDraft::DependsOn {
            entry: dependent,
            dependency: prerequisite,
        },
    )
    .await?;
    Ok(DependencyPlanFixture {
        session,
        batch,
        repository,
        prerequisite,
        dependent,
    })
}

async fn insert_direct_dependency_event(
    pool: &PgPool,
    fixture: &DependencyPlanFixture,
    authorized: &ToolDispatchAuthority,
) -> Result<(), sqlx::Error> {
    insert_direct_dependency_event_between(
        pool,
        fixture,
        authorized,
        fixture.prerequisite,
        fixture.dependent,
    )
    .await
}

async fn insert_direct_dependency_event_between(
    pool: &PgPool,
    fixture: &DependencyPlanFixture,
    authorized: &ToolDispatchAuthority,
    entry: PlanEntryId,
    dependency: PlanEntryId,
) -> Result<(), sqlx::Error> {
    insert_direct_dependency_event_at(pool, fixture, authorized, 3, 4, entry, dependency).await
}

async fn insert_direct_dependency_event_at(
    pool: &PgPool,
    fixture: &DependencyPlanFixture,
    authorized: &ToolDispatchAuthority,
    prior_event_ordinal: u64,
    event_ordinal: u64,
    entry: PlanEntryId,
    dependency: PlanEntryId,
) -> Result<(), sqlx::Error> {
    let correlation = authorized.correlation();
    sqlx::query(
        "INSERT INTO session_plan_event
            (session_id, event_ordinal, prior_event_ordinal,
             event_kind, entry_ordinal, dependency_ordinal,
             entry_text, entry_status, provenance_turn_id,
             provenance_issuing_turn_attempt_id, provenance_request_id,
             provenance_attempt_id, provenance_dispatch_generation)
         VALUES ($1, $2, $3, 'depends_on', $4, $5, NULL, NULL,
                 $6, $7, $8, $9, $10)",
    )
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(event_ordinal))
    .bind(Decimal::from(prior_event_ordinal))
    .bind(Decimal::from(entry.as_u64()))
    .bind(Decimal::from(dependency.as_u64()))
    .bind(correlation.turn().into_uuid())
    .bind(correlation.issuing_attempt().into_uuid())
    .bind(correlation.request().into_uuid())
    .bind(correlation.attempt().into_uuid())
    .bind(Decimal::from(correlation.generation().as_u64()))
    .execute(pool)
    .await
    .map(|_| ())
}

async fn corrupt_plan_event_predecessor(
    pool: &PgPool,
    fixture: &DependencyPlanFixture,
    event_ordinal: u64,
    malformed_prior_event_ordinal: Option<u64>,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "ALTER TABLE session_plan_event
         DROP CONSTRAINT session_plan_event_predecessor_shape",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_immutable",
    )
    .execute(pool)
    .await?;
    let corrupted = sqlx::query(
        "UPDATE session_plan_event
            SET prior_event_ordinal = $1
          WHERE session_id = $2
            AND event_ordinal = $3",
    )
    .bind(malformed_prior_event_ordinal.map(Decimal::from))
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(event_ordinal))
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_immutable",
    )
    .execute(pool)
    .await?;
    Ok(corrupted.rows_affected())
}

async fn corrupt_dependency_event_authority(
    pool: &PgPool,
    session: SessionId,
    event_ordinal: u64,
    mismatched_arguments: &str,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "ALTER TABLE tool_request
         DISABLE TRIGGER tool_request_is_append_only",
    )
    .execute(pool)
    .await?;
    let corrupted = sqlx::query(
        "UPDATE tool_request AS request
            SET arguments_text = $1
           FROM session_plan_event AS event
          WHERE event.session_id = $2
            AND event.event_ordinal = $3
            AND request.request_id = event.provenance_request_id",
    )
    .bind(mismatched_arguments)
    .bind(session.into_uuid())
    .bind(Decimal::from(event_ordinal))
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE tool_request
         ENABLE TRIGGER tool_request_is_append_only",
    )
    .execute(pool)
    .await?;
    Ok(corrupted.rows_affected())
}

async fn corrupt_dependency_projection_predecessor(
    pool: &PgPool,
    session: SessionId,
    first_event_ordinal: u64,
    malformed_prior_first_event_ordinal: u64,
) -> Result<u64, sqlx::Error> {
    let predecessor_order_constraints: Vec<String> = sqlx::query_scalar(
        "SELECT quote_ident(conname)
           FROM pg_constraint
          WHERE conrelid = 'session_plan_current_dependency'::regclass
            AND contype = 'c'
            AND pg_get_constraintdef(oid) LIKE
                '%prior_first_event_ordinal < first_event_ordinal%'
          ORDER BY conname",
    )
    .fetch_all(pool)
    .await?;
    for constraint in predecessor_order_constraints {
        let statement =
            format!("ALTER TABLE session_plan_current_dependency DROP CONSTRAINT {constraint}");
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(pool)
            .await?;
    }
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         DISABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         DISABLE TRIGGER session_plan_current_dependency_predecessor_guard",
    )
    .execute(pool)
    .await?;
    let corrupted = sqlx::query(
        "UPDATE session_plan_current_dependency
            SET prior_first_event_ordinal = $1
          WHERE session_id = $2
            AND first_event_ordinal = $3",
    )
    .bind(Decimal::from(malformed_prior_first_event_ordinal))
    .bind(session.into_uuid())
    .bind(Decimal::from(first_event_ordinal))
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         ENABLE TRIGGER session_plan_current_dependency_predecessor_guard",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         ENABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(pool)
    .await?;
    Ok(corrupted.rows_affected())
}

async fn insert_synthetic_dependency_projection(
    pool: &PgPool,
    session: SessionId,
    entry: PlanEntryId,
    edge_count: i64,
) -> Result<u64, sqlx::Error> {
    let constraints: Vec<String> = sqlx::query_scalar(
        "SELECT quote_ident(conname)
           FROM pg_constraint
          WHERE conrelid = 'session_plan_current_dependency'::regclass
            AND contype = 'f'
            AND (
                pg_get_constraintdef(oid) LIKE
                    'FOREIGN KEY (session_id, dependency_ordinal)%'
                OR pg_get_constraintdef(oid) LIKE
                    'FOREIGN KEY (session_id, first_event_ordinal)%'
            )
          ORDER BY conname",
    )
    .fetch_all(pool)
    .await?;
    for constraint in constraints {
        let statement =
            format!("ALTER TABLE session_plan_current_dependency DROP CONSTRAINT {constraint}");
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(pool)
            .await?;
    }
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         DISABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         DISABLE TRIGGER session_plan_current_dependency_predecessor_guard",
    )
    .execute(pool)
    .await?;
    let inserted = sqlx::query(
        "INSERT INTO session_plan_current_dependency
            (session_id, entry_ordinal, dependency_ordinal,
             first_event_ordinal, prior_first_event_ordinal)
         SELECT $1, $2, $4 + fixture.value, $5 + fixture.value, NULL
           FROM generate_series(0, $3 - 1) AS fixture(value)",
    )
    .bind(session.into_uuid())
    .bind(Decimal::from(entry.as_u64()))
    .bind(edge_count)
    .bind(SYNTHETIC_DEPENDENCY_ORDINAL_BASE)
    .bind(SYNTHETIC_EVENT_ORDINAL_BASE)
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         ENABLE TRIGGER session_plan_current_dependency_predecessor_guard",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         ENABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(pool)
    .await?;
    Ok(inserted.rows_affected())
}

async fn install_duplicate_dependency_projection(
    pool: &PgPool,
    fixture: &DependencyPlanFixture,
    duplicate_event_ordinal: u64,
) -> Result<(u64, u64), sqlx::Error> {
    const FIRST_DEPENDENCY_EVENT_ORDINAL: u64 = 3;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         DROP CONSTRAINT session_plan_current_dependency_pkey",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         DISABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(pool)
    .await?;
    let inserted = sqlx::query(
        "INSERT INTO session_plan_current_dependency
            (session_id, entry_ordinal, dependency_ordinal,
             first_event_ordinal, prior_first_event_ordinal)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(fixture.dependent.as_u64()))
    .bind(Decimal::from(fixture.prerequisite.as_u64()))
    .bind(Decimal::from(duplicate_event_ordinal))
    .bind(Decimal::from(FIRST_DEPENDENCY_EVENT_ORDINAL))
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         ENABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_head
         DISABLE TRIGGER session_plan_head_maintenance_guard",
    )
    .execute(pool)
    .await?;
    let certified = sqlx::query(
        "UPDATE session_plan_head
            SET dependency_event_ordinal = $1
          WHERE session_id = $2",
    )
    .bind(Decimal::from(duplicate_event_ordinal))
    .bind(fixture.session.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_head
         ENABLE TRIGGER session_plan_head_maintenance_guard",
    )
    .execute(pool)
    .await?;
    Ok((inserted.rows_affected(), certified.rows_affected()))
}

async fn reorder_dependency_projection_chain(
    pool: &PgPool,
    session: SessionId,
    oldest_event_ordinal: u64,
    middle_event_ordinal: u64,
    newest_event_ordinal: u64,
) -> Result<u64, sqlx::Error> {
    let predecessor_order_constraints: Vec<String> = sqlx::query_scalar(
        "SELECT quote_ident(conname)
           FROM pg_constraint
          WHERE conrelid = 'session_plan_current_dependency'::regclass
            AND contype = 'c'
            AND pg_get_constraintdef(oid) LIKE
                '%prior_first_event_ordinal < first_event_ordinal%'
          ORDER BY conname",
    )
    .fetch_all(pool)
    .await?;
    for constraint in predecessor_order_constraints {
        let statement =
            format!("ALTER TABLE session_plan_current_dependency DROP CONSTRAINT {constraint}");
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(pool)
            .await?;
    }
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         DISABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         DISABLE TRIGGER session_plan_current_dependency_predecessor_guard",
    )
    .execute(pool)
    .await?;
    let reordered = sqlx::query(
        "UPDATE session_plan_current_dependency
            SET prior_first_event_ordinal =
                CASE first_event_ordinal
                    WHEN $1 THEN $2
                    WHEN $2 THEN NULL
                    WHEN $3 THEN $1
                END
          WHERE session_id = $4
            AND first_event_ordinal IN ($1, $2, $3)",
    )
    .bind(Decimal::from(oldest_event_ordinal))
    .bind(Decimal::from(middle_event_ordinal))
    .bind(Decimal::from(newest_event_ordinal))
    .bind(session.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         ENABLE TRIGGER session_plan_current_dependency_predecessor_guard",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         ENABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(pool)
    .await?;
    Ok(reordered.rows_affected())
}

async fn insert_orphan_dependency_projection(
    pool: &PgPool,
    session: SessionId,
) -> Result<u64, sqlx::Error> {
    let event_constraints: Vec<String> = sqlx::query_scalar(
        "SELECT quote_ident(conname)
           FROM pg_constraint
          WHERE conrelid = 'session_plan_current_dependency'::regclass
            AND contype = 'f'
            AND confrelid = 'session_plan_event'::regclass
          ORDER BY conname",
    )
    .fetch_all(pool)
    .await?;
    for constraint in event_constraints {
        let statement =
            format!("ALTER TABLE session_plan_current_dependency DROP CONSTRAINT {constraint}");
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(pool)
            .await?;
    }
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         DISABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(pool)
    .await?;
    let inserted = sqlx::query(
        "INSERT INTO session_plan_current_dependency
            (session_id, entry_ordinal, dependency_ordinal,
             first_event_ordinal, prior_first_event_ordinal)
         VALUES ($1, 1, 2, 3, NULL)",
    )
    .bind(session.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         ENABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(pool)
    .await?;
    Ok(inserted.rows_affected())
}

async fn insert_direct_malformed_status_event(
    pool: &PgPool,
    fixture: &DependencyPlanFixture,
    authorized: &ToolDispatchAuthority,
) -> Result<(), sqlx::Error> {
    const PRIOR_EVENT_ORDINAL: u64 = 3;
    const EVENT_ORDINAL: u64 = 4;
    const MALFORMED_STATUS_TEXT: &str = "status event must not carry text";
    let correlation = authorized.correlation();
    sqlx::query(
        "INSERT INTO session_plan_event
            (session_id, event_ordinal, prior_event_ordinal,
             event_kind, entry_ordinal, dependency_ordinal,
             entry_text, entry_status, provenance_turn_id,
             provenance_issuing_turn_attempt_id, provenance_request_id,
             provenance_attempt_id, provenance_dispatch_generation)
         VALUES ($1, $2, $3, 'status_changed', $4, NULL, $5, 'completed',
                 $6, $7, $8, $9, $10)",
    )
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(EVENT_ORDINAL))
    .bind(Decimal::from(PRIOR_EVENT_ORDINAL))
    .bind(Decimal::from(fixture.prerequisite.as_u64()))
    .bind(MALFORMED_STATUS_TEXT)
    .bind(correlation.turn().into_uuid())
    .bind(correlation.issuing_attempt().into_uuid())
    .bind(correlation.request().into_uuid())
    .bind(correlation.attempt().into_uuid())
    .bind(Decimal::from(correlation.generation().as_u64()))
    .execute(pool)
    .await
    .map(|_| ())
}

async fn insert_dependency_without_target(
    pool: &PgPool,
    fixture: &DependencyPlanFixture,
    authorized: &ToolDispatchAuthority,
) -> Result<(), sqlx::Error> {
    const PRIOR_EVENT_ORDINAL: u64 = 3;
    const EVENT_ORDINAL: u64 = 4;
    let correlation = authorized.correlation();
    sqlx::query(
        "INSERT INTO session_plan_event
            (session_id, event_ordinal, prior_event_ordinal,
             event_kind, entry_ordinal, dependency_ordinal,
             entry_text, entry_status, provenance_turn_id,
             provenance_issuing_turn_attempt_id, provenance_request_id,
             provenance_attempt_id, provenance_dispatch_generation)
         VALUES ($1, $2, $3, 'depends_on', $4, NULL, NULL, NULL,
                 $5, $6, $7, $8, $9)",
    )
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(EVENT_ORDINAL))
    .bind(Decimal::from(PRIOR_EVENT_ORDINAL))
    .bind(Decimal::from(fixture.prerequisite.as_u64()))
    .bind(correlation.turn().into_uuid())
    .bind(correlation.issuing_attempt().into_uuid())
    .bind(correlation.request().into_uuid())
    .bind(correlation.attempt().into_uuid())
    .bind(Decimal::from(correlation.generation().as_u64()))
    .execute(pool)
    .await
    .map(|_| ())
}

fn dependency_edge(event: &PlanEvent) -> (PlanEntryId, PlanEntryId) {
    match event.kind() {
        PlanEventKind::DependsOn { entry, dependency } => (*entry, *dependency),
        PlanEventKind::Created { .. }
        | PlanEventKind::TextRevised { .. }
        | PlanEventKind::StatusChanged { .. } => {
            panic!("fixture event is not a dependency edge")
        }
    }
}
