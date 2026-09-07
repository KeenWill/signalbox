//! Feature-gated PostgreSQL coverage for migrations, durable invariants, and repository
//! composition.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics, explicit fixture expectations, and impossible fixture branches; the workspace gate remains active for production targets"
)]

mod fixtures;

use fixtures::approval::*;
use fixtures::delegation::*;
use fixtures::model_call::*;
use fixtures::outbox::*;
use fixtures::plan::*;
use fixtures::session_admission::*;
use fixtures::tool_rounds::*;
use fixtures::turn_activation::*;

#[path = "../support/mod.rs"]
mod support;

mod approval_decisions;
mod attention;
mod convergence_sweep;
mod credential_capacity;
mod credential_capacity_policy;
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
mod oauth_credential;
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
    PreparedCreateSession, PreparedModelCallRequest, ProviderCompactionBlock,
    ProviderModelCallFailureCause, ProviderModelIdentity, ProviderReportedTokenUsage,
    ReasoningLevel, RecordedUserOverride, RefusedModelCallTurnIdentities, ReplaceSessionDefaults,
    ReplaceSessionDefaultsRejectedResult, ReplaceSessionDefaultsResult, ResolvedProviderTarget,
    SemanticTranscriptEntryId, SemanticTranscriptEntryRef, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    SessionId, SessionInputPosition, SessionOwnership, SessionPlacement, SessionPlacementPath,
    SessionSystemPrompt, SessionTemplateContentDigest, SessionTemplateName,
    SessionTemplateProvenance, SettingOverlay, StoppedToolResponsePartIdentity,
    StoppedToolRoundModelCallIdentities, SubmitInput, SubmitInputAppliedResult,
    SubmitInputReconstitutionFailure, SubmitInputRejectedResult, SubmitInputResult,
    ToolApprovalDecider, ToolApprovalDecision, ToolApprovalResolution, ToolAttemptCrashOutcome,
    ToolAttemptEnd, ToolAttemptId, ToolAttemptObservation, ToolBatchExecutionFailure,
    ToolCallProposal, ToolDecisionRationale, ToolDecisionSource, ToolDenialReason,
    ToolDispatchAuthority, ToolEffectClass, ToolExecutionError, ToolExecutionErrorDetail,
    ToolExecutionErrorKind, ToolName, ToolPermissionDefault, ToolRequestId,
    ToolResponsePartIdentity, ToolResultContent, ToolResultText, ToolRoundModelCallIdentities,
    ToolUsingAssistantResponse, TranscriptAncestry, TurnAttemptId, TurnConfigurationProvenance,
    TurnId, TurnTerminalCause, UserContent, UserContentPart,
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
        CommitActivationPreviewOutcome, CommitCountedAttachmentFailurePreviewOutcome,
        StartEligibleTurnCorruption, StartEligibleTurnIdentityCollision,
        StartEligibleTurnRepository, StartEligibleTurnRepositoryError,
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

#[path = "../../../../tooling/postgres_test_image.rs"]
mod postgres_test_image;
use postgres_test_image::POSTGRES_IMAGE_TAG;
const DATABASE_NAME: &str = "signalbox_integration";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
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

fn test_session_credential_pin() -> signalbox_persistence::SessionCredentialPin {
    signalbox_persistence::SessionCredentialPin::try_new(vec![
        signalbox_persistence::SessionModelCredential::new(
            "test-model-family",
            "test-model-primary",
        ),
    ])
    .expect("test credential pin is valid")
}

const DELEGATION_OUTBOX_COMMAND_ID: u128 = 0xdc00;
const DELEGATION_AFTER_MESSAGE_OUTCOME_ORDINAL: i16 = 3;

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

async fn insert_raw_wait_and_message(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
) -> Result<(), sqlx::Error> {
    insert_raw_wait(connection, fixture).await?;
    insert_raw_message(connection, fixture, "parent_to_child", fixture.child).await?;
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

fn prepared_with_fast_target(
    command: u128,
    session: u128,
    selection: DirectModelSelection,
    fast_target: ResolvedProviderTarget,
) -> PreparedCreateSession {
    let precedence = ModelSettingsPrecedence::new(
        ModelSettingsOverlay::inherit_all(),
        ModelSettingsOverlay::new(
            SettingOverlay::Inherit,
            FastModeOverlay::Value(FastMode::Enabled),
            SettingOverlay::Inherit,
        ),
        ModelSettingsOverlay::inherit_all(),
        ModelSettingsOverlay::inherit_all(),
    );
    let settings = ModelCapabilities::new(
        BTreeSet::new(),
        FastModeSupport::AlternateTarget(fast_target),
        BTreeSet::new(),
    )
    .validate_precedence(selection, precedence)
    .expect("the fixture capability admits its alternate fast target");
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
const FIXTURE_ATTACHMENT_MAXIMUM_BYTES: u64 = 1_024;

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
    provider_compaction: Option<ProviderCompactionBlock>,
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
    commit_authorized_tool_batch(
        seed,
        (fixture, model_repository, authorized),
        proposals,
        initial_approval,
        usage,
        provider_compaction,
    )
    .await
}

async fn commit_authorized_tool_batch(
    seed: u128,
    authorized_call: (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        AuthorizedModelCall,
    ),
    proposals: &[(&str, &str)],
    initial_approval: InitialToolApproval,
    usage: ProviderReportedTokenUsage,
    provider_compaction: Option<ProviderCompactionBlock>,
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        CorrelatedModelCallTerminalObservation,
        Vec<signalbox_domain::ToolRequestId>,
    ),
    Box<dyn Error>,
> {
    let (fixture, model_repository, authorized) = authorized_call;
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
        provider_compaction
            .iter()
            .cloned()
            .map(AssistantResponsePart::ProviderCompaction)
            .chain(proposals.iter().map(|(tool_name, arguments)| {
                AssistantResponsePart::ToolCall(ToolCallProposal::new(
                    ToolName::try_new(String::from(*tool_name)).expect("valid fixture tool name"),
                    NormalizedToolArguments::try_from_provider_text(String::from(*arguments))
                        .expect("bounded fixture arguments"),
                ))
            }))
            .collect(),
    )
    .expect("the proposals form a tool-using response");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation_with_usage(
            ModelCallTerminalObservation::CompletedWithTools {
                response,
                retained_input_tokens: provider_compaction.as_ref().map(|_| 10),
                retained_output_tokens: provider_compaction.as_ref().map(|_| 5),
            },
            usage,
        );
    let identities = provider_compaction
        .iter()
        .map(|_| {
            ToolResponsePartIdentity::provider_compaction(SemanticTranscriptEntryId::from_uuid(
                Uuid::from_u128(seed + 0x7f),
            ))
        })
        .chain(requests.iter().enumerate().map(|(index, request)| {
            ToolResponsePartIdentity::tool_call(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 0x80 + u128::try_from(index).expect("the bounded batch index fits u128"),
                )),
                *request,
                initial_approval,
            )
        }))
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

async fn checkpoint_fast_tool_batch_with_provider_compaction(
    pool: &PgPool,
    seed: u128,
    fast_target: ResolvedProviderTarget,
    provider_compaction: ProviderCompactionBlock,
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        CorrelatedModelCallTerminalObservation,
        Vec<signalbox_domain::ToolRequestId>,
    ),
    Box<dyn Error>,
> {
    let session = SessionId::from_uuid(Uuid::from_u128(seed + 1));
    let turn = TurnId::from_uuid(Uuid::from_u128(seed + 2));
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 3));
    let call = ModelCallId::from_uuid(Uuid::from_u128(seed + 4));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let selected_target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6)));

    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared_with_fast_target(
            seed + 7,
            seed + 1,
            selection,
            fast_target,
        ))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 8,
                seed + 1,
                "fast tool-round request",
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
        selected_target,
    )])
    .expect("one fast tool-round target forms a catalog");
    let families = ModelCredentialFamilyCatalog::try_new([
        (selected_target, Arc::<str>::from("test-model-family"), None),
        (fast_target, Arc::<str>::from("test-model-family"), None),
    ])
    .and_then(|catalog| catalog.with_fast_targets([(selected_target, fast_target)]))
    .expect("the fast tool-round targets share one credential family");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference())
            .with_session_credentials(families);
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
    let AuthorizeModelCallOutcome::Authorized(authorized) =
        repository.authorize_send(session, call).await?
    else {
        panic!("the fast tool-round call authorizes")
    };
    let fixture = RestartModelCallFixture {
        session,
        turn,
        attempt,
        call,
    };
    commit_authorized_tool_batch(
        seed,
        (fixture, repository, *authorized),
        &[("current_time", "{}")],
        InitialToolApproval::Confirm,
        ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(70))
            .with_output_tokens(Some(50)),
        Some(provider_compaction),
    )
    .await
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

fn database_constraint(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(|error| error.constraint())
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
