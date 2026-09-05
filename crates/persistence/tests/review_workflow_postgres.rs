#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

mod support;

use std::{error::Error, future::Future};

use signalbox_application::{
    AuthorizeModelCallOutcome, ModelCallCredentialReference, ReviewConcernClaim,
    ReviewConcernOutcome, ReviewConcernSpec, ReviewConcernSuccess, ReviewDurableSealOutcome,
    ReviewImportOutcome, ReviewImportedContextEvidence, ReviewJudgmentEffectId, ReviewJudgmentPlan,
    ReviewJudgmentPlanMember, ReviewOrchestrationAttempt, ReviewOrchestrationAttemptId,
    ReviewOrchestrationAttemptStore, ReviewPassCompletionStatus, ReviewPlannedDisposition,
    ReviewRepairMemberOutcome, ReviewRepairSuccess, ReviewStageTemplateDigests,
    ReviewTemplateDigest, ReviewWorkflowCommand, ReviewWorkflowCommandOutcome,
    ReviewWorkflowCommandResult, ReviewWorkflowCommandService, ReviewWorkflowOperation,
    ReviewWorkflowOperationKind, StartEligibleTurnIdGenerator, StartEligibleTurnOutcome,
    StartEligibleTurnService,
};
use signalbox_domain::{
    AcceptedInputId, AmbiguousModelCallTurnIdentities, AssistantText, AuthorizedModelCall,
    CancelledModelCallTurnIdentities, CompletedModelCallIdentities, ContextFrontierId,
    CreateSession, DeliveryRequest, DescendantTerminationScope, DirectModelSelection,
    DurableCommandId, FailedModelCallTurnIdentities, ModelCallId, ModelCallTerminalIdentities,
    ModelCallTerminalObservation, ModelCallTerminalOutcome, ModelSelectionOverride,
    ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition, PerInputConfigurationChoices,
    ProviderModelIdentity, ResolvedProviderTarget, ReviewChangeRequestNumber, ReviewConfidence,
    ReviewEventOrdinal, ReviewExternalLink, ReviewExternalLinkAssociation,
    ReviewExternalLinkAttachment, ReviewExternalLinkAttachmentResult, ReviewExternalLinkId,
    ReviewExternalLinkNoChangeResult, ReviewExternalLinkObservation,
    ReviewExternalLinkObservationResult, ReviewExternalLinkPublicationBlockedResult,
    ReviewExternalLinkTransitionFailure, ReviewExternalObjectKind, ReviewExternalObjectState,
    ReviewFinding, ReviewFindingConfidenceAxes, ReviewFindingContent, ReviewFindingDiffSide,
    ReviewFindingEvent, ReviewFindingEventKind, ReviewFindingEventResult,
    ReviewFindingEventResultKind, ReviewFindingId, ReviewFindingLocation,
    ReviewFindingPendingExternalLinkRef, ReviewFindingProposal, ReviewFindingRef,
    ReviewFindingSeverity, ReviewFindingStatus, ReviewFindingTransitionFailure, ReviewKey,
    ReviewLineRange, ReviewPass, ReviewPassAcceptedInputEvidence, ReviewPassEvidence, ReviewPassId,
    ReviewPassKind, ReviewPassRef, ReviewPassResult, ReviewPassState, ReviewPassTransitionFailure,
    ReviewPassTurnEvidence, ReviewPassTurnOutcome, ReviewPolicy, ReviewProducedFindings,
    ReviewReferencedFindingEvidence, ReviewRun, ReviewRunEvidence, ReviewRunId, ReviewRunRef,
    ReviewRunState, ReviewTarget, ReviewTargetId, ReviewTargetSubject, ReviewText,
    ReviewWorkflowKind, SemanticTranscriptEntryId, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    SessionId, SubmitInput, TranscriptAncestry, TurnAttemptId, TurnId, UserContent,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
    model_execution::{PostgresModelCallRepository, PrepareInitialModelCallOutcome},
    review_orchestration::{
        PostgresReviewOrchestrationStore, ReviewOrchestrationCommand,
        ReviewOrchestrationCommandClaim, ReviewOrchestrationCommandGuard,
        ReviewOrchestrationCommandKind, ReviewOrchestrationCommandResult,
        ReviewOrchestrationCurrentStage, ReviewOrchestrationStage, ReviewOrchestrationStoreError,
    },
    review_workflow::{
        ReserveExternalLinkOutcome, ReviewWorkflowInsertionError, ReviewWorkflowStore,
        ReviewWorkflowStoreError, ReviewWorkflowTransitionError,
    },
    start_eligible_turn::StartEligibleTurnRepository,
    submit_input::SubmitInputRepository,
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

use support::{blocked_backends_reached, record_empty_instruction_manifest};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_review_workflow_integration";
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

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    migrated_postgres_with_max_connections(4).await
}

async fn migrated_postgres_with_max_connections(
    max_connections: u32,
) -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
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
        .max_connections(max_connections)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

async fn migrated_postgres_in_configured_schema()
-> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
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
    let bootstrap = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    sqlx::query("CREATE SCHEMA configured_review_workflow AUTHORIZATION signalbox")
        .execute(&bootstrap)
        .await?;
    sqlx::query(
        "ALTER ROLE signalbox
         SET search_path TO configured_review_workflow",
    )
    .execute(&bootstrap)
    .await?;
    bootstrap.close().await;

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

/// Starts PostgreSQL with `pg_stat_statements` loaded so a test can count the
/// statements one call issues.
///
/// The count is taken server-side rather than by instrumenting a call site,
/// because the cost this guards against is spread across two crates: the
/// orchestration loaders and the workflow store they delegate to. A server-side
/// counter sees every statement either one issues, including any added later.
async fn migrated_postgres_counting_statements()
-> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        // A later `with_cmd` replaces the whole command, so the statement
        // counter's settings extend the shared ephemeral-durability arguments
        // rather than following them in a second call.
        .with_cmd(disposable_postgres_server_args().into_iter().chain([
            "-c",
            "shared_preload_libraries=pg_stat_statements",
            "-c",
            "pg_stat_statements.track=all",
        ]))
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
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .execute(&pool)
        .await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

/// Runs `work` and reports how many statements PostgreSQL executed for it.
///
/// The reset and the reading query are both excluded by name, and the reading
/// query has not yet been recorded when it computes its own sum.
async fn statements_executed<Work: Future<Output = Output>, Output>(
    pool: &PgPool,
    work: Work,
) -> Result<(Output, i64), Box<dyn Error>> {
    sqlx::query("SELECT pg_stat_statements_reset()")
        .execute(pool)
        .await?;
    let output = work.await;
    let executed = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(sum(calls), 0)::bigint
           FROM pg_stat_statements
          WHERE query NOT LIKE '%pg_stat_statements%'",
    )
    .fetch_one(pool)
    .await?;
    Ok((output, executed))
}

fn uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn key(value: &str) -> ReviewKey {
    ReviewKey::try_new(String::from(value)).expect("fixture key is admitted")
}

fn text(value: &str) -> ReviewText {
    ReviewText::try_new(String::from(value)).expect("fixture text is admitted")
}

#[derive(Clone, Copy)]
enum MaximumWidthKeyRole {
    Provider,
    Repository,
    HeadRevision,
    BaseRevision,
}

fn maximum_width_key(role: MaximumWidthKeyRole) -> ReviewKey {
    const KEY_BYTES: usize = 1_024;
    const HEX_CHUNK_WIDTH: usize = 16;
    const CHUNK_COUNT: usize = KEY_BYTES / HEX_CHUNK_WIDTH;
    const SPLITMIX_INCREMENT: u64 = 0x9e37_79b9_7f4a_7c15;
    const SPLITMIX_FIRST_FACTOR: u64 = 0xbf58_476d_1ce4_e5b9;
    const SPLITMIX_SECOND_FACTOR: u64 = 0x94d0_49bb_1331_11eb;
    const SPLITMIX_FIRST_SHIFT: u32 = 30;
    const SPLITMIX_SECOND_SHIFT: u32 = 27;
    const SPLITMIX_FINAL_SHIFT: u32 = 31;
    const PROVIDER_SEED: u64 = 0x243f_6a88_85a3_08d3;
    const REPOSITORY_SEED: u64 = 0x1319_8a2e_0370_7344;
    const HEAD_REVISION_SEED: u64 = 0xa409_3822_299f_31d0;
    const BASE_REVISION_SEED: u64 = 0x082e_fa98_ec4e_6c89;

    let mut state = match role {
        MaximumWidthKeyRole::Provider => PROVIDER_SEED,
        MaximumWidthKeyRole::Repository => REPOSITORY_SEED,
        MaximumWidthKeyRole::HeadRevision => HEAD_REVISION_SEED,
        MaximumWidthKeyRole::BaseRevision => BASE_REVISION_SEED,
    };
    let mut value = String::with_capacity(KEY_BYTES);
    for _ in 0..CHUNK_COUNT {
        state = state.wrapping_add(SPLITMIX_INCREMENT);
        let mut mixed = state;
        mixed = (mixed ^ (mixed >> SPLITMIX_FIRST_SHIFT)).wrapping_mul(SPLITMIX_FIRST_FACTOR);
        mixed = (mixed ^ (mixed >> SPLITMIX_SECOND_SHIFT)).wrapping_mul(SPLITMIX_SECOND_FACTOR);
        mixed ^= mixed >> SPLITMIX_FINAL_SHIFT;
        value.push_str(&format!("{mixed:0HEX_CHUNK_WIDTH$x}"));
    }
    key(&value)
}

#[test]
fn maximum_width_key_fixture_is_full_width_and_role_distinct() {
    const MAXIMUM_KEY_BYTES: usize = 1_024;

    let provider = maximum_width_key(MaximumWidthKeyRole::Provider);
    let repository = maximum_width_key(MaximumWidthKeyRole::Repository);
    let head = maximum_width_key(MaximumWidthKeyRole::HeadRevision);
    let base = maximum_width_key(MaximumWidthKeyRole::BaseRevision);

    assert_eq!(provider.as_str().len(), MAXIMUM_KEY_BYTES);
    assert_eq!(repository.as_str().len(), MAXIMUM_KEY_BYTES);
    assert_eq!(head.as_str().len(), MAXIMUM_KEY_BYTES);
    assert_eq!(base.as_str().len(), MAXIMUM_KEY_BYTES);
    assert_ne!(provider, repository);
    assert_ne!(provider, head);
    assert_ne!(provider, base);
    assert_ne!(repository, head);
    assert_ne!(repository, base);
    assert_ne!(head, base);
}

fn workflow_for_pass(kind: ReviewPassKind) -> ReviewWorkflowKind {
    match kind {
        ReviewPassKind::ImportExternalContext => ReviewWorkflowKind::ImportExternalContext,
        ReviewPassKind::ReadOnlyReview => ReviewWorkflowKind::ReadOnlyReview,
        ReviewPassKind::Judge => ReviewWorkflowKind::JudgeFindings,
        ReviewPassKind::Dedupe => ReviewWorkflowKind::DedupeFindings,
        ReviewPassKind::Publish => ReviewWorkflowKind::PublishReview,
        ReviewPassKind::Fix => ReviewWorkflowKind::FixFindings,
        ReviewPassKind::PropagateStack => ReviewWorkflowKind::PropagateStack,
    }
}

fn pass_evidence(
    reference: ReviewPassRef,
    kind: ReviewPassKind,
    policy: ReviewPolicy,
    state: ReviewPassState,
) -> ReviewPassEvidence {
    let session = SessionId::from_uuid(uuid(0x201));
    let accepted_input = AcceptedInputId::from_uuid(uuid(0x202));
    let (origin_turn, turn_evidence) = match &state {
        ReviewPassState::Queued => (TurnId::from_uuid(uuid(0x203)), None),
        ReviewPassState::Running { turn } => (
            *turn,
            Some(ReviewPassTurnEvidence::new(
                *turn,
                session,
                accepted_input,
                ReviewPassTurnOutcome::Active,
                None,
            )),
        ),
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            ..
        } => (
            *turn,
            Some(ReviewPassTurnEvidence::new(
                *turn,
                session,
                accepted_input,
                ReviewPassTurnOutcome::Completed,
                Some(*output_frontier),
            )),
        ),
        ReviewPassState::Failed { turn } => (
            *turn,
            Some(ReviewPassTurnEvidence::new(
                *turn,
                session,
                accepted_input,
                ReviewPassTurnOutcome::Failed,
                Some(ContextFrontierId::from_uuid(uuid(0x204))),
            )),
        ),
        ReviewPassState::Blocked { turn, .. } => (
            *turn,
            Some(ReviewPassTurnEvidence::new(
                *turn,
                session,
                accepted_input,
                ReviewPassTurnOutcome::ReconciliationRequired,
                Some(ContextFrontierId::from_uuid(uuid(0x204))),
            )),
        ),
        ReviewPassState::Cancelled { turn: Some(turn) } => (
            *turn,
            Some(ReviewPassTurnEvidence::new(
                *turn,
                session,
                accepted_input,
                ReviewPassTurnOutcome::Cancelled,
                Some(ContextFrontierId::from_uuid(uuid(0x204))),
            )),
        ),
        ReviewPassState::Cancelled { turn: None } => (TurnId::from_uuid(uuid(0x203)), None),
    };
    let pass = ReviewPass::try_reconstitute(signalbox_domain::ReviewPassReconstitutionInput::new(
        reference,
        kind,
        reference.run(),
        workflow_for_pass(kind),
        session,
        accepted_input,
        ReviewPassAcceptedInputEvidence::new(accepted_input, session, Some(origin_turn)),
        state,
        turn_evidence,
    ))
    .expect("fixture pass evidence is fully authenticated");
    ReviewPassEvidence::from_pass(&pass, policy)
}

fn succeeded_pass(reference: ReviewPassRef, kind: ReviewPassKind) -> ReviewPassEvidence {
    pass_evidence(
        reference,
        kind,
        ReviewPolicy::version_one(),
        ReviewPassState::Succeeded {
            turn: TurnId::from_uuid(uuid(0x203)),
            output_frontier: ContextFrontierId::from_uuid(uuid(0x131)),
            result: None,
        },
    )
}

fn run_evidence_for_pass(pass: ReviewPassEvidence) -> ReviewRunEvidence {
    let reference = pass.reference();
    let state = match pass.state() {
        ReviewPassState::Queued => ReviewRunState::Queued,
        ReviewPassState::Running { .. } => ReviewRunState::Running {
            active_pass: reference,
        },
        ReviewPassState::Succeeded { .. } => ReviewRunState::Succeeded {
            concluding_pass: reference,
        },
        ReviewPassState::Failed { .. } => ReviewRunState::Failed {
            failed_pass: reference,
        },
        ReviewPassState::Blocked { .. } => ReviewRunState::Blocked {
            blocking_pass: reference,
        },
        ReviewPassState::Cancelled { .. } => ReviewRunState::Cancelled {
            last_pass: Some(reference),
        },
    };
    ReviewRunEvidence::new(
        reference.run(),
        workflow_for_pass(pass.kind()),
        pass.policy(),
        state,
    )
}

fn pass_with_finding_event(
    finding: ReviewFindingRef,
    ordinal: ReviewEventOrdinal,
    pass: ReviewPassEvidence,
    kind: ReviewFindingEventResultKind,
) -> ReviewPassEvidence {
    let result =
        ReviewPassResult::FindingEvent(ReviewFindingEventResult::new(finding, ordinal, kind));
    let state = match pass.state() {
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            ..
        } => ReviewPassState::Succeeded {
            turn: *turn,
            output_frontier: *output_frontier,
            result: Some(result),
        },
        ReviewPassState::Blocked { turn, .. } => ReviewPassState::Blocked {
            turn: *turn,
            result: Some(result),
        },
        other => other.clone(),
    };
    pass_evidence(pass.reference(), pass.kind(), pass.policy(), state)
}

fn pass_with_produced_findings(
    findings: Vec<ReviewFindingRef>,
    pass: ReviewPassEvidence,
) -> ReviewPassEvidence {
    let state = match pass.state() {
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            ..
        } => ReviewPassState::Succeeded {
            turn: *turn,
            output_frontier: *output_frontier,
            result: Some(ReviewPassResult::ProducedFindings(
                ReviewProducedFindings::try_new(findings)
                    .expect("fixture findings are a canonical inventory"),
            )),
        },
        state => state.clone(),
    };
    pass_evidence(pass.reference(), pass.kind(), pass.policy(), state)
}

fn finding_event(
    finding: ReviewFindingRef,
    ordinal: ReviewEventOrdinal,
    pass: ReviewPassEvidence,
    kind: ReviewFindingEventKind,
) -> ReviewFindingEvent {
    let result_kind = match &kind {
        ReviewFindingEventKind::Accepted => ReviewFindingEventResultKind::Accepted,
        ReviewFindingEventKind::Rejected { reason } => ReviewFindingEventResultKind::Rejected {
            reason: reason.clone(),
        },
        ReviewFindingEventKind::Duplicate { canonical } => {
            ReviewFindingEventResultKind::Duplicate {
                canonical: *canonical,
            }
        }
        ReviewFindingEventKind::Superseded { successor } => {
            ReviewFindingEventResultKind::Superseded {
                successor: *successor,
            }
        }
        ReviewFindingEventKind::Stale => ReviewFindingEventResultKind::Stale,
        ReviewFindingEventKind::Posted { link } => {
            ReviewFindingEventResultKind::Posted { link: link.link() }
        }
        ReviewFindingEventKind::Fixed => ReviewFindingEventResultKind::Fixed,
        ReviewFindingEventKind::BlockedWithReason { reason, link } => {
            ReviewFindingEventResultKind::BlockedWithReason {
                reason: reason.clone(),
                link: link.as_ref().map(|link| link.link()),
            }
        }
    };
    let pass = pass_with_finding_event(finding, ordinal, pass, result_kind);
    ReviewFindingEvent::new(
        finding,
        ordinal,
        pass.reference(),
        pass.clone(),
        run_evidence_for_pass(pass),
        kind,
    )
}

fn attachment(
    link: ReviewExternalLinkId,
    pass: ReviewPassEvidence,
    external_object: ReviewKey,
) -> ReviewExternalLinkAttachment {
    let state = match pass.state() {
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            result,
        } => {
            let finding_event = match result {
                Some(ReviewPassResult::FindingEvent(event))
                    if matches!(event.kind(), ReviewFindingEventResultKind::Posted { .. }) =>
                {
                    Some(event.clone())
                }
                Some(ReviewPassResult::ExternalLinkAttachment(result)) => {
                    result.finding_event().cloned()
                }
                _ => None,
            };
            ReviewPassState::Succeeded {
                turn: *turn,
                output_frontier: *output_frontier,
                result: Some(ReviewPassResult::ExternalLinkAttachment(
                    ReviewExternalLinkAttachmentResult::new(
                        link,
                        external_object.clone(),
                        finding_event,
                    ),
                )),
            }
        }
        state => state.clone(),
    };
    let pass = pass_evidence(pass.reference(), pass.kind(), pass.policy(), state);
    ReviewExternalLinkAttachment::new(
        link,
        pass.reference(),
        pass.clone(),
        run_evidence_for_pass(pass),
        external_object,
    )
}

fn posted_attachment(
    link: ReviewExternalLinkId,
    pass: ReviewPassEvidence,
    external_object: ReviewKey,
    finding: ReviewFindingRef,
    ordinal: ReviewEventOrdinal,
) -> ReviewExternalLinkAttachment {
    let state = match pass.state() {
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            ..
        } => ReviewPassState::Succeeded {
            turn: *turn,
            output_frontier: *output_frontier,
            result: Some(ReviewPassResult::ExternalLinkAttachment(
                ReviewExternalLinkAttachmentResult::new(
                    link,
                    external_object.clone(),
                    Some(ReviewFindingEventResult::new(
                        finding,
                        ordinal,
                        ReviewFindingEventResultKind::Posted { link },
                    )),
                ),
            )),
        },
        state => state.clone(),
    };
    let pass = pass_evidence(pass.reference(), pass.kind(), pass.policy(), state);
    ReviewExternalLinkAttachment::new(
        link,
        pass.reference(),
        pass.clone(),
        run_evidence_for_pass(pass),
        external_object,
    )
}

fn observation(
    link: ReviewExternalLinkId,
    ordinal: ReviewEventOrdinal,
    pass: ReviewPassEvidence,
    state: ReviewExternalObjectState,
) -> ReviewExternalLinkObservation {
    let pass_state = match pass.state() {
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            ..
        } => ReviewPassState::Succeeded {
            turn: *turn,
            output_frontier: *output_frontier,
            result: Some(ReviewPassResult::ExternalLinkObservation(
                ReviewExternalLinkObservationResult::new(link, ordinal, state),
            )),
        },
        pass_state => pass_state.clone(),
    };
    let pass = pass_evidence(pass.reference(), pass.kind(), pass.policy(), pass_state);
    ReviewExternalLinkObservation::new(
        link,
        ordinal,
        pass.reference(),
        pass.clone(),
        run_evidence_for_pass(pass),
        state,
    )
}

#[derive(Debug)]
struct FixedActivationIds {
    origin_entry: Option<SemanticTranscriptEntryId>,
    starting_frontier: Option<ContextFrontierId>,
    initial_attempt: Option<TurnAttemptId>,
}

impl StartEligibleTurnIdGenerator for FixedActivationIds {
    fn next_model_identity_entry_id(&mut self) -> SemanticTranscriptEntryId {
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(u128::MAX))
    }

    fn next_origin_entry_id(&mut self) -> SemanticTranscriptEntryId {
        self.origin_entry
            .take()
            .expect("one activation is expected")
    }

    fn next_starting_frontier_id(&mut self) -> ContextFrontierId {
        self.starting_frontier
            .take()
            .expect("one activation is expected")
    }

    fn next_initial_attempt_id(&mut self) -> TurnAttemptId {
        self.initial_attempt
            .take()
            .expect("one activation is expected")
    }
}

async fn insert_active_turn(
    pool: &PgPool,
    session: SessionId,
    accepted_input: AcceptedInputId,
    turn: TurnId,
) {
    insert_active_turn_with_offset(pool, session, accepted_input, turn, 0).await;
}

async fn insert_active_turn_with_offset(
    pool: &PgPool,
    session: SessionId,
    accepted_input: AcceptedInputId,
    turn: TurnId,
    offset: u128,
) {
    let create = CreateSession::new(
        DurableCommandId::from_uuid(uuid(0x101 + offset)),
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(uuid(0x102 + offset)),
        )),
    )
    .prepare(session)
    .expect("user-created fixture session is preparable");
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(create)
        .await
        .expect("fixture session persists");

    let submit = SubmitInput::new(
        DurableCommandId::from_uuid(uuid(0x103 + offset)),
        session,
        UserContent::try_text(String::from("Perform the bounded review pass"))
            .expect("fixture content is admitted"),
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            submit,
            accepted_input,
            Some(turn),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0x104 + offset)),
                ContextFrontierId::from_uuid(uuid(0x105 + offset)),
            ),
            |_| TurnId::from_uuid(uuid(0x106 + offset)),
            |requests| {
                (
                    requests
                        .iter()
                        .enumerate()
                        .map(|(index, _)| {
                            SemanticTranscriptEntryId::from_uuid(uuid(
                                0x110 + offset + u128::try_from(index).expect("small fixture"),
                            ))
                        })
                        .collect(),
                    ContextFrontierId::from_uuid(uuid(0x120 + offset)),
                )
            },
        )
        .await
        .expect("fixture input and queued turn persist");

    let mut activation = StartEligibleTurnService::new(
        FixedActivationIds {
            origin_entry: Some(SemanticTranscriptEntryId::from_uuid(uuid(0x130 + offset))),
            starting_frontier: Some(ContextFrontierId::from_uuid(uuid(0x131 + offset))),
            initial_attempt: Some(TurnAttemptId::from_uuid(uuid(0x132 + offset))),
        },
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let outcome = activation
        .execute(session)
        .await
        .expect("fixture turn activates");
    assert!(matches!(outcome, StartEligibleTurnOutcome::Activated(_)));
    record_empty_instruction_manifest(pool, session)
        .await
        .expect("fixture turn records its empty instruction manifest");
}

fn finding(
    reference: ReviewFindingRef,
    producing_pass: ReviewPassEvidence,
    target: &ReviewTarget,
) -> ReviewFinding {
    finding_with_confidence_axes_and_side(
        reference,
        producing_pass,
        target,
        FindingConfidenceAxes {
            is_real: 9_000,
            severity_label: 8_500,
        },
        Some(ReviewFindingDiffSide::Right),
    )
}

struct FindingConfidenceAxes {
    is_real: u16,
    severity_label: u16,
}

fn finding_with_is_real_confidence(
    reference: ReviewFindingRef,
    producing_pass: ReviewPassEvidence,
    target: &ReviewTarget,
    is_real_confidence: u16,
) -> ReviewFinding {
    finding_with_confidence_axes_and_side(
        reference,
        producing_pass,
        target,
        FindingConfidenceAxes {
            is_real: is_real_confidence,
            severity_label: 8_500,
        },
        Some(ReviewFindingDiffSide::Right),
    )
}

fn finding_with_side(
    reference: ReviewFindingRef,
    producing_pass: ReviewPassEvidence,
    target: &ReviewTarget,
    diff_side: Option<ReviewFindingDiffSide>,
) -> ReviewFinding {
    finding_with_confidence_axes_and_side(
        reference,
        producing_pass,
        target,
        FindingConfidenceAxes {
            is_real: 9_000,
            severity_label: 8_500,
        },
        diff_side,
    )
}

fn finding_with_confidence_axes_and_side(
    reference: ReviewFindingRef,
    producing_pass: ReviewPassEvidence,
    target: &ReviewTarget,
    confidence: FindingConfidenceAxes,
    diff_side: Option<ReviewFindingDiffSide>,
) -> ReviewFinding {
    let policy = producing_pass.policy();
    let state = match producing_pass.state() {
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            result,
        } => ReviewPassState::Succeeded {
            turn: *turn,
            output_frontier: *output_frontier,
            result: match result {
                Some(result @ ReviewPassResult::ProducedFindings(findings))
                    if !findings.findings().is_empty() =>
                {
                    Some(result.clone())
                }
                _ => Some(ReviewPassResult::ProducedFindings(
                    ReviewProducedFindings::try_new(vec![reference])
                        .expect("one fixture finding is a canonical inventory"),
                )),
            },
        },
        state => state.clone(),
    };
    let producing_pass = pass_evidence(
        producing_pass.reference(),
        producing_pass.kind(),
        policy,
        state,
    );
    ReviewFinding::new(
        ReviewFindingProposal::try_new(
            reference,
            producing_pass.clone(),
            ReviewRunEvidence::new(
                reference.run(),
                ReviewWorkflowKind::ReadOnlyReview,
                policy,
                ReviewRunState::Succeeded {
                    concluding_pass: reference.pass(),
                },
            ),
            target,
            ReviewFindingContent::new(
                ReviewFindingLocation::new(
                    key("src/review.rs"),
                    Some(ReviewLineRange::try_new(11, 14).expect("ordered fixture range")),
                    diff_side,
                ),
                text("Guard the exact evidence edge"),
                text("The transition must retain the producing turn."),
                ReviewFindingSeverity::High,
                ReviewFindingConfidenceAxes::new(
                    ReviewConfidence::try_from_basis_points(confidence.is_real)
                        .expect("fixture is-real confidence is bounded"),
                    ReviewConfidence::try_from_basis_points(confidence.severity_label)
                        .expect("fixture severity-label confidence is bounded"),
                ),
                key("correctness"),
                Some(text("Bind the transition to the complete pass reference.")),
            ),
        )
        .expect("fixture pass belongs to the finding run"),
    )
}

struct PersistedReviewPassFixture {
    pool: PgPool,
    store: ReviewWorkflowStore,
    target: ReviewTargetId,
    target_snapshot: ReviewTarget,
    run: ReviewRunRef,
    pass: ReviewPassRef,
}

async fn insert_review_pass_fixture(pool: &PgPool) -> PersistedReviewPassFixture {
    let store = ReviewWorkflowStore::new(pool.clone());
    let session = SessionId::from_uuid(uuid(0x201));
    let accepted_input = AcceptedInputId::from_uuid(uuid(0x202));
    let turn = TurnId::from_uuid(uuid(0x203));
    insert_active_turn(pool, session, accepted_input, turn).await;

    let target = ReviewTargetId::from_uuid(uuid(0x301));
    let target_snapshot = ReviewTarget::try_new(
        target,
        key("example-code-host"),
        key("example/repository"),
        ReviewTargetSubject::Commit,
        key("0123456789abcdef"),
        Some(key("fedcba9876543210")),
        None,
    )
    .expect("fixture target topology is valid");
    store
        .insert_target(&target_snapshot)
        .await
        .expect("target persists");
    let run = ReviewRunRef::new(target, ReviewRunId::from_uuid(uuid(0x302)));
    let pass = ReviewPassRef::new(run, ReviewPassId::from_uuid(uuid(0x303)));
    let mut run_value = ReviewRun::new(
        run,
        ReviewWorkflowKind::ReadOnlyReview,
        ReviewPolicy::version_one(),
    );
    let pass_value = ReviewPass::try_new(
        pass,
        ReviewPassKind::ReadOnlyReview,
        &mut run_value,
        session,
        ReviewPassAcceptedInputEvidence::new(accepted_input, session, Some(turn)),
    )
    .expect("accepted input belongs to the fixture session");
    store
        .insert_run(&run_value)
        .await
        .expect("queued run persists");
    store
        .insert_pass(&pass_value)
        .await
        .expect("queued pass persists");

    PersistedReviewPassFixture {
        pool: pool.clone(),
        store,
        target,
        target_snapshot,
        run,
        pass,
    }
}

struct ReviewCommandAdmissionFixture {
    store: ReviewWorkflowStore,
    target: ReviewTargetId,
    run: ReviewRun,
    pass: ReviewPass,
    session: SessionId,
    accepted_input: AcceptedInputId,
    origin_turn: TurnId,
}

async fn review_command_admission_fixture(pool: &PgPool) -> ReviewCommandAdmissionFixture {
    let store = ReviewWorkflowStore::new(pool.clone());
    let session = SessionId::from_uuid(uuid(0x771));
    let accepted_input = AcceptedInputId::from_uuid(uuid(0x772));
    let origin_turn = TurnId::from_uuid(uuid(0x773));
    insert_active_turn(pool, session, accepted_input, origin_turn).await;
    let target = ReviewTargetId::from_uuid(uuid(0x774));
    store
        .insert_target(
            &ReviewTarget::try_new(
                target,
                key("example-code-host"),
                key("example/admission-repository"),
                ReviewTargetSubject::Commit,
                key("admission-head"),
                Some(key("admission-base")),
                None,
            )
            .expect("admission target is valid"),
        )
        .await
        .expect("admission target persists");
    let run_reference = ReviewRunRef::new(target, ReviewRunId::from_uuid(uuid(0x775)));
    let pass_reference = ReviewPassRef::new(run_reference, ReviewPassId::from_uuid(uuid(0x776)));
    let mut run = ReviewRun::new(
        run_reference,
        ReviewWorkflowKind::ReadOnlyReview,
        ReviewPolicy::version_one(),
    );
    let pass = ReviewPass::try_new(
        pass_reference,
        ReviewPassKind::ReadOnlyReview,
        &mut run,
        session,
        ReviewPassAcceptedInputEvidence::new(accepted_input, session, Some(origin_turn)),
    )
    .expect("admission pass is valid");
    ReviewCommandAdmissionFixture {
        store,
        target,
        run,
        pass,
        session,
        accepted_input,
        origin_turn,
    }
}

async fn insert_fixture_pass(
    fixture: &PersistedReviewPassFixture,
    identity: u128,
    kind: ReviewPassKind,
) -> ReviewPassRef {
    insert_isolated_pass_for_target(
        &fixture.pool,
        &fixture.store,
        fixture.target,
        identity,
        kind,
    )
    .await
    .0
}

async fn insert_isolated_pass_for_target(
    pool: &PgPool,
    store: &ReviewWorkflowStore,
    target: ReviewTargetId,
    identity: u128,
    kind: ReviewPassKind,
) -> (ReviewPassRef, TurnId) {
    let session = SessionId::from_uuid(uuid(0x10_0000 + identity));
    let accepted_input = AcceptedInputId::from_uuid(uuid(0x20_0000 + identity));
    let turn = TurnId::from_uuid(uuid(0x20_0001 + identity));
    insert_active_turn_with_offset(
        pool,
        session,
        accepted_input,
        turn,
        0x40_0000 + identity * 0x100,
    )
    .await;
    (
        insert_pass_for_target(store, target, identity, kind, session, accepted_input).await,
        turn,
    )
}

async fn insert_pass_for_target(
    store: &ReviewWorkflowStore,
    target: ReviewTargetId,
    identity: u128,
    kind: ReviewPassKind,
    session: SessionId,
    accepted_input: AcceptedInputId,
) -> ReviewPassRef {
    insert_pass_for_target_with_policy(
        store,
        target,
        identity,
        kind,
        session,
        accepted_input,
        ReviewPolicy::version_one(),
    )
    .await
}

async fn insert_pass_for_target_with_policy(
    store: &ReviewWorkflowStore,
    target: ReviewTargetId,
    identity: u128,
    kind: ReviewPassKind,
    session: SessionId,
    accepted_input: AcceptedInputId,
    policy: ReviewPolicy,
) -> ReviewPassRef {
    let run = ReviewRunRef::new(target, ReviewRunId::from_uuid(uuid(identity + 0x1000)));
    let pass = ReviewPassRef::new(run, ReviewPassId::from_uuid(uuid(identity)));
    let mut run_value = ReviewRun::new(run, workflow_for_pass(kind), policy);
    let origin_turn = TurnId::from_uuid(Uuid::from_u128(
        accepted_input
            .into_uuid()
            .as_u128()
            .checked_add(1)
            .expect("fixture input identity has a successor"),
    ));
    let pass_value = ReviewPass::try_new(
        pass,
        kind,
        &mut run_value,
        session,
        ReviewPassAcceptedInputEvidence::new(accepted_input, session, Some(origin_turn)),
    )
    .expect("fixture input belongs to its session");
    store
        .insert_run(&run_value)
        .await
        .expect("additional fixture run persists");
    store
        .insert_pass(&pass_value)
        .await
        .expect("additional fixture pass persists");
    pass
}

async fn start_review_pass(
    store: &ReviewWorkflowStore,
    reference: ReviewPassRef,
) -> (ReviewPass, TurnId) {
    let turn = store
        .load_pass(reference.pass())
        .await
        .expect("pass origin loads")
        .expect("pass exists")
        .origin_turn();
    let (_, pass) = store
        .transition_run_and_pass(
            reference.run().run(),
            reference.pass(),
            ReviewRunState::Running {
                active_pass: reference,
            },
            ReviewPassState::Running { turn },
        )
        .await
        .expect("run/pass activation persists")
        .expect("fixture run and pass exist");
    (pass, turn)
}

const REVIEW_TURN_IDENTITY_NAMESPACE: u128 = 0xfeed_f00d_dead_beef_0000_0000_0000_0000;
const ARBITRARY_REVIEW_CREDENTIAL_REFERENCE: &str = "review-fixture-primary";
const ARBITRARY_REVIEW_RESPONSE: &str = "Bounded review fixture response";
const ARBITRARY_REVIEW_INTERRUPT_CONTENT: &str = "Continue after review reconciliation";

struct ReviewTurnTransitionIdentities {
    provider: ProviderModelIdentity,
    call: ModelCallId,
    resume_candidate_call: ModelCallId,
    initial_failure_entry: SemanticTranscriptEntryId,
    initial_failure_frontier: ContextFrontierId,
    initial_steering_frontier: ContextFrontierId,
    resume_failure_entry: SemanticTranscriptEntryId,
    resume_failure_frontier: ContextFrontierId,
    resume_steering_frontier: ContextFrontierId,
    assistant_entry: SemanticTranscriptEntryId,
    completion_entry: SemanticTranscriptEntryId,
    terminal_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
    interrupt_command: DurableCommandId,
    interrupt_input: AcceptedInputId,
    interrupt_successor: TurnId,
    interrupt_cancellation_entry: SemanticTranscriptEntryId,
    interrupt_cancellation_frontier: ContextFrontierId,
}

impl ReviewTurnTransitionIdentities {
    fn for_turn(turn: TurnId) -> Self {
        let mut next_value =
            REVIEW_TURN_IDENTITY_NAMESPACE ^ turn.into_uuid().as_u128().rotate_left(u128::BITS / 2);
        let mut next_uuid = || {
            next_value = next_value
                .checked_add(1)
                .expect("review fixture identity namespace is not exhausted");
            Uuid::from_u128(next_value)
        };
        Self {
            provider: ProviderModelIdentity::from_uuid(next_uuid()),
            call: ModelCallId::from_uuid(next_uuid()),
            resume_candidate_call: ModelCallId::from_uuid(next_uuid()),
            initial_failure_entry: SemanticTranscriptEntryId::from_uuid(next_uuid()),
            initial_failure_frontier: ContextFrontierId::from_uuid(next_uuid()),
            initial_steering_frontier: ContextFrontierId::from_uuid(next_uuid()),
            resume_failure_entry: SemanticTranscriptEntryId::from_uuid(next_uuid()),
            resume_failure_frontier: ContextFrontierId::from_uuid(next_uuid()),
            resume_steering_frontier: ContextFrontierId::from_uuid(next_uuid()),
            assistant_entry: SemanticTranscriptEntryId::from_uuid(next_uuid()),
            completion_entry: SemanticTranscriptEntryId::from_uuid(next_uuid()),
            terminal_entry: SemanticTranscriptEntryId::from_uuid(next_uuid()),
            terminal_frontier: ContextFrontierId::from_uuid(next_uuid()),
            interrupt_command: DurableCommandId::from_uuid(next_uuid()),
            interrupt_input: AcceptedInputId::from_uuid(next_uuid()),
            interrupt_successor: TurnId::from_uuid(next_uuid()),
            interrupt_cancellation_entry: SemanticTranscriptEntryId::from_uuid(next_uuid()),
            interrupt_cancellation_frontier: ContextFrontierId::from_uuid(next_uuid()),
        }
    }
}

struct PreparedReviewTurnCall {
    session: SessionId,
    repository: PostgresModelCallRepository,
    authorized: AuthorizedModelCall,
    identities: ReviewTurnTransitionIdentities,
}

async fn prepare_review_turn_call(pool: &PgPool, turn: TurnId) -> PreparedReviewTurnCall {
    #[derive(sqlx::FromRow)]
    struct StoredReviewTurnModelFacts {
        session_id: Uuid,
        direct_selection_id: Uuid,
    }

    let stored = sqlx::query_as::<_, StoredReviewTurnModelFacts>(
        "SELECT lifecycle.session_id,
                COALESCE(
                    origin.frozen_direct_model_selection_id,
                    origin.frozen_alias_selected_direct_id
                ) AS direct_selection_id
           FROM turn_lifecycle AS lifecycle
           JOIN queued_input_origin AS origin
             ON origin.turn_id = lifecycle.turn_id
            AND origin.session_id = lifecycle.session_id
          WHERE lifecycle.turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_one(pool)
    .await
    .expect("active review fixture turn has frozen model facts");
    let session = SessionId::from_uuid(stored.session_id);
    let selection = DirectModelSelection::from_uuid(stored.direct_selection_id);
    let identities = ReviewTurnTransitionIdentities::for_turn(turn);
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(identities.provider),
    )])
    .expect("one review fixture target forms a catalog");
    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        ModelCallCredentialReference::new(ARBITRARY_REVIEW_CREDENTIAL_REFERENCE),
    );
    let checkpointed = repository
        .prepare_initial_call(
            session,
            identities.call,
            FailedModelCallTurnIdentities::new(
                identities.initial_failure_entry,
                identities.initial_failure_frontier,
            ),
            identities.initial_steering_frontier,
            |_| panic!("review fixture has no pending steering"),
        )
        .await
        .expect("review fixture model call checkpoints");
    assert!(matches!(
        checkpointed,
        PrepareInitialModelCallOutcome::Checkpointed(call) if call == identities.call
    ));
    let resumed = repository
        .prepare_initial_call(
            session,
            identities.resume_candidate_call,
            FailedModelCallTurnIdentities::new(
                identities.resume_failure_entry,
                identities.resume_failure_frontier,
            ),
            identities.resume_steering_frontier,
            |_| panic!("review fixture has no pending steering"),
        )
        .await
        .expect("review fixture model call resumes");
    assert!(matches!(
        resumed,
        PrepareInitialModelCallOutcome::Ready { .. }
    ));
    let AuthorizeModelCallOutcome::Authorized(authorized) = repository
        .authorize_send(session, identities.call)
        .await
        .expect("review fixture model call authorizes")
    else {
        panic!("review fixture model call must be ready to authorize");
    };
    PreparedReviewTurnCall {
        session,
        repository,
        authorized: *authorized,
        identities,
    }
}

async fn complete_review_turn(pool: &PgPool, turn: TurnId) -> ContextFrontierId {
    let prepared = prepare_review_turn_call(pool, turn).await;
    let terminal = prepared
        .repository
        .apply_terminal_observation(
            prepared.session,
            prepared
                .authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::Completed {
                    assistant_text: vec![
                        AssistantText::try_new(String::from(ARBITRARY_REVIEW_RESPONSE))
                            .expect("review fixture assistant text is admitted"),
                    ],
                }),
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![prepared.identities.assistant_entry],
                prepared.identities.completion_entry,
                prepared.identities.terminal_frontier,
            )),
            |_| panic!("review fixture terminalization has no pending steering"),
        )
        .await
        .expect("review fixture model call completes");
    assert!(matches!(terminal, ModelCallTerminalOutcome::Completed(_)));
    prepared.identities.terminal_frontier
}

async fn fail_review_turn(pool: &PgPool, turn: TurnId) -> ContextFrontierId {
    let prepared = prepare_review_turn_call(pool, turn).await;
    let terminal = prepared
        .repository
        .apply_terminal_observation(
            prepared.session,
            prepared
                .authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed),
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                prepared.identities.terminal_entry,
                prepared.identities.terminal_frontier,
            )),
            |_| panic!("review fixture terminalization has no pending steering"),
        )
        .await
        .expect("review fixture model call fails");
    assert!(matches!(terminal, ModelCallTerminalOutcome::Failed(_)));
    prepared.identities.terminal_frontier
}

async fn reconcile_review_turn(pool: &PgPool, turn: TurnId) -> ContextFrontierId {
    let prepared = prepare_review_turn_call(pool, turn).await;
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            SubmitInput::new(
                prepared.identities.interrupt_command,
                prepared.session,
                UserContent::try_text(String::from(ARBITRARY_REVIEW_INTERRUPT_CONTENT))
                    .expect("review fixture interrupt content is admitted"),
                DeliveryRequest::Interrupt {
                    expected_active_turn: turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
            ),
            prepared.identities.interrupt_input,
            Some(prepared.identities.interrupt_successor),
            CancelledModelCallTurnIdentities::new(
                prepared.identities.interrupt_cancellation_entry,
                prepared.identities.interrupt_cancellation_frontier,
            ),
            |_| panic!("review fixture interrupt has no pending steering"),
            |_| panic!("review fixture interrupt has no tool batch"),
        )
        .await
        .expect("review fixture interrupt persists");
    let terminal = prepared
        .repository
        .apply_terminal_observation(
            prepared.session,
            prepared
                .authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::Ambiguous),
            ModelCallTerminalIdentities::Ambiguous(AmbiguousModelCallTurnIdentities::new(
                prepared.identities.terminal_frontier,
            )),
            |_| panic!("review fixture terminalization has no pending steering"),
        )
        .await
        .expect("review fixture model call requires reconciliation");
    assert!(matches!(
        terminal,
        ModelCallTerminalOutcome::ReconciliationRequired(_)
    ));
    prepared.identities.terminal_frontier
}

async fn conclude_review_pass(
    store: &ReviewWorkflowStore,
    reference: ReviewPassRef,
    next_pass: ReviewPassState,
) -> ReviewPassEvidence {
    let policy = store
        .load_run(reference.run().run())
        .await
        .expect("fixture run loads")
        .expect("fixture run exists")
        .policy();
    let next_run = match next_pass {
        ReviewPassState::Succeeded { .. } => ReviewRunState::Succeeded {
            concluding_pass: reference,
        },
        ReviewPassState::Failed { .. } => ReviewRunState::Failed {
            failed_pass: reference,
        },
        ReviewPassState::Blocked { .. } => ReviewRunState::Blocked {
            blocking_pass: reference,
        },
        ReviewPassState::Cancelled { turn: Some(_) } => ReviewRunState::Cancelled {
            last_pass: Some(reference),
        },
        ReviewPassState::Cancelled { turn: None } => ReviewRunState::Cancelled { last_pass: None },
        ReviewPassState::Queued | ReviewPassState::Running { .. } => {
            panic!("fixture helper accepts only terminal outcomes")
        }
    };
    let (_, pass) = store
        .transition_run_and_pass(reference.run().run(), reference.pass(), next_run, next_pass)
        .await
        .expect("run/pass conclusion persists")
        .expect("fixture run and pass exist");
    pass_evidence(pass.reference(), pass.kind(), policy, pass.state().clone())
}
async fn propose_read_only_success(
    store: &ReviewWorkflowStore,
    pass: ReviewPass,
    output_frontier: ContextFrontierId,
) -> ReviewPassEvidence {
    let reference = pass.reference();
    let policy = store
        .load_run(reference.run().run())
        .await
        .expect("fixture run loads")
        .expect("fixture run exists")
        .policy();
    let ReviewPassState::Running { turn } = pass.state() else {
        panic!("read-only success proposal requires a running pass");
    };
    let turn = *turn;
    let state = ReviewPassState::Succeeded {
        turn,
        output_frontier,
        result: Some(ReviewPassResult::ProducedFindings(
            ReviewProducedFindings::try_new(Vec::new())
                .expect("empty fixture inventory is canonical"),
        )),
    };
    let turn_evidence = ReviewPassTurnEvidence::new(
        turn,
        pass.session(),
        pass.accepted_input(),
        ReviewPassTurnOutcome::Completed,
        Some(output_frontier),
    );
    let terminal_pass = pass
        .transition(state, Some(turn_evidence))
        .expect("read-only fixture proposes its atomic inventory");
    ReviewPassEvidence::from_pass(&terminal_pass, policy)
}

async fn succeed_fixture_passes(
    pool: &PgPool,
    store: &ReviewWorkflowStore,
    references: &[ReviewPassRef],
) -> Vec<ReviewPassEvidence> {
    let mut terminal = Vec::with_capacity(references.len());
    for reference in references {
        let (pass, turn) = start_review_pass(store, *reference).await;
        let output_frontier = complete_review_turn(pool, turn).await;
        terminal.push((pass, turn, output_frontier));
    }
    let mut evidence = Vec::with_capacity(terminal.len());
    for (pass, turn, output_frontier) in terminal {
        let reference = pass.reference();
        if pass.kind() == ReviewPassKind::ReadOnlyReview {
            let proposed = propose_read_only_success(store, pass, output_frontier).await;
            evidence.push(proposed);
        } else {
            evidence.push(
                conclude_review_pass(
                    store,
                    reference,
                    ReviewPassState::Succeeded {
                        turn,
                        output_frontier,
                        result: None,
                    },
                )
                .await,
            );
        }
    }
    evidence
}

#[track_caller]
fn assert_read_only_success_requires_atomic_inventory(error: ReviewWorkflowStoreError) {
    let ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Pass(error)) =
        error
    else {
        panic!("missing read-only inventory must be a typed pass-transition rejection");
    };
    assert_eq!(
        error.failure(),
        ReviewPassTransitionFailure::IncompatibleResult
    );
}

#[track_caller]
fn assert_finding_reference_load_corruption(
    loaded: Result<Option<ReviewFinding>, ReviewWorkflowStoreError>,
    expected_aggregate: &str,
) {
    let error = loaded.expect_err("corrupt finding reference must fail loading closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed finding-reference corruption");
    };
    assert_eq!(
        error.aggregate(),
        expected_aggregate,
        "corruption detail: {}",
        error.detail(),
    );
}

async fn load_review_aggregate(
    store: &ReviewWorkflowStore,
    reference: ReviewPassRef,
) -> (ReviewRun, ReviewPass) {
    let run = store
        .load_run(reference.run().run())
        .await
        .expect("review run loads without corruption")
        .expect("review run exists");
    let pass = store
        .load_pass(reference.pass())
        .await
        .expect("review pass loads without corruption")
        .expect("review pass exists");
    (run, pass)
}

fn assert_sqlstate(error: &sqlx::Error, expected: &str) {
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.code())
            .as_deref(),
        Some(expected)
    );
}

#[track_caller]
fn assert_external_link_no_change_result(
    state: &ReviewPassState,
    expected: ReviewExternalLinkNoChangeResult,
) {
    let actual = match state {
        ReviewPassState::Succeeded {
            result: Some(ReviewPassResult::ExternalLinkNoChange(result)),
            ..
        } => *result,
        other => panic!("expected an external-link no-change result, got {other:?}"),
    };
    assert_eq!(actual, expected);
}

#[test]
fn external_link_no_change_assertion_accepts_the_exact_result() {
    let expected = ReviewExternalLinkNoChangeResult::new(
        ReviewExternalLinkId::from_uuid(uuid(0x190)),
        ReviewEventOrdinal::one(),
        ReviewExternalObjectState::Current,
    );
    let state = ReviewPassState::Succeeded {
        turn: TurnId::from_uuid(uuid(0x191)),
        output_frontier: ContextFrontierId::from_uuid(uuid(0x192)),
        result: Some(ReviewPassResult::ExternalLinkNoChange(expected)),
    };

    assert_external_link_no_change_result(&state, expected);
}

#[test]
#[should_panic(expected = "expected an external-link no-change result")]
fn external_link_no_change_assertion_rejects_another_result_shape() {
    let state = ReviewPassState::Succeeded {
        turn: TurnId::from_uuid(uuid(0x193)),
        output_frontier: ContextFrontierId::from_uuid(uuid(0x194)),
        result: None,
    };
    let expected = ReviewExternalLinkNoChangeResult::new(
        ReviewExternalLinkId::from_uuid(uuid(0x195)),
        ReviewEventOrdinal::one(),
        ReviewExternalObjectState::Current,
    );

    assert_external_link_no_change_result(&state, expected);
}

fn assert_concurrent_attachment_outcomes(
    first: Result<Option<ReviewExternalLink>, ReviewWorkflowStoreError>,
    second: Result<Option<ReviewExternalLink>, ReviewWorkflowStoreError>,
) {
    let constraint_rejection =
        |outcome: &Result<Option<ReviewExternalLink>, ReviewWorkflowStoreError>| {
            matches!(
                outcome,
                Err(ReviewWorkflowStoreError::Database(error))
                    if error
                        .as_database_error()
                        .and_then(|database| database.code())
                        .as_deref()
                        == Some("23514")
            )
        };
    assert!(
        (first.is_ok() && constraint_rejection(&second))
            || (second.is_ok() && constraint_rejection(&first)),
        "exactly one logical target must be admitted and the other constraint-rejected"
    );
}

/// the event-head migration retains the connection-selected workflow
/// schema and pins trigger lookups ahead of temporary objects.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn event_head_retains_configured_workflow_schema() -> Result<(), Box<dyn Error>> {
    const WORKFLOW_SCHEMA: &str = "configured_review_workflow";
    const PINNED_SEARCH_PATH: &str = "search_path=configured_review_workflow, pg_catalog, pg_temp";

    let (_container, pool) = migrated_postgres_in_configured_schema().await?;
    let current_schema: String = sqlx::query_scalar("SELECT current_schema()")
        .fetch_one(&pool)
        .await?;
    let head_schema: String = sqlx::query_scalar(
        "SELECT table_schema
           FROM information_schema.tables
          WHERE table_name = 'review_finding_event_head'",
    )
    .fetch_one(&pool)
    .await?;
    let transition_paths_are_pinned: bool = sqlx::query_scalar(
        "SELECT count(*) = 2
                AND bool_and($2 = ANY(function.proconfig))
           FROM pg_proc AS function
           JOIN pg_namespace AS namespace
             ON namespace.oid = function.pronamespace
          WHERE namespace.nspname = $1
            AND function.proname IN (
                'authenticate_review_finding_event_head',
                'advance_review_finding_event_head'
            )",
    )
    .bind(WORKFLOW_SCHEMA)
    .bind(PINNED_SEARCH_PATH)
    .fetch_one(&pool)
    .await?;

    assert_eq!(current_schema, WORKFLOW_SCHEMA);
    assert_eq!(head_schema, WORKFLOW_SCHEMA);
    assert!(transition_paths_are_pinned);
    Ok(())
}

/// the store reconstructs complete workflow evidence,
/// including the canonical reservation, attachment, and observation sequence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_workflow_store_reconstructs_complete_evidence() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres_with_max_connections(1).await?;
    let store = ReviewWorkflowStore::new(pool.clone());
    let session = SessionId::from_uuid(uuid(0x201));
    let accepted_input = AcceptedInputId::from_uuid(uuid(0x202));
    let turn = TurnId::from_uuid(uuid(0x203));
    insert_active_turn(&pool, session, accepted_input, turn).await;

    let target_id = ReviewTargetId::from_uuid(uuid(0x301));
    let target = ReviewTarget::try_new(
        target_id,
        key("example-code-host"),
        key("example/repository"),
        ReviewTargetSubject::ChangeRequest(
            ReviewChangeRequestNumber::try_new(42).expect("positive change request"),
        ),
        key("0123456789abcdef"),
        Some(key("fedcba9876543210")),
        None,
    )
    .expect("fixture target topology is valid");
    store.insert_target(&target).await.expect("target persists");
    assert_eq!(
        store.load_target(target_id).await.expect("target loads"),
        Some(target.clone())
    );

    let run_ref = ReviewRunRef::new(target_id, ReviewRunId::from_uuid(uuid(0x302)));
    let pass_ref = ReviewPassRef::new(run_ref, ReviewPassId::from_uuid(uuid(0x303)));
    let mut run = ReviewRun::new(
        run_ref,
        ReviewWorkflowKind::ReadOnlyReview,
        ReviewPolicy::version_one(),
    );
    let pass = ReviewPass::try_new(
        pass_ref,
        ReviewPassKind::ReadOnlyReview,
        &mut run,
        session,
        ReviewPassAcceptedInputEvidence::new(accepted_input, session, Some(turn)),
    )
    .expect("accepted input belongs to the fixture session");
    store.insert_run(&run).await.expect("queued run persists");
    store
        .insert_pass(&pass)
        .await
        .expect("queued pass persists");
    let (judge_pass, judge_turn) =
        insert_isolated_pass_for_target(&pool, &store, target_id, 0x306, ReviewPassKind::Judge)
            .await;
    let (publish_pass, publish_turn) =
        insert_isolated_pass_for_target(&pool, &store, target_id, 0x307, ReviewPassKind::Publish)
            .await;
    let (import_pass, import_turn) = insert_isolated_pass_for_target(
        &pool,
        &store,
        target_id,
        0x308,
        ReviewPassKind::ImportExternalContext,
    )
    .await;
    let (unchanged_import_pass, unchanged_import_turn) = insert_isolated_pass_for_target(
        &pool,
        &store,
        target_id,
        0x309,
        ReviewPassKind::ImportExternalContext,
    )
    .await;
    let (running_review, _) = start_review_pass(&store, pass_ref).await;
    start_review_pass(&store, judge_pass).await;
    start_review_pass(&store, publish_pass).await;
    start_review_pass(&store, import_pass).await;
    start_review_pass(&store, unchanged_import_pass).await;
    let output_frontier = complete_review_turn(&pool, turn).await;
    let judge_output_frontier = complete_review_turn(&pool, judge_turn).await;
    let publish_output_frontier = complete_review_turn(&pool, publish_turn).await;
    let import_output_frontier = complete_review_turn(&pool, import_turn).await;
    let unchanged_import_output_frontier = complete_review_turn(&pool, unchanged_import_turn).await;
    let review_evidence = propose_read_only_success(&store, running_review, output_frontier).await;
    let judge_evidence = conclude_review_pass(
        &store,
        judge_pass,
        ReviewPassState::Succeeded {
            turn: judge_turn,
            output_frontier: judge_output_frontier,
            result: None,
        },
    )
    .await;
    let publish_evidence = conclude_review_pass(
        &store,
        publish_pass,
        ReviewPassState::Succeeded {
            turn: publish_turn,
            output_frontier: publish_output_frontier,
            result: None,
        },
    )
    .await;
    let import_evidence = conclude_review_pass(
        &store,
        import_pass,
        ReviewPassState::Succeeded {
            turn: import_turn,
            output_frontier: import_output_frontier,
            result: None,
        },
    )
    .await;
    let unchanged_import_evidence = conclude_review_pass(
        &store,
        unchanged_import_pass,
        ReviewPassState::Succeeded {
            turn: unchanged_import_turn,
            output_frontier: unchanged_import_output_frontier,
            result: None,
        },
    )
    .await;

    let finding_ref = ReviewFindingRef::new(pass_ref, ReviewFindingId::from_uuid(uuid(0x304)));
    let open_finding = finding(finding_ref, review_evidence, &target);
    store
        .insert_finding(&open_finding)
        .await
        .expect("open finding persists");
    let accepted_event = finding_event(
        finding_ref,
        ReviewEventOrdinal::one(),
        judge_evidence,
        ReviewFindingEventKind::Accepted,
    );
    let accepted_finding = open_finding
        .clone()
        .apply(accepted_event.clone())
        .expect("open finding accepts judgment");
    assert_eq!(
        store
            .append_finding_event(finding_ref.finding(), accepted_event)
            .await
            .expect("finding event persists"),
        Some(accepted_finding.clone())
    );

    let link_id = ReviewExternalLinkId::from_uuid(uuid(0x305));
    let reservation = ReviewExternalLink::try_reserve(
        link_id,
        ReviewExternalLinkAssociation::Finding(finding_ref),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
        &target,
    )
    .expect("reservation matches the target");
    assert_eq!(
        store
            .reserve_external_link(reservation.clone())
            .await
            .expect("first reservation persists"),
        ReserveExternalLinkOutcome::Inserted(reservation.clone())
    );
    assert_eq!(
        store
            .reserve_external_link(reservation.clone())
            .await
            .expect("equal replay loads the canonical reservation"),
        ReserveExternalLinkOutcome::Existing(reservation.clone())
    );
    let conflicting = ReviewExternalLink::try_reserve(
        link_id,
        ReviewExternalLinkAssociation::Finding(finding_ref),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewThread,
        &target,
    )
    .expect("conflicting payload remains target-valid");
    assert!(matches!(
        store.reserve_external_link(conflicting).await,
        Err(ReviewWorkflowStoreError::ReservationConflict(_))
    ));

    let posted_ordinal = ReviewEventOrdinal::try_new(2).expect("positive ordinal");
    let attachment = posted_attachment(
        link_id,
        publish_evidence,
        key("comment-84"),
        finding_ref,
        posted_ordinal,
    );
    let attached = reservation
        .clone()
        .attach(attachment.clone())
        .expect("same-target pass may attach");
    assert_eq!(
        store
            .attach_external_link(link_id, attachment)
            .await
            .expect("attachment persists"),
        Some(attached.clone())
    );
    let attachment_evidence = attached
        .attachment()
        .expect("attached link carries the producing pass")
        .pass_evidence()
        .clone();
    let posted_event = ReviewFindingEvent::new(
        finding_ref,
        posted_ordinal,
        attachment_evidence.reference(),
        attachment_evidence.clone(),
        run_evidence_for_pass(attachment_evidence),
        ReviewFindingEventKind::Posted {
            link: Box::new(
                signalbox_domain::ReviewFindingExternalLinkRef::try_new(finding_ref, &attached)
                    .expect("attached canonical link belongs to the finding"),
            ),
        },
    );
    let posted_finding = accepted_finding
        .apply(posted_event)
        .expect("accepted finding may record an attached publication");
    assert_eq!(
        store
            .load_finding(finding_ref.finding())
            .await
            .expect("atomically posted finding loads"),
        Some(posted_finding.clone())
    );
    sqlx::query(
        "ALTER TABLE review_external_link
         DROP CONSTRAINT review_external_link_finding_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_external_link
         DISABLE TRIGGER review_external_link_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_external_link
            SET finding_producing_pass_id = $1
          WHERE external_link_id = $2",
    )
    .bind(judge_pass.pass().into_uuid())
    .bind(link_id.into_uuid())
    .execute(&pool)
    .await?;
    let error = store
        .load_finding(finding_ref.finding())
        .await
        .expect_err("finding loading must authenticate the stored link producer");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed finding-history corruption");
    };
    assert_eq!(error.aggregate(), "review_external_link");
    assert!(
        error
            .detail()
            .contains("finding producing pass row is missing"),
        "unexpected corruption detail: {}",
        error.detail(),
    );
    sqlx::query(
        "UPDATE review_external_link
            SET finding_producing_pass_id = $1
          WHERE external_link_id = $2",
    )
    .bind(pass_ref.pass().into_uuid())
    .bind(link_id.into_uuid())
    .execute(&pool)
    .await?;
    let first_observation = observation(
        link_id,
        ReviewEventOrdinal::one(),
        import_evidence,
        ReviewExternalObjectState::Current,
    );
    let observed = attached
        .observe(first_observation.clone())
        .expect("first observation is contiguous");
    assert_eq!(
        store
            .append_external_observation(link_id, first_observation)
            .await
            .expect("observation persists"),
        Some(observed.clone())
    );
    assert_eq!(
        store
            .load_external_link(link_id)
            .await
            .expect("complete link loads"),
        Some(observed.clone())
    );
    assert_eq!(
        store
            .load_finding(finding_ref.finding())
            .await
            .expect("posted finding loads through observed link history"),
        Some(posted_finding.clone())
    );
    let unchanged = observation(
        link_id,
        ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
        unchanged_import_evidence,
        ReviewExternalObjectState::Current,
    );
    let unchanged_link = store
        .append_external_observation(link_id, unchanged)
        .await
        .expect("unchanged polling is a semantic no-op")
        .expect("the canonical link remains present");
    let unchanged_pass = store
        .load_pass(unchanged_import_pass.pass())
        .await?
        .expect("unchanged import pass remains durable");
    assert_external_link_no_change_result(
        unchanged_pass.state(),
        ReviewExternalLinkNoChangeResult::new(
            link_id,
            ReviewEventOrdinal::one(),
            ReviewExternalObjectState::Current,
        ),
    );
    let unchanged_evidence =
        ReviewPassEvidence::from_pass(&unchanged_pass, ReviewPolicy::version_one());
    let expected_unchanged = observed
        .confirm_unchanged(
            unchanged_evidence.clone(),
            run_evidence_for_pass(unchanged_evidence),
        )
        .expect("the durable no-change result authenticates its claim");
    assert_eq!(unchanged_link, expected_unchanged);
    assert_eq!(
        store
            .load_external_link(link_id)
            .await
            .expect("no-change claim reloads"),
        Some(expected_unchanged.clone())
    );
    assert_eq!(
        store
            .load_finding(finding_ref.finding())
            .await
            .expect("posted finding loads through a durable no-change claim"),
        Some(posted_finding.clone())
    );
    let (later_import_pass, later_import_turn) = insert_isolated_pass_for_target(
        &pool,
        &store,
        target_id,
        0x30a,
        ReviewPassKind::ImportExternalContext,
    )
    .await;
    start_review_pass(&store, later_import_pass).await;
    let later_output_frontier = complete_review_turn(&pool, later_import_turn).await;
    let later_import_evidence = conclude_review_pass(
        &store,
        later_import_pass,
        ReviewPassState::Succeeded {
            turn: later_import_turn,
            output_frontier: later_output_frontier,
            result: None,
        },
    )
    .await;
    let later_observation = observation(
        link_id,
        ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
        later_import_evidence,
        ReviewExternalObjectState::Outdated,
    );
    let expected_advanced = expected_unchanged
        .observe(later_observation.clone())
        .expect("later changed state advances the observation frontier");
    assert_eq!(
        store
            .append_external_observation(link_id, later_observation)
            .await
            .expect("later changed state persists"),
        Some(expected_advanced.clone())
    );
    assert_eq!(
        store
            .load_external_link(link_id)
            .await
            .expect("historical no-change claim reloads after later state"),
        Some(expected_advanced)
    );
    assert_eq!(
        store
            .load_finding(finding_ref.finding())
            .await
            .expect("posted finding loads after later external observations"),
        Some(posted_finding)
    );
    let unrelated_target = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(0x30b)),
        key("example-code-host"),
        key("example/repository"),
        ReviewTargetSubject::Commit,
        key("unrelated-head"),
        None,
        None,
    )
    .expect("unrelated target is structurally valid");
    store.insert_target(&unrelated_target).await?;
    sqlx::query(
        "ALTER TABLE review_external_object_identity
         DISABLE TRIGGER review_external_object_identity_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_external_object_identity
            SET logical_target_id = $1
          WHERE provider_key = 'example-code-host'
            AND object_kind = 'review_comment'
            AND external_object_key = 'comment-84'",
    )
    .bind(unrelated_target.id().into_uuid())
    .execute(&pool)
    .await?;
    let identity_error = store
        .load_finding(finding_ref.finding())
        .await
        .expect_err("finding history must authenticate the external-object registry");
    let ReviewWorkflowStoreError::Corruption(identity_error) = identity_error else {
        panic!("expected typed finding-history corruption");
    };
    assert_eq!(
        identity_error.aggregate(),
        "review_external_link_attachment"
    );
    assert!(identity_error.detail().contains("unrelated logical target"));
    sqlx::query(
        "UPDATE review_external_object_identity
            SET logical_target_id = $1
          WHERE provider_key = 'example-code-host'
            AND object_kind = 'review_comment'
            AND external_object_key = 'comment-84'",
    )
    .bind(target_id.into_uuid())
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_change_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_event_ordinal = 2
          WHERE pass_id = $1",
    )
    .bind(unchanged_import_pass.pass().into_uuid())
    .execute(&pool)
    .await?;
    let error = store
        .load_external_link(link_id)
        .await
        .expect_err("a no-change claim cannot consume a stale observation frontier");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed external-link corruption");
    };
    assert_eq!(error.aggregate(), "review_external_link");
    assert!(
        error.detail().contains("IncompatibleObservationPass"),
        "unexpected corruption detail: {}",
        error.detail(),
    );

    Ok(())
}

/// pass loading validates the accepted input's canonical session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_loader_rejects_cross_wired_accepted_input() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let pass = fixture.pass.pass();
    let other_session = SessionId::from_uuid(uuid(0x211));
    let other_input = AcceptedInputId::from_uuid(uuid(0x212));
    let other_turn = TurnId::from_uuid(uuid(0x213));
    insert_active_turn_with_offset(&pool, other_session, other_input, other_turn, 0x1_000).await;

    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_change_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_run_projection_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DROP CONSTRAINT review_pass_accepted_input_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DROP CONSTRAINT review_pass_origin_turn_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET session_id = $2
          WHERE pass_id = $1",
    )
    .bind(pass.into_uuid())
    .bind(other_session.into_uuid())
    .execute(&pool)
    .await?;
    let session_error = fixture
        .store
        .load_pass(pass)
        .await
        .expect_err("canonical accepted-input session must reject cross-wiring");
    let ReviewWorkflowStoreError::Corruption(session_error) = session_error else {
        panic!("expected typed review-pass corruption");
    };
    assert_eq!(session_error.aggregate(), "review_pass");
    assert!(
        session_error
            .detail()
            .contains("AcceptedInputSessionMismatch")
    );
    sqlx::query(
        "UPDATE review_pass
            SET session_id = $2
          WHERE pass_id = $1",
    )
    .bind(pass.into_uuid())
    .bind(uuid(0x201))
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE accepted_input
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE accepted_input
            SET origin_turn_id = $2
          WHERE accepted_input_id = $1",
    )
    .bind(uuid(0x202))
    .bind(uuid(0x214))
    .execute(&pool)
    .await?;
    let origin_error = fixture
        .store
        .load_pass(pass)
        .await
        .expect_err("canonical accepted-input origin must reject cross-wiring");
    let ReviewWorkflowStoreError::Corruption(origin_error) = origin_error else {
        panic!("expected typed review-pass corruption");
    };
    assert_eq!(origin_error.aggregate(), "review_pass");
    assert!(
        origin_error
            .detail()
            .contains("accepted input origin turn differs")
    );

    Ok(())
}

/// pass loading authenticates the queued pass's exact origin turn,
/// independently of its accepted-input snapshot.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_loader_rejects_missing_origin_turn() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let missing_turn = uuid(0x21f);

    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_change_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DROP CONSTRAINT review_pass_origin_turn_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE accepted_input DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE accepted_input
            SET origin_turn_id = $2
          WHERE accepted_input_id = $1",
    )
    .bind(uuid(0x202))
    .bind(missing_turn)
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET origin_turn_id = $2
          WHERE pass_id = $1",
    )
    .bind(fixture.pass.pass().into_uuid())
    .bind(missing_turn)
    .execute(&pool)
    .await?;

    let error = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await
        .expect_err("missing canonical origin turn must fail pass loading closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed review-pass corruption");
    };
    assert_eq!(error.aggregate(), "review_pass");
    assert!(error.detail().contains("origin turn row is missing"));
    Ok(())
}

/// a pass whose canonical target row is missing is corruption, even
/// when its run row remains present.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_loader_rejects_missing_target() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    sqlx::query(
        "ALTER TABLE review_run
         DROP CONSTRAINT review_run_target_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_target
         DISABLE TRIGGER review_target_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DELETE FROM review_target WHERE target_id = $1")
        .bind(fixture.target.into_uuid())
        .execute(&pool)
        .await?;

    let error = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await
        .expect_err("missing canonical target must fail pass loading closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed review-pass corruption");
    };
    assert_eq!(error.aggregate(), "review_pass");
    assert!(error.detail().contains("target row is missing"));
    Ok(())
}

/// an accepted orchestration input is owned by at most one review
/// pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn accepted_input_owns_at_most_one_review_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let run_ref = ReviewRunRef::new(fixture.target, ReviewRunId::from_uuid(uuid(0x215)));
    let pass_ref = ReviewPassRef::new(run_ref, ReviewPassId::from_uuid(uuid(0x216)));
    let mut run = ReviewRun::new(
        run_ref,
        ReviewWorkflowKind::ReadOnlyReview,
        ReviewPolicy::version_one(),
    );
    let duplicate = ReviewPass::try_new(
        pass_ref,
        ReviewPassKind::ReadOnlyReview,
        &mut run,
        SessionId::from_uuid(uuid(0x201)),
        ReviewPassAcceptedInputEvidence::new(
            AcceptedInputId::from_uuid(uuid(0x202)),
            SessionId::from_uuid(uuid(0x201)),
            Some(TurnId::from_uuid(uuid(0x203))),
        ),
    )
    .expect("domain construction cannot inspect the global pass inventory");
    fixture.store.insert_run(&run).await?;

    let error = fixture
        .store
        .insert_pass(&duplicate)
        .await
        .expect_err("the canonical accepted input already belongs to a pass");
    let ReviewWorkflowStoreError::Database(error) = error else {
        panic!("expected the unique ownership constraint to reject the pass");
    };
    assert_sqlstate(&error, "23505");
    Ok(())
}

/// pass loading validates the referenced turn's canonical session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_loader_rejects_cross_wired_turn() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let pass = fixture.pass.pass();
    let other_session = SessionId::from_uuid(uuid(0x211));
    let other_input = AcceptedInputId::from_uuid(uuid(0x212));
    let other_turn = TurnId::from_uuid(uuid(0x213));
    insert_active_turn_with_offset(&pool, other_session, other_input, other_turn, 0x1_000).await;

    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_change_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_run_projection_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DROP CONSTRAINT review_pass_turn_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET state_kind = 'running',
                turn_id = $2
          WHERE pass_id = $1",
    )
    .bind(pass.into_uuid())
    .bind(other_turn.into_uuid())
    .execute(&pool)
    .await?;
    let turn_error = fixture
        .store
        .load_pass(pass)
        .await
        .expect_err("canonical turn ownership must reject cross-wiring");
    let ReviewWorkflowStoreError::Corruption(turn_error) = turn_error else {
        panic!("expected typed review-pass corruption");
    };
    assert_eq!(turn_error.aggregate(), "review_pass");
    assert!(
        turn_error.detail().contains("TurnOriginMismatch"),
        "unexpected corruption detail: {}",
        turn_error.detail()
    );

    Ok(())
}

/// a run projection may report only the canonical outcome of its
/// referenced pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn run_projection_rejects_noncanonical_pass_outcome() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    start_review_pass(&fixture.store, fixture.pass).await;

    let guarded = sqlx::query(
        "UPDATE review_run
            SET state_kind = 'succeeded'
          WHERE run_id = $1",
    )
    .bind(fixture.run.run().into_uuid())
    .execute(&pool)
    .await
    .expect_err("run success requires a canonically succeeded pass");
    assert_sqlstate(&guarded, "23514");

    sqlx::query(
        "ALTER TABLE review_run
         DISABLE TRIGGER review_run_change_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_run
         DISABLE TRIGGER review_run_pass_projection_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_run
            SET state_kind = 'succeeded'
          WHERE run_id = $1",
    )
    .bind(fixture.run.run().into_uuid())
    .execute(&pool)
    .await?;
    let error = fixture
        .store
        .load_run(fixture.run.run())
        .await
        .expect_err("loader must reject a run/pass outcome contradiction");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed review-run corruption");
    };
    assert_eq!(error.aggregate(), "review_run");
    assert!(error.detail().contains("PassStateMismatch"));

    Ok(())
}

/// loading a pass validates the canonical state projection of its run.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_loader_rejects_noncanonical_run_projection() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;

    sqlx::query(
        "ALTER TABLE review_run
         DISABLE TRIGGER review_run_change_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_run
         DISABLE TRIGGER review_run_pass_projection_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_run
            SET state_kind = 'cancelled',
                state_pass_id = NULL
          WHERE run_id = $1",
    )
    .bind(fixture.run.run().into_uuid())
    .execute(&pool)
    .await?;

    let error = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await
        .expect_err("pass loading must reject a contradictory canonical run");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed review-run corruption");
    };
    assert_eq!(error.aggregate(), "review_run");
    assert!(error.detail().contains("UnexpectedPassEvidence"));
    Ok(())
}

/// a pass projection may report only the canonical outcome of its
/// referenced turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_projection_rejects_noncanonical_turn_outcome() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    start_review_pass(&fixture.store, fixture.pass).await;

    let guarded = sqlx::query(
        "UPDATE review_pass
            SET state_kind = 'failed'
          WHERE pass_id = $1",
    )
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("pass failure requires a canonical terminal turn");
    assert_sqlstate(&guarded, "23514");

    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_change_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_run_projection_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET state_kind = 'failed'
          WHERE pass_id = $1",
    )
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await?;
    let error = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await
        .expect_err("loader must reject a pass/turn outcome contradiction");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed review-pass corruption");
    };
    assert_eq!(error.aggregate(), "review_pass");
    assert!(error.detail().contains("TurnOutcomeMismatch"));

    Ok(())
}

/// pass failure is the workflow-operation outcome and may follow a
/// canonically completed turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn failed_pass_accepts_completed_turn_evidence() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let (_, turn) = start_review_pass(&fixture.store, fixture.pass).await;
    complete_review_turn(&pool, turn).await;

    let evidence = conclude_review_pass(
        &fixture.store,
        fixture.pass,
        ReviewPassState::Failed { turn },
    )
    .await;

    assert_eq!(evidence.state(), &ReviewPassState::Failed { turn });
    assert_eq!(
        fixture
            .store
            .load_pass(fixture.pass.pass())
            .await?
            .expect("failed pass remains loadable")
            .state(),
        &ReviewPassState::Failed { turn }
    );
    Ok(())
}

/// lifecycle-only transition APIs reject an effect result before
/// changing the pass or run projection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn generic_transition_rejects_effect_result() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let (_, turn) = start_review_pass(&fixture.store, fixture.pass).await;
    let output_frontier = complete_review_turn(&pool, turn).await;
    let no_findings =
        ReviewProducedFindings::try_new(Vec::new()).expect("empty inventory is canonical");

    let error = fixture
        .store
        .transition_run_and_pass(
            fixture.run.run(),
            fixture.pass.pass(),
            ReviewRunState::Succeeded {
                concluding_pass: fixture.pass,
            },
            ReviewPassState::Succeeded {
                turn,
                output_frontier,
                result: Some(ReviewPassResult::ProducedFindings(no_findings)),
            },
        )
        .await
        .expect_err("effect result requires its effect-owning transaction");
    assert!(matches!(
        error,
        ReviewWorkflowStoreError::NonAtomicPassResult
    ));
    assert_eq!(
        fixture
            .store
            .load_pass(fixture.pass.pass())
            .await?
            .expect("rejected transition leaves the pass loadable")
            .state(),
        &ReviewPassState::Running { turn }
    );
    Ok(())
}

/// canonical read-only success admission is atomic, so every committed
/// intermediate aggregate remains loadable rather than appearing corrupt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn read_only_success_admission_is_atomic_and_always_loadable() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;

    let (queued_run, queued_pass) = load_review_aggregate(&fixture.store, fixture.pass).await;
    assert_eq!(queued_run.state(), ReviewRunState::Queued);
    assert_eq!(queued_pass.state(), &ReviewPassState::Queued);

    let (running, turn) = start_review_pass(&fixture.store, fixture.pass).await;
    let running_run_state = ReviewRunState::Running {
        active_pass: fixture.pass,
    };
    assert_eq!(running.state(), &ReviewPassState::Running { turn });
    let (loaded_run, loaded_pass) = load_review_aggregate(&fixture.store, fixture.pass).await;
    assert_eq!(loaded_run.state(), running_run_state);
    assert_eq!(loaded_pass.state(), running.state());

    let output_frontier = complete_review_turn(&pool, turn).await;
    let (loaded_run, loaded_pass) = load_review_aggregate(&fixture.store, fixture.pass).await;
    assert_eq!(loaded_run.state(), running_run_state);
    assert_eq!(loaded_pass.state(), running.state());

    let unbound_success = ReviewPassState::Succeeded {
        turn,
        output_frontier,
        result: None,
    };
    let error = fixture
        .store
        .transition_run_and_pass(
            fixture.run.run(),
            fixture.pass.pass(),
            ReviewRunState::Succeeded {
                concluding_pass: fixture.pass,
            },
            unbound_success,
        )
        .await
        .expect_err("read-only success without its inventory is rejected before commit");
    assert_read_only_success_requires_atomic_inventory(error);
    let (loaded_run, loaded_pass) = load_review_aggregate(&fixture.store, fixture.pass).await;
    assert_eq!(loaded_run.state(), running_run_state);
    assert_eq!(loaded_pass.state(), running.state());

    let no_finding_references = Vec::new();
    let no_findings = Vec::<ReviewFinding>::new();
    let produced_findings = ReviewPassResult::ProducedFindings(
        ReviewProducedFindings::try_new(no_finding_references)
            .expect("empty inventory is canonical"),
    );
    let completed_turn = ReviewPassTurnEvidence::new(
        turn,
        running.session(),
        running.accepted_input(),
        ReviewPassTurnOutcome::Completed,
        Some(output_frontier),
    );
    let succeeded = running
        .transition(
            ReviewPassState::Succeeded {
                turn,
                output_frontier,
                result: Some(produced_findings),
            },
            Some(completed_turn),
        )
        .expect("completed read-only pass proposes its exact inventory");
    let evidence = ReviewPassEvidence::from_pass(&succeeded, queued_run.policy());
    fixture
        .store
        .insert_findings(&evidence, &no_findings)
        .await?;

    let (loaded_run, loaded_pass) = load_review_aggregate(&fixture.store, fixture.pass).await;
    assert_eq!(loaded_pass.state(), evidence.state());
    assert_eq!(
        loaded_run.state(),
        ReviewRunState::Succeeded {
            concluding_pass: fixture.pass,
        }
    );
    Ok(())
}

/// once a produced-finding inventory is sealed, later canonical
/// finding inserts cannot expand the result—even when the inventory was empty.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn sealed_finding_inventory_cannot_expand() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let succeeded = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass]).await[0].clone();
    let evidence = pass_with_produced_findings(Vec::new(), succeeded);
    fixture.store.insert_findings(&evidence, &[]).await?;

    let expansion = sqlx::query(
        "INSERT INTO review_finding
             (finding_id, run_id, target_id, producing_pass_id, file_path,
             line_start, line_end, diff_side, title, body, severity,
             is_real_confidence, severity_label_confidence, category,
             recommended_fix)
         VALUES (
             $1, $2, $3, $4, 'src/lib.rs',
             1, 1, 'right', 'Late finding', 'Body', 'high',
             9000, 8500, 'correctness', NULL
         )",
    )
    .bind(uuid(0x329))
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("sealed finding inventory cannot admit a late finding");
    assert_sqlstate(&expansion, "23514");
    Ok(())
}

/// pass loading authenticates both directions of the sealed
/// produced-finding inventory against canonical finding rows.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_loader_rejects_incomplete_finding_inventory() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let succeeded = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass]).await[0].clone();
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x328)));
    let evidence = pass_with_produced_findings(vec![finding_ref], succeeded);
    fixture
        .store
        .insert_finding(&finding(finding_ref, evidence, &fixture.target_snapshot))
        .await?;
    sqlx::query(
        "ALTER TABLE review_pass_produced_finding
         DISABLE TRIGGER review_pass_produced_finding_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_pass_produced_finding
            SET result_ordinal = 2
          WHERE pass_id = $1",
    )
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await?;
    let ordinal_error = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await
        .expect_err("non-contiguous inventory ordinals must fail pass loading closed");
    let ReviewWorkflowStoreError::Corruption(ordinal_error) = ordinal_error else {
        panic!("expected typed produced-finding ordinal corruption");
    };
    assert_eq!(ordinal_error.aggregate(), "review_pass_produced_finding");
    assert!(ordinal_error.detail().contains("ordinals"));
    sqlx::query(
        "UPDATE review_pass_produced_finding
            SET result_ordinal = 1
          WHERE pass_id = $1",
    )
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM review_pass_produced_finding
          WHERE pass_id = $1",
    )
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await?;

    let error = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await
        .expect_err("missing inventory member must fail pass loading closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed produced-finding inventory corruption");
    };
    assert_eq!(error.aggregate(), "review_pass_produced_finding");
    assert!(error.detail().contains("sealed findings"));
    Ok(())
}

/// finding-event validation reuses its held transaction connection,
/// so a one-connection pool cannot self-deadlock while loading current history.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_event_uses_held_transaction_connection() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres_with_max_connections(1).await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x32a, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x32b)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            review_evidence,
            &fixture.target_snapshot,
        ))
        .await?;
    let append = fixture.store.append_finding_event(
        finding_ref.finding(),
        finding_event(
            finding_ref,
            ReviewEventOrdinal::one(),
            evidence[1].clone(),
            ReviewFindingEventKind::Accepted,
        ),
    );
    let appended = tokio::time::timeout(std::time::Duration::from_secs(5), append)
        .await
        .expect("held-transaction loading must not wait for another pool connection")?;
    assert_eq!(
        appended.expect("finding remains present").status(),
        ReviewFindingStatus::Accepted
    );
    Ok(())
}

/// appending an event through another same-run finding fails before
/// persistence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_event_rejects_foreign_owner() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let review_evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass]).await[0].clone();
    let first = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x330)));
    let second = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x331)));
    let review_evidence = pass_with_produced_findings(vec![first, second], review_evidence);
    fixture
        .store
        .insert_findings(
            &review_evidence,
            &[
                finding(first, review_evidence.clone(), &fixture.target_snapshot),
                finding(second, review_evidence.clone(), &fixture.target_snapshot),
            ],
        )
        .await?;

    let error = fixture
        .store
        .append_finding_event(
            first.finding(),
            finding_event(
                second,
                ReviewEventOrdinal::one(),
                succeeded_pass(fixture.pass, ReviewPassKind::Judge),
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await
        .expect_err("event owner must equal the loaded finding");
    let ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Finding(error)) =
        error
    else {
        panic!("expected a typed finding transition rejection");
    };
    assert_eq!(
        error.failure(),
        ReviewFindingTransitionFailure::ForeignEventFinding
    );

    Ok(())
}

/// a referenced finding reconstitutes with its exact producing pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn referenced_finding_retains_producing_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let canonical_pass = insert_fixture_pass(&fixture, 0x332, ReviewPassKind::ReadOnlyReview).await;
    let dedupe_pass = insert_fixture_pass(&fixture, 0x333, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, canonical_pass, dedupe_pass],
    )
    .await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x330)));
    let canonical_ref =
        ReviewFindingRef::new(canonical_pass, ReviewFindingId::from_uuid(uuid(0x331)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    let canonical_evidence = pass_with_produced_findings(vec![canonical_ref], evidence[1].clone());
    let dedupe_evidence = evidence[2].clone();
    let open = finding(
        finding_ref,
        review_evidence.clone(),
        &fixture.target_snapshot,
    );
    let canonical = finding(
        canonical_ref,
        canonical_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_findings(&review_evidence, std::slice::from_ref(&open))
        .await?;
    fixture
        .store
        .insert_findings(&canonical_evidence, std::slice::from_ref(&canonical))
        .await?;
    let event = finding_event(
        finding_ref,
        ReviewEventOrdinal::one(),
        dedupe_evidence,
        ReviewFindingEventKind::Duplicate {
            canonical: ReviewReferencedFindingEvidence::try_from_finding(&canonical)
                .expect("open finding is eligible reference evidence"),
        },
    );
    let expected = open
        .apply(event.clone())
        .expect("dedupe pass may identify the canonical finding");
    fixture
        .store
        .append_finding_event(finding_ref.finding(), event)
        .await?;

    let retry = fixture
        .store
        .transition_run_and_pass(
            dedupe_pass.run().run(),
            dedupe_pass.pass(),
            ReviewRunState::Queued,
            ReviewPassState::Queued,
        )
        .await
        .expect_err("terminal referenced result rejects a later pass transition cleanly");

    assert!(matches!(
        retry,
        ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Pass(_))
    ));
    assert_eq!(
        fixture.store.load_finding(finding_ref.finding()).await?,
        Some(expected)
    );
    Ok(())
}

/// reference admission and reconstitution observe terminalization
/// that commits while waiting for the relational transition barrier.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn reference_refreshes_after_terminalization_wait() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let canonical_pass = insert_fixture_pass(&fixture, 0x8a1, ReviewPassKind::ReadOnlyReview).await;
    let rejection_pass = insert_fixture_pass(&fixture, 0x8a2, ReviewPassKind::Judge).await;
    let dedupe_pass = insert_fixture_pass(&fixture, 0x8a3, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, canonical_pass, rejection_pass, dedupe_pass],
    )
    .await;
    let subject_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x8a4)));
    let canonical_ref =
        ReviewFindingRef::new(canonical_pass, ReviewFindingId::from_uuid(uuid(0x8a5)));
    let subject_evidence = pass_with_produced_findings(vec![subject_ref], evidence[0].clone());
    let canonical_evidence = pass_with_produced_findings(vec![canonical_ref], evidence[1].clone());
    let subject = finding(
        subject_ref,
        subject_evidence.clone(),
        &fixture.target_snapshot,
    );
    let canonical = finding(
        canonical_ref,
        canonical_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_findings(&subject_evidence, std::slice::from_ref(&subject))
        .await?;
    fixture
        .store
        .insert_findings(&canonical_evidence, std::slice::from_ref(&canonical))
        .await?;
    let reason = text("the canonical finding is no longer actionable");
    let rejection = finding_event(
        canonical_ref,
        ReviewEventOrdinal::one(),
        evidence[2].clone(),
        ReviewFindingEventKind::Rejected {
            reason: reason.clone(),
        },
    );
    let duplicate = finding_event(
        subject_ref,
        ReviewEventOrdinal::one(),
        evidence[3].clone(),
        ReviewFindingEventKind::Duplicate {
            canonical: ReviewReferencedFindingEvidence::try_from_finding(&canonical)
                .expect("open canonical finding is reference evidence"),
        },
    );
    let rejected = canonical
        .apply(rejection)
        .expect("the judge may reject the canonical finding");

    let mut terminalizing = pool.begin().await?;
    sqlx::query(
        "SELECT finding_id
           FROM review_finding
          WHERE finding_id = $1
          FOR NO KEY UPDATE",
    )
    .bind(canonical_ref.finding().into_uuid())
    .fetch_one(&mut *terminalizing)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = 1,
                result_event_kind = 'rejected',
                result_reason = $5
          WHERE pass_id = $1",
    )
    .bind(rejection_pass.pass().into_uuid())
    .bind(canonical_ref.finding().into_uuid())
    .bind(canonical_ref.run().run().into_uuid())
    .bind(canonical_ref.pass().pass().into_uuid())
    .bind(reason.as_str())
    .execute(&mut *terminalizing)
    .await?;
    sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, referenced_finding_run_id,
             referenced_finding_target_id, referenced_finding_pass_id,
             referenced_finding_status, external_link_id,
             external_link_association_kind)
         VALUES (
             $1, 1, $2, $3, $4, $5, 'rejected', $6,
             NULL, NULL, NULL, NULL, NULL, NULL, NULL
         )",
    )
    .bind(canonical_ref.finding().into_uuid())
    .bind(canonical_ref.run().run().into_uuid())
    .bind(canonical_ref.target().into_uuid())
    .bind(rejection_pass.pass().into_uuid())
    .bind(rejection_pass.run().run().into_uuid())
    .bind(reason.as_str())
    .execute(&mut *terminalizing)
    .await?;

    let mut appending_transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = $5,
                result_event_kind = 'duplicate',
                result_referenced_finding_id = $6,
                result_referenced_finding_run_id = $7,
                result_referenced_finding_target_id = $8,
                result_referenced_finding_pass_id = $9,
                result_referenced_finding_status = 'open'
          WHERE pass_id = $1",
    )
    .bind(duplicate.pass().pass().into_uuid())
    .bind(duplicate.finding().finding().into_uuid())
    .bind(duplicate.finding().run().run().into_uuid())
    .bind(duplicate.finding().pass().pass().into_uuid())
    .bind(i64::from(duplicate.ordinal().get()))
    .bind(canonical_ref.finding().into_uuid())
    .bind(canonical_ref.run().run().into_uuid())
    .bind(canonical_ref.target().into_uuid())
    .bind(canonical_ref.pass().pass().into_uuid())
    .execute(&mut *appending_transaction)
    .await?;
    let appending = tokio::spawn(async move {
        sqlx::query(
            "INSERT INTO review_finding_event
                (finding_id, event_ordinal, finding_run_id, target_id,
                 event_pass_id, event_pass_run_id, event_kind, reason,
                 referenced_finding_id, referenced_finding_run_id,
                 referenced_finding_target_id, referenced_finding_pass_id,
                 referenced_finding_status, external_link_id,
                 external_link_association_kind)
             VALUES (
                 $1, $2, $3, $4, $5, $6, 'duplicate', NULL,
                 $7, $8, $9, $10, 'open', NULL, NULL
             )",
        )
        .bind(duplicate.finding().finding().into_uuid())
        .bind(i64::from(duplicate.ordinal().get()))
        .bind(duplicate.finding().run().run().into_uuid())
        .bind(duplicate.finding().target().into_uuid())
        .bind(duplicate.pass().pass().into_uuid())
        .bind(duplicate.pass().run().run().into_uuid())
        .bind(canonical_ref.finding().into_uuid())
        .bind(canonical_ref.run().run().into_uuid())
        .bind(canonical_ref.target().into_uuid())
        .bind(canonical_ref.pass().pass().into_uuid())
        .execute(&mut *appending_transaction)
        .await?;
        appending_transaction.commit().await
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "reference admission waits for the canonical finding lock"
    );
    terminalizing.commit().await?;
    let error = appending
        .await
        .expect("reference admission task remains live")
        .expect_err("terminal canonical status must reject the stale reference");
    assert_sqlstate(&error, "23514");
    assert_eq!(
        fixture.store.load_finding(canonical_ref.finding()).await?,
        Some(rejected)
    );
    assert_eq!(
        fixture.store.load_finding(subject_ref.finding()).await?,
        Some(subject)
    );
    Ok(())
}

/// finding reconstitution rejects a mutable head that does not name
/// the exact latest append-only event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_load_rejects_mismatched_event_head() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x8b1, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x8b2)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    let open = finding(
        finding_ref,
        review_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_finding(&open)
        .await
        .expect("open finding persists");
    fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            finding_event(
                finding_ref,
                ReviewEventOrdinal::one(),
                evidence[1].clone(),
                ReviewFindingEventKind::Rejected {
                    reason: text("the finding is not actionable"),
                },
            ),
        )
        .await
        .expect("rejected finding event persists");

    sqlx::query(
        "ALTER TABLE review_finding_event_head
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_finding_event_head
            SET status = $2
          WHERE finding_id = $1",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind("accepted")
    .execute(&pool)
    .await?;

    assert_finding_reference_load_corruption(
        fixture.store.load_finding(finding_ref.finding()).await,
        "review_finding_event_head",
    );
    Ok(())
}

/// a relational caller cannot forge an event head and then append a
/// later event while omitting the event that supposedly established the head.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_rejects_forged_head_with_gapped_history() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let fix_pass = insert_fixture_pass(&fixture, 0x8d1, ReviewPassKind::Fix).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, fix_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x8d2)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    let open = finding(finding_ref, review_evidence, &fixture.target_snapshot);
    fixture.store.insert_finding(&open).await?;

    let attack: Result<(), sqlx::Error> = async {
        let mut transaction = pool.begin().await?;
        sqlx::query(
            "UPDATE review_finding_event_head
                SET event_ordinal = 1,
                    status = 'accepted',
                    event_pass_kind = 'judge',
                    external_link_id = NULL
              WHERE finding_id = $1",
        )
        .bind(finding_ref.finding().into_uuid())
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE review_pass
                SET result_kind = 'finding_event',
                    result_finding_id = $2,
                    result_finding_run_id = $3,
                    result_finding_pass_id = $4,
                    result_event_ordinal = 2,
                    result_event_kind = 'fixed'
              WHERE pass_id = $1",
        )
        .bind(fix_pass.pass().into_uuid())
        .bind(finding_ref.finding().into_uuid())
        .bind(finding_ref.run().run().into_uuid())
        .bind(finding_ref.pass().pass().into_uuid())
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO review_finding_event
                (finding_id, event_ordinal, finding_run_id, target_id,
                 event_pass_id, event_pass_run_id, event_kind, reason,
                 referenced_finding_id, referenced_finding_run_id,
                 referenced_finding_target_id, referenced_finding_pass_id,
                 referenced_finding_status, external_link_id,
                 external_link_association_kind)
             VALUES (
                 $1, 2, $2, $3, $4, $5, 'fixed', NULL,
                 NULL, NULL, NULL, NULL, NULL, NULL, NULL
             )",
        )
        .bind(finding_ref.finding().into_uuid())
        .bind(finding_ref.run().run().into_uuid())
        .bind(finding_ref.target().into_uuid())
        .bind(fix_pass.pass().into_uuid())
        .bind(fix_pass.run().run().into_uuid())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await
    }
    .await;

    let error = attack.expect_err("a head cannot advance without its exact durable event");
    assert_sqlstate(&error, "23514");
    assert_eq!(
        fixture.store.load_finding(finding_ref.finding()).await?,
        Some(open)
    );
    Ok(())
}

/// a non-posting attachment associated with a finding waits
/// for a concurrent finding transition before loading the aggregate projection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn attachment_load_waits_for_finding_transition() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x8e1, ReviewPassKind::Judge).await;
    let fix_pass = insert_fixture_pass(&fixture, 0x8e2, ReviewPassKind::Fix).await;
    let attaching_pass =
        insert_fixture_pass(&fixture, 0x8e3, ReviewPassKind::ImportExternalContext).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, judge_pass, fix_pass, attaching_pass],
    )
    .await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x8e4)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    let open = finding(finding_ref, review_evidence, &fixture.target_snapshot);
    fixture.store.insert_finding(&open).await?;
    let accepted_event = finding_event(
        finding_ref,
        ReviewEventOrdinal::one(),
        evidence[1].clone(),
        ReviewFindingEventKind::Accepted,
    );
    let accepted = open
        .apply(accepted_event.clone())
        .expect("judge accepts the open finding");
    fixture
        .store
        .append_finding_event(finding_ref.finding(), accepted_event)
        .await?;
    let fixed_event = finding_event(
        finding_ref,
        ReviewEventOrdinal::try_new(2).expect("second ordinal is valid"),
        evidence[2].clone(),
        ReviewFindingEventKind::Fixed,
    );
    let fixed = accepted
        .apply(fixed_event.clone())
        .expect("fix pass closes the accepted finding");

    let link = ReviewExternalLinkId::from_uuid(uuid(0x8e5));
    let reservation = ReviewExternalLink::try_reserve(
        link,
        ReviewExternalLinkAssociation::Finding(finding_ref),
        key("example-code-host"),
        ReviewExternalObjectKind::Commit,
        &fixture.target_snapshot,
    )
    .expect("reservation matches the finding target");
    fixture
        .store
        .reserve_external_link(reservation.clone())
        .await?;
    let attachment = attachment(link, evidence[3].clone(), key("commit-8e5"));
    let expected_link = reservation
        .attach(attachment.clone())
        .expect("same-target pass may attach");

    let mut transitioning = pool.begin().await?;
    sqlx::query(
        "SELECT finding_id
           FROM review_finding
          WHERE finding_id = ANY($1::uuid[])
          ORDER BY finding_id
          FOR NO KEY UPDATE",
    )
    .bind(vec![finding_ref.finding().into_uuid()])
    .fetch_all(&mut *transitioning)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = $5,
                result_event_kind = 'fixed'
          WHERE pass_id = $1",
    )
    .bind(fixed_event.pass().pass().into_uuid())
    .bind(fixed_event.finding().finding().into_uuid())
    .bind(fixed_event.finding().run().run().into_uuid())
    .bind(fixed_event.finding().pass().pass().into_uuid())
    .bind(i64::from(fixed_event.ordinal().get()))
    .execute(&mut *transitioning)
    .await?;

    let attaching_store = fixture.store.clone();
    let attaching =
        tokio::spawn(async move { attaching_store.attach_external_link(link, attachment).await });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "attachment waits for the associated finding transition lock"
    );
    sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, referenced_finding_run_id,
             referenced_finding_target_id, referenced_finding_pass_id,
             referenced_finding_status, external_link_id,
             external_link_association_kind)
         VALUES (
             $1, $2, $3, $4, $5, $6, 'fixed', NULL,
             NULL, NULL, NULL, NULL, NULL, NULL, NULL
         )",
    )
    .bind(fixed_event.finding().finding().into_uuid())
    .bind(i64::from(fixed_event.ordinal().get()))
    .bind(fixed_event.finding().run().run().into_uuid())
    .bind(fixed_event.finding().target().into_uuid())
    .bind(fixed_event.pass().pass().into_uuid())
    .bind(fixed_event.pass().run().run().into_uuid())
    .execute(&mut *transitioning)
    .await?;
    transitioning.commit().await?;

    assert_eq!(
        attaching.await.expect("attachment task remains live")?,
        Some(expected_link)
    );
    assert_eq!(
        fixture.store.load_finding(finding_ref.finding()).await?,
        Some(fixed)
    );
    Ok(())
}

/// a direct event insert that waits behind its uncommitted predecessor
/// authenticates the post-wait head and admits the next ordinal.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn event_sequence_admits_committed_predecessor_after_wait() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x8c1, ReviewPassKind::Judge).await;
    let fix_pass = insert_fixture_pass(&fixture, 0x8c2, ReviewPassKind::Fix).await;
    let evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass, fix_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x8c3)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    let open = finding(
        finding_ref,
        review_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_finding(&open)
        .await
        .expect("open finding persists");
    let accepted_event = finding_event(
        finding_ref,
        ReviewEventOrdinal::one(),
        evidence[1].clone(),
        ReviewFindingEventKind::Accepted,
    );
    let accepted = open
        .clone()
        .apply(accepted_event.clone())
        .expect("judge accepts the open finding");
    let fixed_event = finding_event(
        finding_ref,
        ReviewEventOrdinal::try_new(2).expect("second ordinal is valid"),
        evidence[2].clone(),
        ReviewFindingEventKind::Fixed,
    );
    let fixed = accepted
        .apply(fixed_event.clone())
        .expect("fix pass closes the accepted finding");

    let mut first_appender = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = $5,
                result_event_kind = 'accepted'
          WHERE pass_id = $1",
    )
    .bind(accepted_event.pass().pass().into_uuid())
    .bind(accepted_event.finding().finding().into_uuid())
    .bind(accepted_event.finding().run().run().into_uuid())
    .bind(accepted_event.finding().pass().pass().into_uuid())
    .bind(i64::from(accepted_event.ordinal().get()))
    .execute(&mut *first_appender)
    .await?;
    sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, referenced_finding_run_id,
             referenced_finding_target_id, referenced_finding_pass_id,
             referenced_finding_status, external_link_id,
             external_link_association_kind)
         VALUES (
             $1, $2, $3, $4, $5, $6, 'accepted', NULL,
             NULL, NULL, NULL, NULL, NULL, NULL, NULL
         )",
    )
    .bind(accepted_event.finding().finding().into_uuid())
    .bind(i64::from(accepted_event.ordinal().get()))
    .bind(accepted_event.finding().run().run().into_uuid())
    .bind(accepted_event.finding().target().into_uuid())
    .bind(accepted_event.pass().pass().into_uuid())
    .bind(accepted_event.pass().run().run().into_uuid())
    .execute(&mut *first_appender)
    .await?;

    let mut second_appender = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = $5,
                result_event_kind = 'fixed'
          WHERE pass_id = $1",
    )
    .bind(fixed_event.pass().pass().into_uuid())
    .bind(fixed_event.finding().finding().into_uuid())
    .bind(fixed_event.finding().run().run().into_uuid())
    .bind(fixed_event.finding().pass().pass().into_uuid())
    .bind(i64::from(fixed_event.ordinal().get()))
    .execute(&mut *second_appender)
    .await?;
    let waiting_append = tokio::spawn(async move {
        sqlx::query(
            "INSERT INTO review_finding_event
                (finding_id, event_ordinal, finding_run_id, target_id,
                 event_pass_id, event_pass_run_id, event_kind, reason,
                 referenced_finding_id, referenced_finding_run_id,
                 referenced_finding_target_id, referenced_finding_pass_id,
                 referenced_finding_status, external_link_id,
                 external_link_association_kind)
             VALUES (
                 $1, $2, $3, $4, $5, $6, 'fixed', NULL,
                 NULL, NULL, NULL, NULL, NULL, NULL, NULL
             )",
        )
        .bind(fixed_event.finding().finding().into_uuid())
        .bind(i64::from(fixed_event.ordinal().get()))
        .bind(fixed_event.finding().run().run().into_uuid())
        .bind(fixed_event.finding().target().into_uuid())
        .bind(fixed_event.pass().pass().into_uuid())
        .bind(fixed_event.pass().run().run().into_uuid())
        .execute(&mut *second_appender)
        .await?;
        second_appender.commit().await
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "second ordinal waits for its predecessor's finding lock"
    );
    first_appender.commit().await?;
    waiting_append
        .await
        .expect("second event task remains live")
        .expect("second event observes and follows its committed predecessor");

    assert_eq!(
        fixture.store.load_finding(finding_ref.finding()).await?,
        Some(fixed)
    );
    Ok(())
}

/// a superseded event round-trips a successor from another sealed
/// producer run without rewriting either finding's ancestry.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cross_run_superseded_retains_independent_ancestry() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let successor_pass =
        insert_fixture_pass(&fixture, 0x4332, ReviewPassKind::ReadOnlyReview).await;
    let dedupe_pass = insert_fixture_pass(&fixture, 0x4333, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, successor_pass, dedupe_pass],
    )
    .await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x4330)));
    let successor_ref =
        ReviewFindingRef::new(successor_pass, ReviewFindingId::from_uuid(uuid(0x4331)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    let successor_evidence = pass_with_produced_findings(vec![successor_ref], evidence[1].clone());
    let open = finding(
        finding_ref,
        review_evidence.clone(),
        &fixture.target_snapshot,
    );
    let successor = finding(
        successor_ref,
        successor_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_findings(&review_evidence, std::slice::from_ref(&open))
        .await?;
    fixture
        .store
        .insert_findings(&successor_evidence, std::slice::from_ref(&successor))
        .await?;
    let event = finding_event(
        finding_ref,
        ReviewEventOrdinal::one(),
        evidence[2].clone(),
        ReviewFindingEventKind::Superseded {
            successor: ReviewReferencedFindingEvidence::try_from_finding(&successor)
                .expect("open finding is eligible successor evidence"),
        },
    );
    let expected = open
        .apply(event.clone())
        .expect("dedupe pass may identify the successor finding");
    fixture
        .store
        .append_finding_event(finding_ref.finding(), event)
        .await?;

    assert_eq!(
        fixture.store.load_finding(finding_ref.finding()).await?,
        Some(expected)
    );
    Ok(())
}

/// the persistence boundary rejects a complete reference whose
/// authenticated producer belongs to another immutable target.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn store_rejects_cross_target_finding_reference() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let foreign_target = ReviewTargetId::from_uuid(uuid(0x6330));
    let foreign_snapshot = ReviewTarget::try_new(
        foreign_target,
        key("example-code-host"),
        key("example/repository"),
        ReviewTargetSubject::Commit,
        key("foreign-head"),
        Some(key("foreign-base")),
        None,
    )
    .expect("foreign target is valid");
    fixture.store.insert_target(&foreign_snapshot).await?;
    let foreign_producer = insert_isolated_pass_for_target(
        &pool,
        &fixture.store,
        foreign_target,
        0x6331,
        ReviewPassKind::ReadOnlyReview,
    )
    .await
    .0;
    let dedupe_pass = insert_fixture_pass(&fixture, 0x6332, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, foreign_producer, dedupe_pass],
    )
    .await;
    let subject_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x6333)));
    let foreign_ref =
        ReviewFindingRef::new(foreign_producer, ReviewFindingId::from_uuid(uuid(0x6334)));
    let subject_evidence = pass_with_produced_findings(vec![subject_ref], evidence[0].clone());
    let foreign_evidence = pass_with_produced_findings(vec![foreign_ref], evidence[1].clone());
    let subject = finding(
        subject_ref,
        subject_evidence.clone(),
        &fixture.target_snapshot,
    );
    let foreign = finding(foreign_ref, foreign_evidence.clone(), &foreign_snapshot);
    fixture
        .store
        .insert_findings(&subject_evidence, std::slice::from_ref(&subject))
        .await?;
    fixture
        .store
        .insert_findings(&foreign_evidence, std::slice::from_ref(&foreign))
        .await?;
    let error = sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = 1,
                result_event_kind = 'duplicate',
                result_referenced_finding_id = $5,
                result_referenced_finding_run_id = $6,
                result_referenced_finding_target_id = $7,
                result_referenced_finding_pass_id = $8,
                result_referenced_finding_status = 'open'
          WHERE pass_id = $1",
    )
    .bind(dedupe_pass.pass().into_uuid())
    .bind(subject_ref.finding().into_uuid())
    .bind(subject_ref.run().run().into_uuid())
    .bind(subject_ref.pass().pass().into_uuid())
    .bind(foreign_ref.finding().into_uuid())
    .bind(foreign_ref.run().run().into_uuid())
    .bind(foreign_ref.target().into_uuid())
    .bind(foreign_ref.pass().pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("cross-target reference must fail relational admission");
    assert_sqlstate(&error, "23514");
    Ok(())
}

/// reconstitution rejects a referenced producer whose durable frozen
/// policy differs or whose canonical pass is no longer read-only review.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn loader_rejects_reference_policy_or_producer_mismatch() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let canonical_pass =
        insert_fixture_pass(&fixture, 0x6431, ReviewPassKind::ReadOnlyReview).await;
    let dedupe_pass = insert_fixture_pass(&fixture, 0x6432, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, canonical_pass, dedupe_pass],
    )
    .await;
    let subject_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x6433)));
    let canonical_ref =
        ReviewFindingRef::new(canonical_pass, ReviewFindingId::from_uuid(uuid(0x6434)));
    let subject_evidence = pass_with_produced_findings(vec![subject_ref], evidence[0].clone());
    let canonical_evidence = pass_with_produced_findings(vec![canonical_ref], evidence[1].clone());
    let subject = finding(
        subject_ref,
        subject_evidence.clone(),
        &fixture.target_snapshot,
    );
    let canonical = finding(
        canonical_ref,
        canonical_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_findings(&subject_evidence, std::slice::from_ref(&subject))
        .await?;
    fixture
        .store
        .insert_findings(&canonical_evidence, std::slice::from_ref(&canonical))
        .await?;
    fixture
        .store
        .append_finding_event(
            subject_ref.finding(),
            finding_event(
                subject_ref,
                ReviewEventOrdinal::one(),
                evidence[2].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_from_finding(&canonical)
                        .expect("open canonical finding is reference evidence"),
                },
            ),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE review_run
         DROP CONSTRAINT review_run_confidence_bounds",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_run
         DISABLE TRIGGER review_run_change_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_run
            SET minimum_judge_confidence = 7001
          WHERE run_id = $1",
    )
    .bind(canonical_ref.run().run().into_uuid())
    .execute(&pool)
    .await?;

    let error = fixture
        .store
        .load_finding(subject_ref.finding())
        .await
        .expect_err("different frozen producer policy must fail loading closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed referenced-policy corruption");
    };
    assert_finding_reference_load_corruption(
        Err(ReviewWorkflowStoreError::Corruption(error)),
        "review_pass",
    );
    sqlx::query(
        "UPDATE review_run
            SET minimum_judge_confidence = 7000
          WHERE run_id = $1",
    )
    .bind(canonical_ref.run().run().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_change_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET pass_kind = 'judge'
          WHERE pass_id = $1",
    )
    .bind(canonical_ref.pass().pass().into_uuid())
    .execute(&pool)
    .await?;
    assert_finding_reference_load_corruption(
        fixture.store.load_finding(subject_ref.finding()).await,
        "review_pass",
    );
    Ok(())
}

/// target/run/pass/finding legs are independently retained; corrupting
/// any one leg cannot be normalized into a plausible reference during load.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn loader_rejects_each_cross_wired_reference_ancestry_leg() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let canonical_pass =
        insert_fixture_pass(&fixture, 0x6531, ReviewPassKind::ReadOnlyReview).await;
    let dedupe_pass = insert_fixture_pass(&fixture, 0x6532, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, canonical_pass, dedupe_pass],
    )
    .await;
    let subject_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x6533)));
    let canonical_ref =
        ReviewFindingRef::new(canonical_pass, ReviewFindingId::from_uuid(uuid(0x6534)));
    let subject_evidence = pass_with_produced_findings(vec![subject_ref], evidence[0].clone());
    let canonical_evidence = pass_with_produced_findings(vec![canonical_ref], evidence[1].clone());
    let subject = finding(
        subject_ref,
        subject_evidence.clone(),
        &fixture.target_snapshot,
    );
    let canonical = finding(
        canonical_ref,
        canonical_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_findings(&subject_evidence, std::slice::from_ref(&subject))
        .await?;
    fixture
        .store
        .insert_findings(&canonical_evidence, std::slice::from_ref(&canonical))
        .await?;
    fixture
        .store
        .append_finding_event(
            subject_ref.finding(),
            finding_event(
                subject_ref,
                ReviewEventOrdinal::one(),
                evidence[2].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_from_finding(&canonical)
                        .expect("open canonical finding is reference evidence"),
                },
            ),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE review_finding_event
         DROP CONSTRAINT review_finding_event_referenced_finding_fk,
         DROP CONSTRAINT review_finding_event_referenced_inventory_fk,
         DROP CONSTRAINT review_finding_event_referenced_ancestry_shape,
         DISABLE TRIGGER review_finding_event_is_append_only",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "UPDATE review_finding_event
            SET referenced_finding_target_id = $2
          WHERE finding_id = $1",
    )
    .bind(subject_ref.finding().into_uuid())
    .bind(uuid(0x65f1))
    .execute(&pool)
    .await?;
    assert_finding_reference_load_corruption(
        fixture.store.load_finding(subject_ref.finding()).await,
        "review_finding_event",
    );
    sqlx::query(
        "UPDATE review_finding_event
            SET referenced_finding_target_id = $2,
                referenced_finding_run_id = $3
          WHERE finding_id = $1",
    )
    .bind(subject_ref.finding().into_uuid())
    .bind(canonical_ref.target().into_uuid())
    .bind(uuid(0x65f2))
    .execute(&pool)
    .await?;
    assert_finding_reference_load_corruption(
        fixture.store.load_finding(subject_ref.finding()).await,
        "review_finding_event",
    );
    sqlx::query(
        "UPDATE review_finding_event
            SET referenced_finding_run_id = $2,
                referenced_finding_pass_id = $3
          WHERE finding_id = $1",
    )
    .bind(subject_ref.finding().into_uuid())
    .bind(canonical_ref.run().run().into_uuid())
    .bind(uuid(0x65f3))
    .execute(&pool)
    .await?;
    assert_finding_reference_load_corruption(
        fixture.store.load_finding(subject_ref.finding()).await,
        "review_finding_event",
    );
    sqlx::query(
        "UPDATE review_finding_event
            SET referenced_finding_pass_id = $2,
                referenced_finding_id = $3
          WHERE finding_id = $1",
    )
    .bind(subject_ref.finding().into_uuid())
    .bind(canonical_ref.pass().pass().into_uuid())
    .bind(uuid(0x65f4))
    .execute(&pool)
    .await?;
    assert_finding_reference_load_corruption(
        fixture.store.load_finding(subject_ref.finding()).await,
        "review_finding_event",
    );
    Ok(())
}

/// referenced producer reconstitution requires both its immutable
/// inventory seal and the exact referenced finding member.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn loader_rejects_unsealed_or_nonmember_referenced_producer() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let canonical_pass =
        insert_fixture_pass(&fixture, 0x6631, ReviewPassKind::ReadOnlyReview).await;
    let dedupe_pass = insert_fixture_pass(&fixture, 0x6632, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, canonical_pass, dedupe_pass],
    )
    .await;
    let subject_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x6633)));
    let canonical_ref =
        ReviewFindingRef::new(canonical_pass, ReviewFindingId::from_uuid(uuid(0x6634)));
    let subject_evidence = pass_with_produced_findings(vec![subject_ref], evidence[0].clone());
    let canonical_evidence = pass_with_produced_findings(vec![canonical_ref], evidence[1].clone());
    let subject = finding(
        subject_ref,
        subject_evidence.clone(),
        &fixture.target_snapshot,
    );
    let canonical = finding(
        canonical_ref,
        canonical_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_findings(&subject_evidence, std::slice::from_ref(&subject))
        .await?;
    fixture
        .store
        .insert_findings(&canonical_evidence, std::slice::from_ref(&canonical))
        .await?;
    fixture
        .store
        .append_finding_event(
            subject_ref.finding(),
            finding_event(
                subject_ref,
                ReviewEventOrdinal::one(),
                evidence[2].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_from_finding(&canonical)
                        .expect("open canonical finding is reference evidence"),
                },
            ),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE review_pass_finding_inventory_seal
         DISABLE TRIGGER review_pass_finding_inventory_seal_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM review_pass_finding_inventory_seal
          WHERE pass_id = $1",
    )
    .bind(canonical_pass.pass().into_uuid())
    .execute(&pool)
    .await?;
    assert_finding_reference_load_corruption(
        fixture.store.load_finding(subject_ref.finding()).await,
        "review_pass_produced_finding",
    );
    sqlx::query(
        "INSERT INTO review_pass_finding_inventory_seal
            (pass_id, finding_count)
         VALUES ($1, 1)",
    )
    .bind(canonical_pass.pass().into_uuid())
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE review_pass
         DROP CONSTRAINT review_pass_result_referenced_inventory_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_finding_event
         DROP CONSTRAINT review_finding_event_referenced_inventory_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass_produced_finding
         DISABLE TRIGGER review_pass_produced_finding_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM review_pass_produced_finding
          WHERE pass_id = $1
            AND finding_id = $2",
    )
    .bind(canonical_pass.pass().into_uuid())
    .bind(canonical_ref.finding().into_uuid())
    .execute(&pool)
    .await?;
    assert_finding_reference_load_corruption(
        fixture.store.load_finding(subject_ref.finding()).await,
        "review_pass_produced_finding",
    );
    Ok(())
}

/// duplicate/superseded references cannot close a cycle by
/// referencing a finding whose current status is already terminal.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_references_reject_cycles() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let second_producer =
        insert_fixture_pass(&fixture, 0x335, ReviewPassKind::ReadOnlyReview).await;
    let first_dedupe = insert_fixture_pass(&fixture, 0x336, ReviewPassKind::Dedupe).await;
    let second_dedupe = insert_fixture_pass(&fixture, 0x337, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, second_producer, first_dedupe, second_dedupe],
    )
    .await;
    let first = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x338)));
    let second = ReviewFindingRef::new(second_producer, ReviewFindingId::from_uuid(uuid(0x339)));
    let review_evidence = pass_with_produced_findings(vec![first], evidence[0].clone());
    let second_evidence = pass_with_produced_findings(vec![second], evidence[1].clone());
    let first_finding = finding(first, review_evidence.clone(), &fixture.target_snapshot);
    let second_finding = finding(second, second_evidence.clone(), &fixture.target_snapshot);
    fixture
        .store
        .insert_findings(&review_evidence, std::slice::from_ref(&first_finding))
        .await?;
    fixture
        .store
        .insert_findings(&second_evidence, std::slice::from_ref(&second_finding))
        .await?;
    fixture
        .store
        .append_finding_event(
            first.finding(),
            finding_event(
                first,
                ReviewEventOrdinal::one(),
                evidence[2].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_from_finding(&second_finding)
                        .expect("open finding is eligible reference evidence"),
                },
            ),
        )
        .await?
        .expect("first reference persists");

    let cycle = fixture
        .store
        .append_finding_event(
            second.finding(),
            finding_event(
                second,
                ReviewEventOrdinal::one(),
                evidence[3].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_from_finding(&first_finding)
                        .expect("open finding is eligible reference evidence"),
                },
            ),
        )
        .await
        .expect_err("terminal reference targets cannot close a cycle");
    let ReviewWorkflowStoreError::Database(cycle) = cycle else {
        panic!("cycle prevention must be a database rejection");
    };
    assert_sqlstate(&cycle, "23514");
    Ok(())
}

/// complete-target loading rejects a transitive cross-run cycle even
/// when corrupt SQL bypassed the admission trigger that prevents it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn loader_rejects_transitive_cross_run_reference_cycle() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let second_producer =
        insert_fixture_pass(&fixture, 0x5331, ReviewPassKind::ReadOnlyReview).await;
    let third_producer =
        insert_fixture_pass(&fixture, 0x5332, ReviewPassKind::ReadOnlyReview).await;
    let first_dedupe = insert_fixture_pass(&fixture, 0x5333, ReviewPassKind::Dedupe).await;
    let second_dedupe = insert_fixture_pass(&fixture, 0x5334, ReviewPassKind::Dedupe).await;
    let third_dedupe = insert_fixture_pass(&fixture, 0x5335, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[
            fixture.pass,
            second_producer,
            third_producer,
            first_dedupe,
            second_dedupe,
            third_dedupe,
        ],
    )
    .await;
    let first = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x5340)));
    let second = ReviewFindingRef::new(second_producer, ReviewFindingId::from_uuid(uuid(0x5341)));
    let third = ReviewFindingRef::new(third_producer, ReviewFindingId::from_uuid(uuid(0x5342)));
    let first_evidence = pass_with_produced_findings(vec![first], evidence[0].clone());
    let second_evidence = pass_with_produced_findings(vec![second], evidence[1].clone());
    let third_evidence = pass_with_produced_findings(vec![third], evidence[2].clone());
    let first_finding = finding(first, first_evidence.clone(), &fixture.target_snapshot);
    let second_finding = finding(second, second_evidence.clone(), &fixture.target_snapshot);
    let third_finding = finding(third, third_evidence.clone(), &fixture.target_snapshot);
    fixture
        .store
        .insert_findings(&first_evidence, std::slice::from_ref(&first_finding))
        .await?;
    fixture
        .store
        .insert_findings(&second_evidence, std::slice::from_ref(&second_finding))
        .await?;
    fixture
        .store
        .insert_findings(&third_evidence, std::slice::from_ref(&third_finding))
        .await?;
    fixture
        .store
        .append_finding_event(
            first.finding(),
            finding_event(
                first,
                ReviewEventOrdinal::one(),
                evidence[3].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_from_finding(&second_finding)
                        .expect("open second finding is reference evidence"),
                },
            ),
        )
        .await?;
    fixture
        .store
        .append_finding_event(
            second.finding(),
            finding_event(
                second,
                ReviewEventOrdinal::one(),
                evidence[4].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_from_finding(&third_finding)
                        .expect("open third finding is reference evidence"),
                },
            ),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE review_finding_event
         DISABLE TRIGGER review_finding_event_sequence_is_guarded,
         DISABLE TRIGGER review_finding_event_transition_head_is_guarded,
         DISABLE TRIGGER review_finding_event_transition_head_is_advanced",
    )
    .execute(&pool)
    .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = 1,
                result_event_kind = 'duplicate',
                result_referenced_finding_id = $5,
                result_referenced_finding_run_id = $6,
                result_referenced_finding_target_id = $7,
                result_referenced_finding_pass_id = $8,
                result_referenced_finding_status = 'open'
          WHERE pass_id = $1",
    )
    .bind(third_dedupe.pass().into_uuid())
    .bind(third.finding().into_uuid())
    .bind(third.run().run().into_uuid())
    .bind(third.pass().pass().into_uuid())
    .bind(first.finding().into_uuid())
    .bind(first.run().run().into_uuid())
    .bind(first.target().into_uuid())
    .bind(first.pass().pass().into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, referenced_finding_run_id,
             referenced_finding_target_id, referenced_finding_pass_id,
             referenced_finding_status, external_link_id,
             external_link_association_kind)
         VALUES (
             $1, 1, $2, $3, $4, $5, 'duplicate', NULL,
             $6, $7, $8, $9, 'open', NULL, NULL
         )",
    )
    .bind(third.finding().into_uuid())
    .bind(third.run().run().into_uuid())
    .bind(third.target().into_uuid())
    .bind(third_dedupe.pass().into_uuid())
    .bind(third_dedupe.run().run().into_uuid())
    .bind(first.finding().into_uuid())
    .bind(first.run().run().into_uuid())
    .bind(first.target().into_uuid())
    .bind(first.pass().pass().into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE review_finding_event_head
            SET event_ordinal = 1,
                status = $2,
                event_pass_kind = $3,
                external_link_id = NULL
          WHERE finding_id = $1",
    )
    .bind(third.finding().into_uuid())
    .bind("duplicate")
    .bind("dedupe")
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    sqlx::query(
        "ALTER TABLE review_finding_event
         ENABLE TRIGGER review_finding_event_sequence_is_guarded,
         ENABLE TRIGGER review_finding_event_transition_head_is_guarded,
         ENABLE TRIGGER review_finding_event_transition_head_is_advanced",
    )
    .execute(&pool)
    .await?;

    let error = fixture
        .store
        .load_finding(first.finding())
        .await
        .expect_err("transitive reference cycle must fail complete graph loading");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed finding reference-graph corruption");
    };
    assert_eq!(error.aggregate(), "review_finding");
    assert!(error.detail().contains("reference graph"));
    Ok(())
}

/// a referenced finding's missing canonical producer is corruption,
/// even when the aggregate finding's own producer remains intact.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_load_rejects_missing_referenced_producer() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let dedupe_pass = insert_fixture_pass(&fixture, 0x33a, ReviewPassKind::Dedupe).await;
    let evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, dedupe_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x33b)));
    let canonical_ref =
        ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x33c)));
    let review_evidence =
        pass_with_produced_findings(vec![finding_ref, canonical_ref], evidence[0].clone());
    let subject = finding(
        finding_ref,
        review_evidence.clone(),
        &fixture.target_snapshot,
    );
    let canonical = finding(
        canonical_ref,
        review_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_findings(&review_evidence, &[subject, canonical.clone()])
        .await?;
    fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            finding_event(
                finding_ref,
                ReviewEventOrdinal::one(),
                evidence[1].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_from_finding(&canonical)
                        .expect("open finding is eligible reference evidence"),
                },
            ),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE review_finding
         DROP CONSTRAINT review_finding_producing_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DROP CONSTRAINT review_pass_result_referenced_finding_fk,
         DROP CONSTRAINT review_pass_result_referenced_inventory_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass_produced_finding
         DROP CONSTRAINT review_pass_produced_finding_finding_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_finding_event
         DROP CONSTRAINT review_finding_event_referenced_finding_fk,
         DROP CONSTRAINT review_finding_event_referenced_inventory_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_finding
         DISABLE TRIGGER review_finding_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_finding
            SET producing_pass_id = $2
          WHERE finding_id = $1",
    )
    .bind(canonical_ref.finding().into_uuid())
    .bind(uuid(0x33d))
    .execute(&pool)
    .await?;

    assert_finding_reference_load_corruption(
        fixture.store.load_finding(finding_ref.finding()).await,
        "review_pass_produced_finding",
    );
    Ok(())
}

/// direct SQL cannot admit a judgment below the finding producer's
/// frozen minimum confidence threshold.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_enforces_judge_confidence_threshold() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x33e, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x33f)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    fixture
        .store
        .insert_finding(&finding_with_is_real_confidence(
            finding_ref,
            review_evidence,
            &fixture.target_snapshot,
            6_999,
        ))
        .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = 1,
                result_event_kind = 'accepted'
          WHERE pass_id = $1",
    )
    .bind(judge_pass.pass().into_uuid())
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.run().run().into_uuid())
    .bind(finding_ref.pass().pass().into_uuid())
    .execute(&mut *transaction)
    .await?;
    let event = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, referenced_finding_status,
             external_link_id, external_link_association_kind)
         VALUES ($1, 1, $2, $3, $4, $5, 'accepted',
                 NULL, NULL, NULL, NULL, NULL)",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.run().run().into_uuid())
    .bind(finding_ref.target().into_uuid())
    .bind(judge_pass.pass().into_uuid())
    .bind(judge_pass.run().run().into_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("below-threshold judgment cannot bypass the domain through SQL");
    assert_sqlstate(&event, "23514");
    transaction.rollback().await?;
    Ok(())
}

/// direct SQL cannot publish a finding below the producer's
/// frozen publication threshold, even with matching attachment evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_enforces_publication_confidence_threshold() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x34a, ReviewPassKind::Judge).await;
    let publish_pass = insert_fixture_pass(&fixture, 0x34b, ReviewPassKind::Publish).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, judge_pass, publish_pass],
    )
    .await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x34c)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    fixture
        .store
        .insert_finding(&finding_with_is_real_confidence(
            finding_ref,
            review_evidence,
            &fixture.target_snapshot,
            7_999,
        ))
        .await?;
    fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            finding_event(
                finding_ref,
                ReviewEventOrdinal::one(),
                evidence[1].clone(),
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await?;
    let link = ReviewExternalLinkId::from_uuid(uuid(0x34d));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                link,
                ReviewExternalLinkAssociation::Finding(finding_ref),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &fixture.target_snapshot,
            )
            .expect("reservation matches the target"),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE review_finding_event
         DISABLE TRIGGER USER",
    )
    .execute(&pool)
    .await?;
    let missing_association = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, referenced_finding_status,
             external_link_id, external_link_association_kind)
         VALUES ($1, 2, $2, $3, $4, $5, 'posted',
                 NULL, NULL, NULL, $6, NULL)",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.run().run().into_uuid())
    .bind(finding_ref.target().into_uuid())
    .bind(publish_pass.pass().into_uuid())
    .bind(publish_pass.run().run().into_uuid())
    .bind(link.into_uuid())
    .execute(&pool)
    .await
    .expect_err("linked finding events require their association discriminator");
    sqlx::query(
        "ALTER TABLE review_finding_event
         ENABLE TRIGGER USER",
    )
    .execute(&pool)
    .await?;
    assert_eq!(
        missing_association
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("review_finding_event_shape")
    );

    let missing_discriminator = sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'external_link_attachment',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = 2,
                result_external_link_id = $5,
                result_external_object_key = 'comment-34d'
          WHERE pass_id = $1",
    )
    .bind(publish_pass.pass().into_uuid())
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.run().run().into_uuid())
    .bind(finding_ref.pass().pass().into_uuid())
    .bind(link.into_uuid())
    .execute(&pool)
    .await
    .expect_err("posted attachment evidence requires its event discriminator");
    assert_sqlstate(&missing_discriminator, "23514");

    let mut transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'external_link_attachment',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = 2,
                result_event_kind = 'posted',
                result_external_link_id = $5,
                result_external_object_key = 'comment-34d'
          WHERE pass_id = $1",
    )
    .bind(publish_pass.pass().into_uuid())
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.run().run().into_uuid())
    .bind(finding_ref.pass().pass().into_uuid())
    .bind(link.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO review_external_link_attachment
            (external_link_id, target_id, pass_run_id, pass_id,
             provider_key, object_kind, external_object_key)
         VALUES ($1, $2, $3, $4, 'example-code-host',
                 'review_comment', 'comment-34d')",
    )
    .bind(link.into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(publish_pass.run().run().into_uuid())
    .bind(publish_pass.pass().into_uuid())
    .execute(&mut *transaction)
    .await?;
    let posted = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, referenced_finding_status,
             external_link_id, external_link_association_kind)
         VALUES ($1, 2, $2, $3, $4, $5, 'posted',
                 NULL, NULL, NULL, $6, 'finding')",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.run().run().into_uuid())
    .bind(finding_ref.target().into_uuid())
    .bind(publish_pass.pass().into_uuid())
    .bind(publish_pass.run().run().into_uuid())
    .bind(link.into_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("below-threshold publication cannot bypass the domain through SQL");
    assert_sqlstate(&posted, "23514");
    transaction.rollback().await?;
    Ok(())
}

/// severity-label uncertainty cannot suppress a finding
/// whose is-real confidence clears both frozen policy thresholds.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_thresholds_ignore_severity_label_confidence() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x34e, ReviewPassKind::Judge).await;
    let publish_pass = insert_fixture_pass(&fixture, 0x34f, ReviewPassKind::Publish).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, judge_pass, publish_pass],
    )
    .await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x350)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    let finding = finding_with_confidence_axes_and_side(
        finding_ref,
        review_evidence,
        &fixture.target_snapshot,
        FindingConfidenceAxes {
            is_real: 9_500,
            severity_label: 0,
        },
        Some(ReviewFindingDiffSide::Right),
    );
    fixture.store.insert_finding(&finding).await?;
    fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            finding_event(
                finding_ref,
                ReviewEventOrdinal::one(),
                evidence[1].clone(),
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await?;

    let link = ReviewExternalLinkId::from_uuid(uuid(0x351));
    let reservation = ReviewExternalLink::try_reserve(
        link,
        ReviewExternalLinkAssociation::Finding(finding_ref),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
        &fixture.target_snapshot,
    )
    .expect("reservation matches the target");
    fixture.store.reserve_external_link(reservation).await?;
    fixture
        .store
        .attach_external_link(
            link,
            posted_attachment(
                link,
                evidence[2].clone(),
                key("comment-351"),
                finding_ref,
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
            ),
        )
        .await?;

    let posted = fixture
        .store
        .load_finding(finding_ref.finding())
        .await?
        .expect("posted finding loads");
    assert_eq!(posted.status(), ReviewFindingStatus::Posted);
    Ok(())
}

/// event compatibility is checked against the canonical persisted
/// pass kind, not only the kind carried by the in-memory event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn canonical_pass_kind_rejects_misclassified_event() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let publish_pass = insert_fixture_pass(&fixture, 0x334, ReviewPassKind::Publish).await;
    let evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, publish_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x335)));
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            evidence[0].clone(),
            &fixture.target_snapshot,
        ))
        .await?;

    let error = fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            finding_event(
                finding_ref,
                ReviewEventOrdinal::one(),
                pass_evidence(
                    publish_pass,
                    ReviewPassKind::Judge,
                    evidence[1].clone().policy(),
                    evidence[1].state().clone(),
                ),
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await
        .expect_err("canonical publication pass cannot accept a finding");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("canonical pass-kind mismatch must fail closed as pass corruption");
    };
    assert_eq!(error.aggregate(), "review_pass");
    assert!(
        error
            .detail()
            .contains("differs from canonical execution facts")
    );
    Ok(())
}

/// the event row must exactly match the finding result committed by
/// its terminal pass, including ordinal and event type.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_event_requires_exact_pass_result() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x33e, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x33f)));
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            evidence[0].clone(),
            &fixture.target_snapshot,
        ))
        .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $1,
                result_finding_run_id = $2,
                result_finding_pass_id = $3,
                result_event_ordinal = 1,
                result_event_kind = 'accepted'
          WHERE pass_id = $4",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.run().run().into_uuid())
    .bind(finding_ref.pass().pass().into_uuid())
    .bind(judge_pass.pass().into_uuid())
    .execute(&mut *transaction)
    .await?;

    let mismatched = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, referenced_finding_status,
             external_link_id, external_link_association_kind)
         VALUES (
             $1, 1, $2, $3, $4, $5, 'rejected', 'not accepted',
             NULL, NULL, NULL, NULL
         )",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.run().run().into_uuid())
    .bind(finding_ref.target().into_uuid())
    .bind(judge_pass.pass().into_uuid())
    .bind(judge_pass.run().run().into_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("event kind must equal the terminal pass result");
    assert_sqlstate(&mismatched, "23514");
    transaction.rollback().await?;
    Ok(())
}

/// an effect result cannot be changed after its first atomic binding.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn bound_pass_result_is_immutable() -> Result<(), Box<dyn Error>> {
    const JUDGE_PASS_IDENTITY: u128 = 0x3342;
    const FINDING_IDENTITY: u128 = 0x3343;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass =
        insert_fixture_pass(&fixture, JUDGE_PASS_IDENTITY, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(
        fixture.pass,
        ReviewFindingId::from_uuid(uuid(FINDING_IDENTITY)),
    );
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            evidence[0].clone(),
            &fixture.target_snapshot,
        ))
        .await?;
    fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            finding_event(
                finding_ref,
                ReviewEventOrdinal::one(),
                evidence[1].clone(),
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await?;

    let mutation = sqlx::query(
        "UPDATE review_pass
            SET result_event_kind = 'rejected',
                result_reason = 'changed after binding'
          WHERE pass_id = $1",
    )
    .bind(judge_pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("bound result payload is immutable");
    assert_sqlstate(&mutation, "23514");
    Ok(())
}

/// persistence rejects review policy versions that the domain cannot
/// reconstitute.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_rejects_unsupported_policy_version() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let unsupported_policy = sqlx::query(
        "INSERT INTO review_run
            (run_id, target_id, workflow_kind, policy_version,
             minimum_judge_confidence, minimum_publication_confidence,
             state_kind, state_pass_id)
         VALUES ($1, $2, 'read_only_review', 2, 7500, 8500, 'queued', NULL)",
    )
    .bind(uuid(0x3340))
    .bind(fixture.target.into_uuid())
    .execute(&pool)
    .await
    .expect_err("unsupported policy versions must fail before domain loading");
    assert_sqlstate(&unsupported_policy, "23514");
    Ok(())
}

/// an attachment carried through another same-target reservation
/// fails before persistence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn external_attachment_rejects_foreign_owner() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let first = ReviewExternalLinkId::from_uuid(uuid(0x336));
    let second = ReviewExternalLinkId::from_uuid(uuid(0x337));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                first,
                ReviewExternalLinkAssociation::Target(fixture.target),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &fixture.target_snapshot,
            )
            .expect("reservation matches the target"),
        )
        .await?;

    let error = fixture
        .store
        .attach_external_link(
            first,
            attachment(
                second,
                succeeded_pass(fixture.pass, ReviewPassKind::Publish),
                key("comment-337"),
            ),
        )
        .await
        .expect_err("attachment owner must equal the loaded external link");
    assert!(matches!(
        error,
        ReviewWorkflowStoreError::InvalidTransition(
            ReviewWorkflowTransitionError::ExternalLink(error)
        ) if error.failure()
            == ReviewExternalLinkTransitionFailure::ForeignAttachmentLink
    ));
    Ok(())
}

/// appending an observation through another same-target external link
/// fails before persistence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn external_observation_rejects_foreign_owner() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let first_publish_pass = insert_fixture_pass(&fixture, 0x338, ReviewPassKind::Publish).await;
    let second_publish_pass = insert_fixture_pass(&fixture, 0x33a, ReviewPassKind::Publish).await;
    let import_pass =
        insert_fixture_pass(&fixture, 0x339, ReviewPassKind::ImportExternalContext).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[first_publish_pass, second_publish_pass, import_pass],
    )
    .await;
    let first_publish_evidence = evidence[0].clone();
    let second_publish_evidence = evidence[1].clone();
    let import_evidence = evidence[2].clone();
    let first = ReviewExternalLinkId::from_uuid(uuid(0x335));
    let second = ReviewExternalLinkId::from_uuid(uuid(0x336));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                first,
                ReviewExternalLinkAssociation::Target(fixture.target),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &fixture.target_snapshot,
            )
            .expect("reservation matches the target"),
        )
        .await?;
    fixture
        .store
        .attach_external_link(
            first,
            attachment(first, first_publish_evidence, key("comment-335")),
        )
        .await?;
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                second,
                ReviewExternalLinkAssociation::Target(fixture.target),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &fixture.target_snapshot,
            )
            .expect("reservation matches the target"),
        )
        .await?;
    fixture
        .store
        .attach_external_link(
            second,
            attachment(second, second_publish_evidence, key("comment-336")),
        )
        .await?;

    let error = fixture
        .store
        .append_external_observation(
            first,
            observation(
                second,
                ReviewEventOrdinal::one(),
                import_evidence,
                ReviewExternalObjectState::Current,
            ),
        )
        .await
        .expect_err("observation owner must equal the loaded external link");
    assert!(matches!(
        error,
        ReviewWorkflowStoreError::InvalidTransition(
            ReviewWorkflowTransitionError::ExternalLink(error)
        ) if error.failure()
            == ReviewExternalLinkTransitionFailure::ForeignObservationLink
    ));

    Ok(())
}

/// file-relative findings admit no diff side, while a diff-relative
/// location requires a canonical target base.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_diff_side_requires_target_base() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let target = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(0x340)),
        key("example-code-host"),
        key("example/base-free-repository"),
        ReviewTargetSubject::Commit,
        key("0123456789abcdef"),
        None,
        None,
    )
    .expect("a commit target need not name a comparison revision");
    fixture.store.insert_target(&target).await?;
    let (pass, _) = insert_isolated_pass_for_target(
        &pool,
        &fixture.store,
        target.id(),
        0x342,
        ReviewPassKind::ReadOnlyReview,
    )
    .await;
    let run = pass.run();
    let review_evidence = succeed_fixture_passes(&pool, &fixture.store, &[pass]).await[0].clone();
    let file_relative = ReviewFindingRef::new(pass, ReviewFindingId::from_uuid(uuid(0x343)));
    let file_relative = finding_with_side(file_relative, review_evidence, &target, None);
    fixture.store.insert_finding(&file_relative).await?;
    assert_eq!(
        fixture
            .store
            .load_finding(file_relative.proposal().reference().finding())
            .await?,
        Some(file_relative)
    );

    let diff_relative = sqlx::query(
        "INSERT INTO review_finding
             (finding_id, run_id, target_id, producing_pass_id, file_path,
             line_start, line_end, diff_side, title, body, severity,
             is_real_confidence, severity_label_confidence, category,
             recommended_fix)
         VALUES (
             $1, $2, $3, $4, 'src/lib.rs',
             1, 1, 'right', 'Finding', 'Body', 'high',
             9000, 8500, 'correctness', NULL
         )",
    )
    .bind(uuid(0x344))
    .bind(run.run().into_uuid())
    .bind(target.id().into_uuid())
    .bind(pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("diff-relative finding requires target comparison evidence");
    assert_sqlstate(&diff_relative, "23514");

    Ok(())
}

/// the store refuses to insert a run projection after transition.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn run_insert_requires_queued_state() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let running = fixture
        .store
        .load_run(fixture.run.run())
        .await?
        .expect("fixture run exists")
        .transition(
            ReviewRunState::Running {
                active_pass: fixture.pass,
            },
            Some(pass_evidence(
                fixture.pass,
                ReviewPassKind::ReadOnlyReview,
                ReviewPolicy::version_one(),
                ReviewPassState::Running {
                    turn: TurnId::from_uuid(uuid(0x203)),
                },
            )),
        )
        .expect("queued run activates with matching pass evidence");
    assert!(matches!(
        fixture.store.insert_run(&running).await,
        Err(ReviewWorkflowStoreError::InvalidInsertion(
            ReviewWorkflowInsertionError::RunNotQueued { .. }
        ))
    ));
    Ok(())
}

/// the store refuses to insert a pass projection after transition.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_insert_requires_queued_state() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let turn = TurnId::from_uuid(uuid(0x203));
    let running = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await?
        .expect("fixture pass exists")
        .transition(
            ReviewPassState::Running { turn },
            Some(ReviewPassTurnEvidence::new(
                turn,
                SessionId::from_uuid(uuid(0x201)),
                AcceptedInputId::from_uuid(uuid(0x202)),
                ReviewPassTurnOutcome::Active,
                None,
            )),
        )
        .expect("queued pass activates with matching turn evidence");
    assert!(matches!(
        fixture.store.insert_pass(&running).await,
        Err(ReviewWorkflowStoreError::InvalidInsertion(
            ReviewWorkflowInsertionError::PassNotQueued { .. }
        ))
    ));
    Ok(())
}

/// the store refuses to insert a finding carrying event history.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_insert_requires_open_state() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x3060, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x306)));
    let accepted = finding(finding_ref, evidence[0].clone(), &fixture.target_snapshot)
        .apply(finding_event(
            finding_ref,
            ReviewEventOrdinal::one(),
            evidence[1].clone(),
            ReviewFindingEventKind::Accepted,
        ))
        .expect("open finding accepts judgment");
    assert!(matches!(
        fixture.store.insert_finding(&accepted).await,
        Err(ReviewWorkflowStoreError::InvalidInsertion(
            ReviewWorkflowInsertionError::FindingNotOpen { .. }
        ))
    ));
    Ok(())
}

/// reservation insertion refuses post-effect attachment evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn reservation_insert_requires_pending_state() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let link = ReviewExternalLinkId::from_uuid(uuid(0x307));
    let attached = ReviewExternalLink::try_reserve(
        link,
        ReviewExternalLinkAssociation::Target(fixture.target),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
        &fixture.target_snapshot,
    )
    .expect("reservation matches the target")
    .attach(attachment(
        link,
        succeeded_pass(fixture.pass, ReviewPassKind::Publish),
        key("comment-85"),
    ))
    .expect("same-target pass may attach");
    assert!(matches!(
        fixture.store.reserve_external_link(attached).await,
        Err(ReviewWorkflowStoreError::InvalidInsertion(
            ReviewWorkflowInsertionError::ExternalLinkNotPending
        ))
    ));
    let claimed_link = ReviewExternalLinkId::from_uuid(uuid(0x308));
    let claimed_pass = pass_evidence(
        fixture.pass,
        ReviewPassKind::Publish,
        ReviewPolicy::version_one(),
        ReviewPassState::Blocked {
            turn: TurnId::from_uuid(uuid(0x203)),
            result: Some(ReviewPassResult::ExternalLinkPublicationBlocked(
                ReviewExternalLinkPublicationBlockedResult::new(
                    claimed_link,
                    text("provider acknowledgement requires reconciliation"),
                ),
            )),
        },
    );
    let claimed = ReviewExternalLink::try_reserve(
        claimed_link,
        ReviewExternalLinkAssociation::Target(fixture.target),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
        &fixture.target_snapshot,
    )
    .expect("reservation matches the target")
    .block_publication(claimed_pass.clone(), run_evidence_for_pass(claimed_pass))
    .expect("blocked publication claim belongs to the reservation");
    assert!(matches!(
        fixture.store.reserve_external_link(claimed).await,
        Err(ReviewWorkflowStoreError::InvalidInsertion(
            ReviewWorkflowInsertionError::ExternalLinkNotPending
        ))
    ));
    Ok(())
}

/// a blocked publication pass is consumed by the exact pending
/// reservation and its nonempty reconciliation reason.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn blocked_publication_binds_pending_reservation() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let publish_pass = insert_fixture_pass(&fixture, 0x30a, ReviewPassKind::Publish).await;
    let (_, turn) = start_review_pass(&fixture.store, publish_pass).await;
    reconcile_review_turn(&pool, turn).await;
    let link = ReviewExternalLinkId::from_uuid(uuid(0x30b));
    let reservation = ReviewExternalLink::try_reserve(
        link,
        ReviewExternalLinkAssociation::Target(fixture.target),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
        &fixture.target_snapshot,
    )
    .expect("reservation matches the target");
    fixture
        .store
        .reserve_external_link(reservation.clone())
        .await?;
    let reason = text("provider acknowledgement requires reconciliation");
    let pass = pass_evidence(
        publish_pass,
        ReviewPassKind::Publish,
        ReviewPolicy::version_one(),
        ReviewPassState::Blocked {
            turn,
            result: Some(ReviewPassResult::ExternalLinkPublicationBlocked(
                ReviewExternalLinkPublicationBlockedResult::new(link, reason.clone()),
            )),
        },
    );
    let run = run_evidence_for_pass(pass.clone());
    let expected = reservation
        .clone()
        .block_publication(pass.clone(), run)
        .expect("blocked pass belongs to the pending reservation");
    assert_eq!(
        fixture
            .store
            .block_external_link_publication(link, pass.clone(), run)
            .await?,
        Some(expected.clone())
    );
    let loaded = fixture
        .store
        .load_pass(publish_pass.pass())
        .await?
        .expect("blocked publication pass remains loadable");
    assert_eq!(loaded.state(), pass.state());
    assert_eq!(
        fixture.store.load_external_link(link).await?,
        Some(expected),
        "publication-block claims survive aggregate reload"
    );
    Ok(())
}

/// a finding-associated reservation authenticates the finding's exact
/// canonical producing pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn reservation_rejects_forged_finding_producer() -> Result<(), Box<dyn Error>> {
    const FINDING_IDENTITY: u128 = 0x751;
    const FORGED_PASS_IDENTITY: u128 = 0x752;
    const LINK_IDENTITY: u128 = 0x753;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass]).await[0].clone();
    let finding_ref = ReviewFindingRef::new(
        fixture.pass,
        ReviewFindingId::from_uuid(uuid(FINDING_IDENTITY)),
    );
    fixture
        .store
        .insert_finding(&finding(finding_ref, evidence, &fixture.target_snapshot))
        .await?;
    let forged_finding = ReviewFindingRef::new(
        ReviewPassRef::new(
            fixture.run,
            ReviewPassId::from_uuid(uuid(FORGED_PASS_IDENTITY)),
        ),
        finding_ref.finding(),
    );
    let reservation = ReviewExternalLink::try_reserve(
        ReviewExternalLinkId::from_uuid(uuid(LINK_IDENTITY)),
        ReviewExternalLinkAssociation::Finding(forged_finding),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
        &fixture.target_snapshot,
    )
    .expect("forged producer remains target-valid before persistence authentication");

    let error = fixture
        .store
        .reserve_external_link(reservation)
        .await
        .expect_err("reservation must authenticate the complete finding reference");
    let ReviewWorkflowStoreError::Database(error) = error else {
        panic!("canonical finding authentication must be a database rejection");
    };
    assert_sqlstate(&error, "23503");
    Ok(())
}

/// raw reservation inserts cannot diverge from the canonical target
/// provider.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn reservation_requires_canonical_target_provider() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let forged = sqlx::query(
        "INSERT INTO review_external_link
            (external_link_id, target_id, association_kind, run_id,
             finding_id, finding_producing_pass_id, provider_key,
             object_kind)
         VALUES ($1, $2, 'target', NULL, NULL, NULL,
                 'another-code-host', 'review_comment')",
    )
    .bind(uuid(0x754))
    .bind(fixture.target.into_uuid())
    .execute(&pool)
    .await
    .expect_err("reservation provider must equal the canonical target provider");
    assert_sqlstate(&forged, "23514");
    Ok(())
}

/// the identity registry is derivable attachment evidence, not an
/// independently writable claim.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn external_identity_requires_establishing_attachment() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let unbacked = sqlx::query(
        "INSERT INTO review_external_object_identity
            (provider_key, object_kind, external_object_key,
             logical_target_id)
         VALUES ('example-code-host', 'review_comment',
                 'unbacked-comment', $1)",
    )
    .bind(fixture.target.into_uuid())
    .execute(&pool)
    .await
    .expect_err("identity claims require an establishing attachment");
    assert_sqlstate(&unbacked, "23514");
    Ok(())
}

/// the canonical pass/finding and external-claim lookup
/// paths remain indexed by their leading filter columns.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_lookup_indexes_are_pinned() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let external_link_index: String = sqlx::query_scalar(
        "SELECT indexdef
           FROM pg_indexes
          WHERE schemaname = 'public'
            AND indexname = 'review_pass_external_link_result_index'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(external_link_index.contains("(result_external_link_id, pass_id)"));
    assert!(external_link_index.contains("result_external_link_id IS NOT NULL"));

    let producing_pass_index: String = sqlx::query_scalar(
        "SELECT indexdef
           FROM pg_indexes
          WHERE schemaname = 'public'
            AND indexname = 'review_finding_producing_pass_index'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(producing_pass_index.contains("(producing_pass_id, target_id, run_id, finding_id)"));

    let attachment_identity_index: String = sqlx::query_scalar(
        "SELECT indexdef
           FROM pg_indexes
          WHERE schemaname = 'public'
            AND indexname = 'review_external_link_attachment_identity_index'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(attachment_identity_index.contains("(identity_digest, target_id)"));

    let blocked_link_index: String = sqlx::query_scalar(
        "SELECT indexdef
           FROM pg_indexes
          WHERE schemaname = 'public'
            AND indexname = 'review_finding_event_blocked_link_index'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(blocked_link_index.contains("(external_link_id)"));
    assert!(blocked_link_index.contains("event_kind = 'blocked_with_reason'"));
    Ok(())
}

/// raw run rows must begin queued.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_requires_new_run_to_be_queued() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let direct_cancelled_run = sqlx::query(
        "INSERT INTO review_run
            (run_id, target_id, workflow_kind, policy_version,
             minimum_judge_confidence, minimum_publication_confidence,
             state_kind, state_pass_id)
         VALUES (
             $1, $2, 'read_only_review', 1, 7000, 8000,
             'cancelled', NULL
         )",
    )
    .bind(uuid(0x607))
    .bind(fixture.target.into_uuid())
    .execute(&pool)
    .await
    .expect_err("raw review runs must begin queued");
    assert_sqlstate(&direct_cancelled_run, "23514");
    Ok(())
}

/// raw pass rows must begin queued.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_requires_new_pass_to_be_queued() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let direct_failed_pass = sqlx::query(
        "INSERT INTO review_pass
            (pass_id, run_id, target_id, pass_kind, session_id,
             accepted_input_id, origin_turn_id, state_kind, turn_id,
             output_frontier_id)
         VALUES (
             $1, $2, $3, 'read_only_review', $4,
             $5, $6, 'failed', $6, NULL
         )",
    )
    .bind(uuid(0x608))
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(uuid(0x201))
    .bind(uuid(0x202))
    .bind(uuid(0x203))
    .execute(&pool)
    .await
    .expect_err("raw review passes must begin queued");
    assert_sqlstate(&direct_failed_pass, "23514");
    Ok(())
}

/// S29: change-request targets require a frozen comparison
/// revision.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s29_schema_requires_change_request_base() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let missing_change_request = sqlx::query(
        "INSERT INTO review_target
            (target_id, provider_key, repository_key, subject_kind,
             change_request_number, head_revision, base_revision,
             stack_parent_target_id)
         VALUES (
             $1, 'example-code-host', 'example/repository',
             'change_request', 42, '0123456789abcdef', NULL, NULL
         )",
    )
    .bind(uuid(0x601))
    .execute(&pool)
    .await
    .expect_err("change-request targets require their frozen comparison revision");
    assert_sqlstate(&missing_change_request, "23514");
    Ok(())
}

/// policy version one has one canonical threshold tuple.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_requires_canonical_version_one_policy() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let noncanonical_policy = sqlx::query(
        "INSERT INTO review_run
            (run_id, target_id, workflow_kind, policy_version,
             minimum_judge_confidence, minimum_publication_confidence,
             state_kind, state_pass_id)
         VALUES ($1, $2, 'read_only_review', 1, 7001, 8000, 'queued', NULL)",
    )
    .bind(uuid(0x602))
    .bind(fixture.target.into_uuid())
    .execute(&pool)
    .await
    .expect_err("version one requires the exact 7000/8000 threshold tuple");
    assert_sqlstate(&noncanonical_policy, "23514");
    Ok(())
}

/// finding line ranges are absent or complete.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_rejects_half_populated_line_range() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let half_populated_range = sqlx::query(
        "INSERT INTO review_finding
             (finding_id, run_id, target_id, producing_pass_id, file_path,
             line_start, line_end, diff_side, title, body, severity,
             is_real_confidence, severity_label_confidence, category,
             recommended_fix)
         VALUES (
             $1, $2, $3, $4, 'src/lib.rs',
             1, NULL, 'right', 'Finding', 'Body', 'high',
             9000, 8500, 'correctness', NULL
         )",
    )
    .bind(uuid(0x603))
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("finding line ranges are absent or fully populated");
    assert_sqlstate(&half_populated_range, "23514");
    Ok(())
}

/// rejected finding events require their exact reason.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_requires_rejection_reason() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x609, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let reason_finding =
        ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x604)));
    fixture
        .store
        .insert_finding(&finding(
            reason_finding,
            evidence[0].clone(),
            &fixture.target_snapshot,
        ))
        .await
        .expect("reason-shape fixture persists");
    let missing_reason = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, external_link_id,
             external_link_association_kind)
         VALUES ($1, 1, $2, $3, $4, $2, 'rejected', NULL, NULL, NULL, NULL)",
    )
    .bind(reason_finding.finding().into_uuid())
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(judge_pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("rejected events require a reason");
    assert_sqlstate(&missing_reason, "23514");
    Ok(())
}

/// a posted event requires attached external review content.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_authenticates_posted_external_review_content() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x607, ReviewPassKind::Judge).await;
    let publish_pass = insert_fixture_pass(&fixture, 0x608, ReviewPassKind::Publish).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, judge_pass, publish_pass],
    )
    .await;
    let posted_finding =
        ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x605)));
    fixture
        .store
        .insert_finding(&finding(
            posted_finding,
            evidence[0].clone(),
            &fixture.target_snapshot,
        ))
        .await
        .expect("posted-shape fixture persists");
    fixture
        .store
        .append_finding_event(
            posted_finding.finding(),
            finding_event(
                posted_finding,
                ReviewEventOrdinal::one(),
                evidence[1].clone(),
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await
        .expect("accepted event persists");
    let pending_link = ReviewExternalLinkId::from_uuid(uuid(0x606));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                pending_link,
                ReviewExternalLinkAssociation::Finding(posted_finding),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &fixture.target_snapshot,
            )
            .expect("reservation matches the target"),
        )
        .await
        .expect("pending reservation persists");
    let posted_without_attachment = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, external_link_id,
             external_link_association_kind)
         VALUES ($1, 2, $2, $3, $4, $5, 'posted', NULL, NULL, $6, 'finding')",
    )
    .bind(posted_finding.finding().into_uuid())
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(publish_pass.pass().into_uuid())
    .bind(publish_pass.run().run().into_uuid())
    .bind(pending_link.into_uuid())
    .execute(&pool)
    .await
    .expect_err("posted status requires attachment evidence from the event pass");
    assert_sqlstate(&posted_without_attachment, "23514");

    let commit_link = ReviewExternalLinkId::from_uuid(uuid(0x609));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                commit_link,
                ReviewExternalLinkAssociation::Finding(posted_finding),
                key("example-code-host"),
                ReviewExternalObjectKind::Commit,
                &fixture.target_snapshot,
            )
            .expect("reservation matches the target"),
        )
        .await
        .expect("repository correlation reservation persists");
    fixture
        .store
        .attach_external_link(
            commit_link,
            attachment(commit_link, evidence[2].clone(), key("external-commit-609")),
        )
        .await
        .expect("repository correlation attachment persists");
    let posted_through_commit = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, external_link_id,
             external_link_association_kind)
         VALUES ($1, 2, $2, $3, $4, $5, 'posted', NULL, NULL, $6, 'finding')",
    )
    .bind(posted_finding.finding().into_uuid())
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(publish_pass.pass().into_uuid())
    .bind(publish_pass.run().run().into_uuid())
    .bind(commit_link.into_uuid())
    .execute(&pool)
    .await
    .expect_err("an attached commit does not prove that finding content was posted");
    assert_sqlstate(&posted_through_commit, "23514");
    Ok(())
}

/// cancelling a running run cannot erase its active pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_running_run_cancellation_retains_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    start_review_pass(&fixture.store, fixture.pass).await;

    let erased_run_pass = sqlx::query(
        "UPDATE review_run
            SET state_kind = 'cancelled', state_pass_id = NULL
          WHERE run_id = $1",
    )
    .bind(fixture.run.run().into_uuid())
    .execute(&pool)
    .await
    .expect_err("running cancellation retains the active pass");
    assert_sqlstate(&erased_run_pass, "23514");
    Ok(())
}

/// loading a queued run retains its already-recorded pass, so the
/// store rejects a passless cancellation before issuing an update.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn queued_run_cannot_discard_recorded_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;

    let error = fixture
        .store
        .transition_run(
            fixture.run.run(),
            ReviewRunState::Cancelled { last_pass: None },
        )
        .await
        .expect_err("queued run loading must retain its canonical pass");
    assert!(matches!(
        error,
        ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Run(_))
    ));

    Ok(())
}

/// cancelling a running pass cannot erase its active turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_running_pass_cancellation_retains_turn() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    start_review_pass(&fixture.store, fixture.pass).await;
    let erased_pass_turn = sqlx::query(
        "UPDATE review_pass
            SET state_kind = 'cancelled', turn_id = NULL
          WHERE pass_id = $1",
    )
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("running cancellation retains the active turn");
    assert_sqlstate(&erased_pass_turn, "23514");

    Ok(())
}

/// a multi-row external-link load observes one database snapshot
/// while a concurrent attachment and observation commit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn external_link_load_is_one_repeatable_snapshot() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let publish_pass = insert_fixture_pass(&fixture, 0x60a, ReviewPassKind::Publish).await;
    let import_pass =
        insert_fixture_pass(&fixture, 0x60b, ReviewPassKind::ImportExternalContext).await;
    succeed_fixture_passes(&pool, &fixture.store, &[publish_pass, import_pass]).await;
    let link = ReviewExternalLinkId::from_uuid(uuid(0x607));
    let reservation = ReviewExternalLink::try_reserve(
        link,
        ReviewExternalLinkAssociation::Target(fixture.target),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
        &fixture.target_snapshot,
    )
    .expect("reservation matches the target");
    fixture
        .store
        .reserve_external_link(reservation.clone())
        .await
        .expect("pending reservation persists");

    let mut writer = pool.begin().await?;
    sqlx::query(
        "LOCK TABLE review_external_link_observation
         IN ACCESS EXCLUSIVE MODE",
    )
    .execute(&mut *writer)
    .await?;

    let loading_store = fixture.store.clone();
    let loading = tokio::spawn(async move { loading_store.load_external_link(link).await });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "external-link load reaches the held observation relation"
    );

    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'external_link_attachment',
                result_external_link_id = $2,
                result_external_object_key = 'comment-87'
          WHERE pass_id = $1",
    )
    .bind(publish_pass.pass().into_uuid())
    .bind(link.into_uuid())
    .execute(&mut *writer)
    .await?;
    sqlx::query(
        "INSERT INTO review_external_link_attachment
            (external_link_id, target_id, pass_run_id, pass_id,
             provider_key, object_kind, external_object_key)
         VALUES ($1, $2, $3, $4, 'example-code-host', 'review_comment', 'comment-87')",
    )
    .bind(link.into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(publish_pass.run().run().into_uuid())
    .bind(publish_pass.pass().into_uuid())
    .execute(&mut *writer)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'external_link_observation',
                result_external_link_id = $2,
                result_event_ordinal = 1,
                result_observation_state = 'current'
          WHERE pass_id = $1",
    )
    .bind(import_pass.pass().into_uuid())
    .bind(link.into_uuid())
    .execute(&mut *writer)
    .await?;
    sqlx::query(
        "INSERT INTO review_external_link_observation
            (external_link_id, observation_ordinal, target_id,
             pass_run_id, pass_id, object_state)
         VALUES ($1, 1, $2, $3, $4, 'current')",
    )
    .bind(link.into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(import_pass.run().run().into_uuid())
    .bind(import_pass.pass().into_uuid())
    .execute(&mut *writer)
    .await?;
    writer.commit().await?;

    let during_commit = loading.await??.expect("reservation remains visible");
    assert_eq!(
        during_commit, reservation,
        "one repeatable snapshot cannot tear attachment from observation"
    );

    let after_commit = fixture
        .store
        .load_external_link(link)
        .await?
        .expect("committed external link loads");
    assert!(after_commit.attachment().is_some());
    assert_eq!(after_commit.observations().len(), 1);

    Ok(())
}

/// finding-event serialization remains compatible with the key-share
/// lock PostgreSQL takes while checking a foreign finding reference.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_event_serialization_is_fk_compatible() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x30a, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x309)));
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            evidence[0].clone(),
            &fixture.target_snapshot,
        ))
        .await
        .expect("open finding persists");

    let mut foreign_key_reader = pool.begin().await?;
    sqlx::query(
        "SELECT finding_id
           FROM review_finding
          WHERE finding_id = $1
          FOR KEY SHARE",
    )
    .bind(finding_ref.finding().into_uuid())
    .fetch_one(&mut *foreign_key_reader)
    .await?;

    let mut appender = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '1s'")
        .execute(&mut *appender)
        .await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $1,
                result_finding_run_id = $2,
                result_finding_pass_id = $3,
                result_event_ordinal = 1,
                result_event_kind = 'accepted'
          WHERE pass_id = $4",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.pass().run().run().into_uuid())
    .bind(finding_ref.pass().pass().into_uuid())
    .bind(judge_pass.pass().into_uuid())
    .execute(&mut *appender)
    .await?;
    sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, external_link_id,
             external_link_association_kind)
         VALUES ($1, 1, $2, $3, $4, $5, 'accepted', NULL, NULL, NULL, NULL)",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(judge_pass.pass().into_uuid())
    .bind(judge_pass.run().run().into_uuid())
    .execute(&mut *appender)
    .await
    .expect("event root lock must remain compatible with foreign-key readers");
    appender.commit().await?;
    foreign_key_reader.rollback().await?;

    Ok(())
}

/// observation ordinal serialization remains compatible with the
/// key-share lock used by external-link foreign-key checks.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn external_observation_serialization_is_fk_compatible() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let publish_pass = insert_fixture_pass(&fixture, 0x30c, ReviewPassKind::Publish).await;
    let import_pass =
        insert_fixture_pass(&fixture, 0x30d, ReviewPassKind::ImportExternalContext).await;
    let evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[publish_pass, import_pass]).await;
    let link = ReviewExternalLinkId::from_uuid(uuid(0x30e));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                link,
                ReviewExternalLinkAssociation::Target(fixture.target),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &fixture.target_snapshot,
            )
            .expect("reservation matches the target"),
        )
        .await?;
    fixture
        .store
        .attach_external_link(
            link,
            attachment(link, evidence[0].clone(), key("comment-30e")),
        )
        .await?;

    let mut foreign_key_reader = pool.begin().await?;
    sqlx::query(
        "SELECT external_link_id
           FROM review_external_link
          WHERE external_link_id = $1
          FOR KEY SHARE",
    )
    .bind(link.into_uuid())
    .fetch_one(&mut *foreign_key_reader)
    .await?;

    let mut appender = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '1s'")
        .execute(&mut *appender)
        .await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'external_link_observation',
                result_external_link_id = $2,
                result_event_ordinal = 1,
                result_observation_state = 'current'
          WHERE pass_id = $1",
    )
    .bind(import_pass.pass().into_uuid())
    .bind(link.into_uuid())
    .execute(&mut *appender)
    .await?;
    sqlx::query(
        "INSERT INTO review_external_link_observation
            (external_link_id, observation_ordinal, target_id,
             pass_run_id, pass_id, object_state)
         VALUES ($1, 1, $2, $3, $4, 'current')",
    )
    .bind(link.into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(import_pass.run().run().into_uuid())
    .bind(import_pass.pass().into_uuid())
    .execute(&mut *appender)
    .await
    .expect("observation root lock must remain compatible with foreign-key readers");
    appender.commit().await?;
    foreign_key_reader.rollback().await?;

    Ok(())
}

/// PostgreSQL rejects an event history that does not begin at one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn gapped_finding_history_is_rejected() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x30b, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let second_finding_ref =
        ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x306)));
    fixture
        .store
        .insert_finding(&finding(
            second_finding_ref,
            evidence[0].clone(),
            &fixture.target_snapshot,
        ))
        .await
        .expect("second open finding persists");
    let gap = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, external_link_id,
             external_link_association_kind)
         VALUES ($1, 2, $2, $3, $4, $5, 'accepted', NULL, NULL, NULL, NULL)",
    )
    .bind(second_finding_ref.finding().into_uuid())
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(judge_pass.pass().into_uuid())
    .bind(judge_pass.run().run().into_uuid())
    .execute(&pool)
    .await
    .expect_err("event history must start at ordinal one");
    assert_sqlstate(&gap, "23514");

    Ok(())
}

/// PostgreSQL rejects a producing pass from another target/run edge.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cross_wired_pass_ancestry_is_rejected() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let other_target = ReviewTargetId::from_uuid(uuid(0x401));
    fixture
        .store
        .insert_target(
            &ReviewTarget::try_new(
                other_target,
                key("example-code-host"),
                key("example/other-repository"),
                ReviewTargetSubject::Commit,
                key("abcdef0123456789"),
                Some(key("9876543210fedcba")),
                None,
            )
            .expect("other target topology is valid"),
        )
        .await
        .expect("other target persists");
    let other_run = ReviewRunRef::new(other_target, ReviewRunId::from_uuid(uuid(0x402)));
    fixture
        .store
        .insert_run(&ReviewRun::new(
            other_run,
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        ))
        .await
        .expect("other run persists");

    let cross_wired = sqlx::query(
        "INSERT INTO review_finding
             (finding_id, run_id, target_id, producing_pass_id, file_path,
             line_start, line_end, diff_side, title, body, severity,
             is_real_confidence, severity_label_confidence, category,
             recommended_fix)
         VALUES (
             $1, $2, $3, $4, 'src/lib.rs',
             1, 1, 'right', 'Finding', 'Body', 'high',
             9000, 8500, 'correctness', NULL
         )",
    )
    .bind(uuid(0x403))
    .bind(other_run.run().into_uuid())
    .bind(other_target.into_uuid())
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("producing pass must be canonical for the finding run and target");
    assert_sqlstate(&cross_wired, "23514");

    Ok(())
}

/// a missing immutable finding producer is corruption, not absence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_load_rejects_missing_producing_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let review_evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass]).await[0].clone();
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x740)));
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            review_evidence,
            &fixture.target_snapshot,
        ))
        .await?;

    sqlx::query(
        "ALTER TABLE review_finding
         DROP CONSTRAINT review_finding_producing_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass_produced_finding
         DROP CONSTRAINT review_pass_produced_finding_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass_finding_inventory_seal
         DROP CONSTRAINT review_pass_finding_inventory_seal_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_run
         DROP CONSTRAINT review_run_state_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_reject_delete",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DELETE FROM review_pass WHERE pass_id = $1")
        .bind(fixture.pass.pass().into_uuid())
        .execute(&pool)
        .await?;

    let error = fixture
        .store
        .load_finding(finding_ref.finding())
        .await
        .expect_err("missing producer must fail closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed review-finding corruption");
    };
    assert_eq!(error.aggregate(), "review_finding");
    assert!(error.detail().contains("producing pass row is missing"));
    Ok(())
}

/// a finding-associated external link cannot load when its finding's
/// canonical producing pass is missing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn external_link_load_rejects_missing_finding_producer() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let review_evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass]).await[0].clone();
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x743)));
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            review_evidence,
            &fixture.target_snapshot,
        ))
        .await?;
    let link = ReviewExternalLinkId::from_uuid(uuid(0x744));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                link,
                ReviewExternalLinkAssociation::Finding(finding_ref),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &fixture.target_snapshot,
            )
            .expect("reservation matches the target"),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE review_finding
         DROP CONSTRAINT review_finding_producing_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass_produced_finding
         DROP CONSTRAINT review_pass_produced_finding_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass_finding_inventory_seal
         DROP CONSTRAINT review_pass_finding_inventory_seal_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_run
         DROP CONSTRAINT review_run_state_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_reject_delete",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DELETE FROM review_pass WHERE pass_id = $1")
        .bind(fixture.pass.pass().into_uuid())
        .execute(&pool)
        .await?;

    let error = fixture
        .store
        .load_external_link(link)
        .await
        .expect_err("missing finding producer must fail external-link loading closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed review-external-link corruption");
    };
    assert_eq!(error.aggregate(), "review_external_link");
    assert!(
        error
            .detail()
            .contains("finding producing pass row is missing")
    );
    Ok(())
}

/// a missing attachment-pass run is corruption, not absence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn external_link_load_rejects_missing_attachment_run() -> Result<(), Box<dyn Error>> {
    const PUBLISH_PASS_IDENTITY: u128 = 0x745;
    const LINK_IDENTITY: u128 = 0x746;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let publish_pass =
        insert_fixture_pass(&fixture, PUBLISH_PASS_IDENTITY, ReviewPassKind::Publish).await;
    let publish_evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[publish_pass]).await[0].clone();
    let link = ReviewExternalLinkId::from_uuid(uuid(LINK_IDENTITY));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                link,
                ReviewExternalLinkAssociation::Target(fixture.target),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &fixture.target_snapshot,
            )
            .expect("reservation matches the target"),
        )
        .await?;
    fixture
        .store
        .attach_external_link(link, attachment(link, publish_evidence, key("comment-746")))
        .await?;
    let unrelated_target = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(0x74a)),
        key("example-code-host"),
        key("example/repository"),
        ReviewTargetSubject::Commit,
        key("unrelated-head"),
        None,
        None,
    )
    .expect("unrelated target is structurally valid");
    fixture.store.insert_target(&unrelated_target).await?;
    sqlx::query(
        "ALTER TABLE review_external_object_identity
         DISABLE TRIGGER review_external_object_identity_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_external_object_identity
            SET logical_target_id = $1
          WHERE provider_key = 'example-code-host'
            AND object_kind = 'review_comment'
            AND external_object_key = 'comment-746'",
    )
    .bind(unrelated_target.id().into_uuid())
    .execute(&pool)
    .await?;
    let identity_error = fixture
        .store
        .load_external_link(link)
        .await
        .expect_err("attachment loading must authenticate its object registry target");
    let ReviewWorkflowStoreError::Corruption(identity_error) = identity_error else {
        panic!("expected typed review-external-link corruption");
    };
    assert_eq!(
        identity_error.aggregate(),
        "review_external_link_attachment"
    );
    assert!(identity_error.detail().contains("unrelated logical target"));
    sqlx::query(
        "UPDATE review_external_object_identity
            SET logical_target_id = $1
          WHERE provider_key = 'example-code-host'
            AND object_kind = 'review_comment'
            AND external_object_key = 'comment-746'",
    )
    .bind(fixture.target.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_external_object_identity
         DISABLE TRIGGER review_external_object_identity_insert_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_external_object_identity
         DISABLE TRIGGER review_external_identity_attachment_is_required",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO review_external_object_identity
            (provider_key, object_kind, external_object_key, logical_target_id)
         VALUES (
            'example-code-host', 'review_comment', 'comment-746', $1
         )",
    )
    .bind(unrelated_target.id().into_uuid())
    .execute(&pool)
    .await?;
    let duplicate_error = fixture
        .store
        .load_external_link(link)
        .await
        .expect_err("duplicate external-object identities must fail loading closed");
    let ReviewWorkflowStoreError::Corruption(duplicate_error) = duplicate_error else {
        panic!("expected typed external-object identity multiplicity corruption");
    };
    assert_eq!(
        duplicate_error.aggregate(),
        "review_external_link_attachment"
    );
    assert!(duplicate_error.detail().contains("exactly one"));
    sqlx::query(
        "DELETE FROM review_external_object_identity
          WHERE provider_key = 'example-code-host'
            AND object_kind = 'review_comment'
            AND external_object_key = 'comment-746'
            AND logical_target_id = $1",
    )
    .bind(unrelated_target.id().into_uuid())
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE review_pass
         DROP CONSTRAINT review_pass_run_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_run
         DISABLE TRIGGER review_run_reject_delete",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DELETE FROM review_run WHERE run_id = $1")
        .bind(publish_pass.run().run().into_uuid())
        .execute(&pool)
        .await?;

    let error = fixture
        .store
        .load_external_link(link)
        .await
        .expect_err("missing attachment run must fail external-link loading closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed review-external-link corruption");
    };
    assert_eq!(error.aggregate(), "review_external_link_attachment");
    assert!(error.detail().contains("attaching run row is missing"));
    Ok(())
}

/// a missing observation-pass run is corruption, not a shortened
/// observation history.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn external_link_load_rejects_missing_observation_run() -> Result<(), Box<dyn Error>> {
    const PUBLISH_PASS_IDENTITY: u128 = 0x747;
    const IMPORT_PASS_IDENTITY: u128 = 0x748;
    const LINK_IDENTITY: u128 = 0x749;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let publish_pass =
        insert_fixture_pass(&fixture, PUBLISH_PASS_IDENTITY, ReviewPassKind::Publish).await;
    let import_pass = insert_fixture_pass(
        &fixture,
        IMPORT_PASS_IDENTITY,
        ReviewPassKind::ImportExternalContext,
    )
    .await;
    let evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[publish_pass, import_pass]).await;
    let link = ReviewExternalLinkId::from_uuid(uuid(LINK_IDENTITY));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                link,
                ReviewExternalLinkAssociation::Target(fixture.target),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &fixture.target_snapshot,
            )
            .expect("reservation matches the target"),
        )
        .await?;
    fixture
        .store
        .attach_external_link(
            link,
            attachment(link, evidence[0].clone(), key("comment-749")),
        )
        .await?;
    fixture
        .store
        .append_external_observation(
            link,
            observation(
                link,
                ReviewEventOrdinal::one(),
                evidence[1].clone(),
                ReviewExternalObjectState::Current,
            ),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE review_pass
         DROP CONSTRAINT review_pass_run_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_run
         DISABLE TRIGGER review_run_reject_delete",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DELETE FROM review_run WHERE run_id = $1")
        .bind(import_pass.run().run().into_uuid())
        .execute(&pool)
        .await?;

    let error = fixture
        .store
        .load_external_link(link)
        .await
        .expect_err("missing observation run must fail external-link loading closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed review-external-link corruption");
    };
    assert_eq!(error.aggregate(), "review_external_link_observation");
    assert!(error.detail().contains("observing run row is missing"));
    Ok(())
}

/// a missing finding-event pass is corruption, not a silently
/// shortened history.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_load_rejects_missing_event_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x741, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x742)));
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            evidence[0].clone(),
            &fixture.target_snapshot,
        ))
        .await?;
    fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            finding_event(
                finding_ref,
                ReviewEventOrdinal::one(),
                evidence[1].clone(),
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE review_finding_event
         DROP CONSTRAINT review_finding_event_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_run
         DROP CONSTRAINT review_run_state_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_reject_delete",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DELETE FROM review_pass WHERE pass_id = $1")
        .bind(judge_pass.pass().into_uuid())
        .execute(&pool)
        .await?;

    let error = fixture
        .store
        .load_finding(finding_ref.finding())
        .await
        .expect_err("missing event pass must fail closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed review-event corruption");
    };
    assert_eq!(error.aggregate(), "review_finding_event");
    assert!(error.detail().contains("event pass row is missing"));
    Ok(())
}

/// one provider/kind/object identity has at most one attachment per
/// frozen target, cannot move to an unrelated logical target, and may follow
/// one change request across refreshed snapshots.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn external_object_attachment_is_unique() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let first_publish_pass = insert_fixture_pass(&fixture, 0x503, ReviewPassKind::Publish).await;
    let second_publish_pass = insert_fixture_pass(&fixture, 0x504, ReviewPassKind::Publish).await;
    let publish_evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[first_publish_pass, second_publish_pass],
    )
    .await;
    let first_publish_evidence = publish_evidence[0].clone();
    let second_publish_evidence = publish_evidence[1].clone();
    let first_link = ReviewExternalLinkId::from_uuid(uuid(0x501));
    let second_link = ReviewExternalLinkId::from_uuid(uuid(0x502));
    let first_reservation = ReviewExternalLink::try_reserve(
        first_link,
        ReviewExternalLinkAssociation::Target(fixture.target),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
        &fixture.target_snapshot,
    )
    .expect("reservation matches the target");
    let second_reservation = ReviewExternalLink::try_reserve(
        second_link,
        ReviewExternalLinkAssociation::Target(fixture.target),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
        &fixture.target_snapshot,
    )
    .expect("reservation matches the target");
    fixture
        .store
        .reserve_external_link(first_reservation)
        .await
        .expect("first reservation persists");
    fixture
        .store
        .reserve_external_link(second_reservation)
        .await
        .expect("second reservation persists");
    fixture
        .store
        .attach_external_link(
            first_link,
            attachment(first_link, first_publish_evidence, key("comment-84")),
        )
        .await
        .expect("first attachment persists");

    let duplicate = fixture
        .store
        .attach_external_link(
            second_link,
            attachment(second_link, second_publish_evidence, key("comment-84")),
        )
        .await
        .expect_err("one external object identity cannot attach twice");
    let ReviewWorkflowStoreError::Database(duplicate) = duplicate else {
        panic!("external-object uniqueness must be a database rejection")
    };
    assert_sqlstate(&duplicate, "23505");

    let refreshed_target_id = ReviewTargetId::from_uuid(uuid(0x813));
    let refreshed_target = ReviewTarget::try_new(
        refreshed_target_id,
        key("example-code-host"),
        key("example/repository"),
        ReviewTargetSubject::ChangeRequest(
            ReviewChangeRequestNumber::try_new(42).expect("positive change request"),
        ),
        key("1122334455667788"),
        Some(key("0123456789abcdef")),
        None,
    )
    .expect("refreshed target snapshot is valid");
    fixture.store.insert_target(&refreshed_target).await?;
    let refreshed_session = SessionId::from_uuid(uuid(0x810));
    let refreshed_input = AcceptedInputId::from_uuid(uuid(0x811));
    let refreshed_turn = TurnId::from_uuid(uuid(0x812));
    insert_active_turn_with_offset(
        &pool,
        refreshed_session,
        refreshed_input,
        refreshed_turn,
        0x7_000,
    )
    .await;
    let refreshed_publish = insert_pass_for_target(
        &fixture.store,
        refreshed_target_id,
        0x814,
        ReviewPassKind::Publish,
        refreshed_session,
        refreshed_input,
    )
    .await;
    start_review_pass(&fixture.store, refreshed_publish).await;
    let refreshed_frontier = complete_review_turn(&pool, refreshed_turn).await;
    let refreshed_evidence = conclude_review_pass(
        &fixture.store,
        refreshed_publish,
        ReviewPassState::Succeeded {
            turn: refreshed_turn,
            output_frontier: refreshed_frontier,
            result: None,
        },
    )
    .await;
    let refreshed_link = ReviewExternalLinkId::from_uuid(uuid(0x815));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                refreshed_link,
                ReviewExternalLinkAssociation::Target(refreshed_target_id),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &refreshed_target,
            )
            .expect("reservation matches the refreshed target"),
        )
        .await?;
    let unrelated = fixture
        .store
        .attach_external_link(
            refreshed_link,
            attachment(
                refreshed_link,
                refreshed_evidence.clone(),
                key("comment-84"),
            ),
        )
        .await
        .expect_err("a commit object cannot move to a change request");
    let ReviewWorkflowStoreError::Database(unrelated) = unrelated else {
        panic!("logical-target reassociation must be a database rejection")
    };
    assert_sqlstate(&unrelated, "23514");

    let first_change_request_link = ReviewExternalLinkId::from_uuid(uuid(0x816));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                first_change_request_link,
                ReviewExternalLinkAssociation::Target(refreshed_target_id),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &refreshed_target,
            )
            .expect("first change-request reservation matches its target"),
        )
        .await?;
    fixture
        .store
        .attach_external_link(
            first_change_request_link,
            attachment(
                first_change_request_link,
                refreshed_evidence,
                key("refreshed-comment"),
            ),
        )
        .await?
        .expect("first change-request snapshot establishes object ownership");

    let later_target_id = ReviewTargetId::from_uuid(uuid(0x817));
    let later_target = ReviewTarget::try_new(
        later_target_id,
        key("example-code-host"),
        key("example/repository"),
        ReviewTargetSubject::ChangeRequest(
            ReviewChangeRequestNumber::try_new(42).expect("positive change request"),
        ),
        key("2233445566778899"),
        Some(key("1122334455667788")),
        None,
    )
    .expect("later snapshot of the same change request is valid");
    fixture.store.insert_target(&later_target).await?;
    let later_session = SessionId::from_uuid(uuid(0x818));
    let later_input = AcceptedInputId::from_uuid(uuid(0x819));
    let later_turn = TurnId::from_uuid(uuid(0x81a));
    insert_active_turn_with_offset(&pool, later_session, later_input, later_turn, 0x7_500).await;
    let later_publish = insert_pass_for_target(
        &fixture.store,
        later_target_id,
        0x81b,
        ReviewPassKind::Publish,
        later_session,
        later_input,
    )
    .await;
    start_review_pass(&fixture.store, later_publish).await;
    let later_frontier = complete_review_turn(&pool, later_turn).await;
    let later_evidence = conclude_review_pass(
        &fixture.store,
        later_publish,
        ReviewPassState::Succeeded {
            turn: later_turn,
            output_frontier: later_frontier,
            result: None,
        },
    )
    .await;
    let later_link = ReviewExternalLinkId::from_uuid(uuid(0x81c));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                later_link,
                ReviewExternalLinkAssociation::Target(later_target_id),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &later_target,
            )
            .expect("later change-request reservation matches its target"),
        )
        .await?;
    fixture
        .store
        .attach_external_link(
            later_link,
            attachment(later_link, later_evidence, key("refreshed-comment")),
        )
        .await?
        .expect("the same logical change request may retain the external object");

    Ok(())
}

/// concurrent first attachments serialize on canonical object identity,
/// so unrelated targets cannot both establish ownership.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn concurrent_external_object_attachment_has_one_logical_target() -> Result<(), Box<dyn Error>>
{
    const FIRST_PASS_IDENTITY: u128 = 0x820;
    const SECOND_TARGET_IDENTITY: u128 = 0x821;
    const SECOND_PASS_IDENTITY: u128 = 0x822;
    const SECOND_SESSION_IDENTITY: u128 = 0x823;
    const SECOND_INPUT_IDENTITY: u128 = 0x824;
    const SECOND_TURN_IDENTITY: u128 = SECOND_INPUT_IDENTITY + 1;
    const FIRST_LINK_IDENTITY: u128 = 0x826;
    const SECOND_LINK_IDENTITY: u128 = 0x827;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let first_pass =
        insert_fixture_pass(&fixture, FIRST_PASS_IDENTITY, ReviewPassKind::Publish).await;

    let second_target_id = ReviewTargetId::from_uuid(uuid(SECOND_TARGET_IDENTITY));
    let second_target = ReviewTarget::try_new(
        second_target_id,
        key("example-code-host"),
        key("example/repository"),
        ReviewTargetSubject::Commit,
        key("unrelated-head"),
        Some(key("unrelated-base")),
        None,
    )
    .expect("second target is a distinct commit snapshot");
    fixture.store.insert_target(&second_target).await?;
    let second_session = SessionId::from_uuid(uuid(SECOND_SESSION_IDENTITY));
    let second_input = AcceptedInputId::from_uuid(uuid(SECOND_INPUT_IDENTITY));
    let second_turn = TurnId::from_uuid(uuid(SECOND_TURN_IDENTITY));
    insert_active_turn_with_offset(&pool, second_session, second_input, second_turn, 0x8_000).await;
    let second_pass = insert_pass_for_target(
        &fixture.store,
        second_target_id,
        SECOND_PASS_IDENTITY,
        ReviewPassKind::Publish,
        second_session,
        second_input,
    )
    .await;

    let (_, first_turn) = start_review_pass(&fixture.store, first_pass).await;
    let (_, second_turn) = start_review_pass(&fixture.store, second_pass).await;
    let first_frontier = complete_review_turn(&pool, first_turn).await;
    let second_frontier = complete_review_turn(&pool, second_turn).await;
    let first_evidence = conclude_review_pass(
        &fixture.store,
        first_pass,
        ReviewPassState::Succeeded {
            turn: first_turn,
            output_frontier: first_frontier,
            result: None,
        },
    )
    .await;
    let second_evidence = conclude_review_pass(
        &fixture.store,
        second_pass,
        ReviewPassState::Succeeded {
            turn: second_turn,
            output_frontier: second_frontier,
            result: None,
        },
    )
    .await;

    let first_link = ReviewExternalLinkId::from_uuid(uuid(FIRST_LINK_IDENTITY));
    let second_link = ReviewExternalLinkId::from_uuid(uuid(SECOND_LINK_IDENTITY));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                first_link,
                ReviewExternalLinkAssociation::Target(fixture.target),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &fixture.target_snapshot,
            )
            .expect("first reservation matches its target"),
        )
        .await?;
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                second_link,
                ReviewExternalLinkAssociation::Target(second_target_id),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &second_target,
            )
            .expect("second reservation matches its target"),
        )
        .await?;

    let first_store = fixture.store.clone();
    let second_store = fixture.store.clone();
    let (first, second) = tokio::join!(
        first_store.attach_external_link(
            first_link,
            attachment(first_link, first_evidence, key("shared-object")),
        ),
        second_store.attach_external_link(
            second_link,
            attachment(second_link, second_evidence, key("shared-object")),
        ),
    );
    assert_concurrent_attachment_outcomes(first, second);
    Ok(())
}

/// target loading reconstructs the complete stack ancestry, and the
/// schema rejects a logical change request repeated anywhere in that chain.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn target_stack_ancestry_is_complete_and_nonrepeating() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = ReviewWorkflowStore::new(pool.clone());
    let provider = key("example-code-host");
    let repository = key("example/repository");
    let root = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(0x770)),
        provider.clone(),
        repository.clone(),
        ReviewTargetSubject::ChangeRequest(
            ReviewChangeRequestNumber::try_new(41).expect("positive change request"),
        ),
        key("root-head"),
        Some(key("root-base")),
        None,
    )
    .expect("root target topology is valid");
    let middle = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(0x771)),
        provider.clone(),
        repository.clone(),
        ReviewTargetSubject::ChangeRequest(
            ReviewChangeRequestNumber::try_new(42).expect("positive change request"),
        ),
        key("middle-head"),
        Some(key("root-head")),
        Some(&root),
    )
    .expect("middle target topology is valid");
    let leaf = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(0x772)),
        provider,
        repository,
        ReviewTargetSubject::ChangeRequest(
            ReviewChangeRequestNumber::try_new(43).expect("positive change request"),
        ),
        key("leaf-head"),
        Some(key("middle-head")),
        Some(&middle),
    )
    .expect("leaf target topology is valid");
    store.insert_target(&root).await?;
    store.insert_target(&middle).await?;
    store.insert_target(&leaf).await?;
    assert_eq!(store.load_target(leaf.id()).await?, Some(leaf.clone()));

    let repeated = sqlx::query(
        "INSERT INTO review_target
            (target_id, provider_key, repository_key, subject_kind,
             change_request_number, head_revision, base_revision,
             stack_parent_target_id)
         VALUES (
             $1, 'example-code-host', 'example/repository',
             'change_request', 41, 'repeat-head', 'leaf-head', $2
         )",
    )
    .bind(uuid(0x773))
    .bind(leaf.id().into_uuid())
    .execute(&pool)
    .await
    .expect_err("one logical change request cannot repeat in a stack chain");
    assert_sqlstate(&repeated, "23514");
    Ok(())
}

/// stack parents are confined to the target's provider and
/// repository.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stack_parent_requires_same_repository() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let foreign_repository_parent = sqlx::query(
        "INSERT INTO review_target
            (target_id, provider_key, repository_key, subject_kind,
             change_request_number, head_revision, base_revision,
             stack_parent_target_id)
         VALUES (
             $1, 'example-code-host', 'example/other-repository',
             'commit', NULL, '1122334455667788',
             '0123456789abcdef', $2
         )",
    )
    .bind(uuid(0x701))
    .bind(fixture.target.into_uuid())
    .execute(&pool)
    .await
    .expect_err("stack parent must be in the target repository");
    assert_sqlstate(&foreign_repository_parent, "23514");
    Ok(())
}

/// a stack edge joins the child base to the canonical parent head.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stack_parent_requires_canonical_revision() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let child = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(0x801)),
        key("example-code-host"),
        key("example/repository"),
        ReviewTargetSubject::Commit,
        key("child-head"),
        Some(key("0123456789abcdef")),
        Some(&fixture.target_snapshot),
    )
    .expect("child base equals its canonical parent head");
    fixture.store.insert_target(&child).await?;
    assert_eq!(
        fixture.store.load_target(child.id()).await?,
        Some(child.clone())
    );

    let base_less = sqlx::query(
        "INSERT INTO review_target
            (target_id, provider_key, repository_key, subject_kind,
             change_request_number, head_revision, base_revision,
             stack_parent_target_id)
         VALUES (
             $1, 'example-code-host', 'example/repository',
             'commit', NULL, 'base-less-child', NULL, $2
         )",
    )
    .bind(uuid(0x802))
    .bind(fixture.target.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a parented target must freeze an exact base revision");
    assert_sqlstate(&base_less, "23514");

    let disconnected = sqlx::query(
        "INSERT INTO review_target
            (target_id, provider_key, repository_key, subject_kind,
             change_request_number, head_revision, base_revision,
             stack_parent_target_id)
         VALUES (
             $1, 'example-code-host', 'example/repository',
             'commit', NULL, 'disconnected-child', 'unrelated-base', $2
         )",
    )
    .bind(uuid(0x803))
    .bind(fixture.target.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a child base must equal its canonical parent head");
    assert_sqlstate(&disconnected, "23514");
    Ok(())
}

/// a pass kind is the exact one-to-one projection of its run
/// workflow.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_kind_requires_matching_run_workflow() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let run = ReviewRunRef::new(fixture.target, ReviewRunId::from_uuid(uuid(0x702)));
    fixture
        .store
        .insert_run(&ReviewRun::new(
            run,
            ReviewWorkflowKind::JudgeFindings,
            ReviewPolicy::version_one(),
        ))
        .await?;
    let mismatched = sqlx::query(
        "INSERT INTO review_pass
            (pass_id, run_id, target_id, pass_kind, session_id,
             accepted_input_id, origin_turn_id, state_kind, turn_id,
             output_frontier_id)
         VALUES (
             $1, $2, $3, 'read_only_review', $4, $5, $6,
             'queued', NULL, NULL
         )",
    )
    .bind(uuid(0x703))
    .bind(run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(uuid(0x201))
    .bind(uuid(0x202))
    .bind(uuid(0x203))
    .execute(&pool)
    .await
    .expect_err("pass kind must match the canonical run workflow");
    assert_sqlstate(&mismatched, "23514");
    Ok(())
}

/// one run owns at most one pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn run_rejects_second_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let second = sqlx::query(
        "INSERT INTO review_pass
            (pass_id, run_id, target_id, pass_kind, session_id,
             accepted_input_id, origin_turn_id, state_kind, turn_id,
             output_frontier_id)
         VALUES (
             $1, $2, $3, 'read_only_review', $4, $5, $6,
             'queued', NULL, NULL
         )",
    )
    .bind(uuid(0x704))
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(uuid(0x201))
    .bind(uuid(0x202))
    .bind(uuid(0x203))
    .execute(&pool)
    .await
    .expect_err("one run cannot own a second pass");
    assert_sqlstate(&second, "23505");
    Ok(())
}

/// a pass-only state change cannot commit without its run
/// projection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_only_projection_is_rejected() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let turn = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await?
        .expect("fixture pass exists")
        .origin_turn();
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET state_kind = 'running',
                turn_id = $2
          WHERE pass_id = $1",
    )
    .bind(fixture.pass.pass().into_uuid())
    .bind(turn.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("pass-only activation cannot commit");
    assert_sqlstate(&error, "23514");
    Ok(())
}

/// pre-start cancellation updates the run and pass atomically.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn queued_run_and_pass_cancel_together() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let (run, pass) = fixture
        .store
        .transition_run_and_pass(
            fixture.run.run(),
            fixture.pass.pass(),
            ReviewRunState::Cancelled {
                last_pass: Some(fixture.pass),
            },
            ReviewPassState::Cancelled { turn: None },
        )
        .await?
        .expect("queued run and pass exist");
    assert_eq!(
        run.state(),
        ReviewRunState::Cancelled {
            last_pass: Some(fixture.pass),
        }
    );
    assert_eq!(pass.state(), &ReviewPassState::Cancelled { turn: None });
    Ok(())
}

/// a running pass may load while its canonical turn has reached a
/// terminal outcome not yet projected into the pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn running_pass_admits_monotonic_terminal_turn_lag() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let turn = TurnId::from_uuid(uuid(0x203));
    start_review_pass(&fixture.store, fixture.pass).await;
    complete_review_turn(&pool, turn).await;
    let loaded = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await?
        .expect("running pass still loads after its turn concludes");
    assert_eq!(loaded.state(), &ReviewPassState::Running { turn });
    Ok(())
}

/// finding insertion authenticates a succeeded read-only-review
/// producer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_rejects_queued_producer() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let unauthorized = sqlx::query(
        "INSERT INTO review_finding
             (finding_id, run_id, target_id, producing_pass_id, file_path,
             line_start, line_end, diff_side, title, body, severity,
             is_real_confidence, severity_label_confidence, category,
             recommended_fix)
         VALUES (
             $1, $2, $3, $4, 'src/lib.rs',
             NULL, NULL, NULL, 'Finding', 'Body', 'high',
             9000, 8500, 'correctness', NULL
         )",
    )
    .bind(uuid(0x705))
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("queued pass cannot produce a finding");
    assert_sqlstate(&unauthorized, "23514");
    Ok(())
}

/// a finding event cannot claim a failed disposition pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_event_rejects_failed_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let other_session = SessionId::from_uuid(uuid(0x721));
    let other_input = AcceptedInputId::from_uuid(uuid(0x722));
    let other_turn = TurnId::from_uuid(uuid(0x723));
    insert_active_turn_with_offset(&pool, other_session, other_input, other_turn, 0x3_000).await;
    let judge_pass = insert_pass_for_target(
        &fixture.store,
        fixture.target,
        0x706,
        ReviewPassKind::Judge,
        other_session,
        other_input,
    )
    .await;
    let turn = TurnId::from_uuid(uuid(0x203));
    let (running_review, _) = start_review_pass(&fixture.store, fixture.pass).await;
    start_review_pass(&fixture.store, judge_pass).await;
    let output_frontier = complete_review_turn(&pool, turn).await;
    fail_review_turn(&pool, other_turn).await;
    let review_evidence =
        propose_read_only_success(&fixture.store, running_review, output_frontier).await;
    conclude_review_pass(
        &fixture.store,
        judge_pass,
        ReviewPassState::Failed { turn: other_turn },
    )
    .await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x707)));
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            review_evidence,
            &fixture.target_snapshot,
        ))
        .await?;
    let failed_event = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, external_link_id,
             external_link_association_kind)
         VALUES ($1, 1, $2, $3, $4, $5, 'accepted', NULL, NULL, NULL, NULL)",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(judge_pass.pass().into_uuid())
    .bind(judge_pass.run().run().into_uuid())
    .execute(&pool)
    .await
    .expect_err("failed pass cannot author a completed event");
    assert_sqlstate(&failed_event, "23514");
    Ok(())
}

/// attachment insertion rejects an otherwise canonical pass of the
/// wrong kind.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn attachment_rejects_read_only_review_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let review_evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass]).await;
    let no_findings = Vec::<ReviewFinding>::new();
    fixture
        .store
        .insert_findings(&review_evidence[0], &no_findings)
        .await?;
    let link = ReviewExternalLinkId::from_uuid(uuid(0x708));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                link,
                ReviewExternalLinkAssociation::Target(fixture.target),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &fixture.target_snapshot,
            )
            .expect("reservation matches the target"),
        )
        .await?;
    let unauthorized = sqlx::query(
        "INSERT INTO review_external_link_attachment
            (external_link_id, target_id, pass_run_id, pass_id,
             provider_key, object_kind, external_object_key)
         VALUES ($1, $2, $3, $4, 'example-code-host', 'review_comment', 'comment-708')",
    )
    .bind(link.into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("read-only review pass cannot produce an attachment");
    assert_sqlstate(&unauthorized, "23514");
    Ok(())
}

/// terminal pass effects require their exact finding-event,
/// attachment, or observation child row in the same transaction.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_results_require_exact_child_rows() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let publish_pass = insert_fixture_pass(&fixture, 0x7a0, ReviewPassKind::Publish).await;
    let import_pass =
        insert_fixture_pass(&fixture, 0x7a1, ReviewPassKind::ImportExternalContext).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x7a3, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, publish_pass, import_pass, judge_pass],
    )
    .await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x7a4)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            review_evidence,
            &fixture.target_snapshot,
        ))
        .await?;
    let link = ReviewExternalLinkId::from_uuid(uuid(0x7a2));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                link,
                ReviewExternalLinkAssociation::Target(fixture.target),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &fixture.target_snapshot,
            )
            .expect("reservation matches the target"),
        )
        .await?;

    let mut attachment_only = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'external_link_attachment',
                result_external_link_id = $2,
                result_external_object_key = 'comment-7a2'
          WHERE pass_id = $1",
    )
    .bind(publish_pass.pass().into_uuid())
    .bind(link.into_uuid())
    .execute(&mut *attachment_only)
    .await?;
    let missing_attachment = attachment_only
        .commit()
        .await
        .expect_err("attachment result cannot commit without its child row");
    assert_sqlstate(&missing_attachment, "23514");

    fixture
        .store
        .attach_external_link(
            link,
            attachment(link, evidence[1].clone(), key("comment-7a2")),
        )
        .await?;
    let mut observation_only = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'external_link_observation',
                result_external_link_id = $2,
                result_event_ordinal = 1,
                result_observation_state = 'current'
          WHERE pass_id = $1",
    )
    .bind(import_pass.pass().into_uuid())
    .bind(link.into_uuid())
    .execute(&mut *observation_only)
    .await?;
    let missing_observation = observation_only
        .commit()
        .await
        .expect_err("observation result cannot commit without its child row");
    assert_sqlstate(&missing_observation, "23514");

    let mut finding_event_only = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = 1,
                result_event_kind = 'accepted'
          WHERE pass_id = $1",
    )
    .bind(judge_pass.pass().into_uuid())
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.run().run().into_uuid())
    .bind(finding_ref.pass().pass().into_uuid())
    .execute(&mut *finding_event_only)
    .await?;
    let missing_finding_event = finding_event_only
        .commit()
        .await
        .expect_err("finding-event result cannot commit without its child row");
    assert_sqlstate(&missing_finding_event, "23514");
    Ok(())
}

/// observation insertion authenticates a succeeded import pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn observation_rejects_queued_import_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let publish_pass = insert_fixture_pass(&fixture, 0x709, ReviewPassKind::Publish).await;
    let import_pass =
        insert_fixture_pass(&fixture, 0x70a, ReviewPassKind::ImportExternalContext).await;
    let publish_evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[publish_pass]).await[0].clone();
    let link = ReviewExternalLinkId::from_uuid(uuid(0x70b));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                link,
                ReviewExternalLinkAssociation::Target(fixture.target),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &fixture.target_snapshot,
            )
            .expect("reservation matches the target"),
        )
        .await?;
    fixture
        .store
        .attach_external_link(link, attachment(link, publish_evidence, key("comment-70b")))
        .await?;
    let unauthorized = sqlx::query(
        "INSERT INTO review_external_link_observation
            (external_link_id, observation_ordinal, target_id,
             pass_run_id, pass_id, object_state)
         VALUES ($1, 1, $2, $3, $4, 'current')",
    )
    .bind(link.into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(import_pass.run().run().into_uuid())
    .bind(import_pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("queued import pass cannot author an observation");
    assert_sqlstate(&unauthorized, "23514");
    Ok(())
}

/// a linked publication block and a non-posting attachment
/// serialize on their shared reservation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn linked_block_serializes_with_non_posting_attachment() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x7b0, ReviewPassKind::Judge).await;
    let attaching_pass =
        insert_fixture_pass(&fixture, 0x7b1, ReviewPassKind::ImportExternalContext).await;
    let blocked_pass = insert_fixture_pass(&fixture, 0x7b2, ReviewPassKind::Publish).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, judge_pass, attaching_pass],
    )
    .await;
    let (_, blocked_turn) = start_review_pass(&fixture.store, blocked_pass).await;
    reconcile_review_turn(&pool, blocked_turn).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x7b3)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            review_evidence,
            &fixture.target_snapshot,
        ))
        .await?;
    fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            finding_event(
                finding_ref,
                ReviewEventOrdinal::one(),
                evidence[1].clone(),
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await?;
    let link = ReviewExternalLinkId::from_uuid(uuid(0x7b4));
    let reservation = ReviewExternalLink::try_reserve(
        link,
        ReviewExternalLinkAssociation::Finding(finding_ref),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
        &fixture.target_snapshot,
    )
    .expect("reservation matches the finding target");
    fixture
        .store
        .reserve_external_link(reservation.clone())
        .await?;
    let pending = ReviewFindingPendingExternalLinkRef::try_new(finding_ref, &reservation)
        .expect("unattached finding reservation is pending");
    let blocked_ordinal = ReviewEventOrdinal::try_new(2).expect("positive ordinal");
    let blocked_reason = text("publication acknowledgement is unresolved");
    let blocked_evidence = pass_evidence(
        blocked_pass,
        ReviewPassKind::Publish,
        ReviewPolicy::version_one(),
        ReviewPassState::Blocked {
            turn: blocked_turn,
            result: Some(ReviewPassResult::FindingEvent(
                ReviewFindingEventResult::new(
                    finding_ref,
                    blocked_ordinal,
                    ReviewFindingEventResultKind::BlockedWithReason {
                        reason: blocked_reason.clone(),
                        link: Some(link),
                    },
                ),
            )),
        },
    );
    let blocked_event = ReviewFindingEvent::new(
        finding_ref,
        blocked_ordinal,
        blocked_pass,
        blocked_evidence.clone(),
        run_evidence_for_pass(blocked_evidence),
        ReviewFindingEventKind::BlockedWithReason {
            reason: blocked_reason,
            link: Some(Box::new(pending)),
        },
    );

    let mut attaching = pool.begin().await?;
    sqlx::query(
        "SELECT external_link_id
           FROM review_external_link
          WHERE external_link_id = $1
          FOR NO KEY UPDATE",
    )
    .bind(link.into_uuid())
    .fetch_one(&mut *attaching)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'external_link_attachment',
                result_external_link_id = $2,
                result_external_object_key = 'comment-7b4'
          WHERE pass_id = $1",
    )
    .bind(attaching_pass.pass().into_uuid())
    .bind(link.into_uuid())
    .execute(&mut *attaching)
    .await?;
    sqlx::query(
        "INSERT INTO review_external_link_attachment
            (external_link_id, target_id, pass_run_id, pass_id,
             provider_key, object_kind, external_object_key)
         VALUES (
             $1, $2, $3, $4,
             'example-code-host', 'review_comment', 'comment-7b4'
         )",
    )
    .bind(link.into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(attaching_pass.run().run().into_uuid())
    .bind(attaching_pass.pass().into_uuid())
    .execute(&mut *attaching)
    .await?;

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let appending_store = fixture.store.clone();
    let appending_barrier = barrier.clone();
    let mut appending = tokio::spawn(async move {
        appending_barrier.wait().await;
        appending_store
            .append_finding_event(finding_ref.finding(), blocked_event)
            .await
    });
    barrier.wait().await;
    tokio::time::timeout(std::time::Duration::from_secs(1), &mut appending)
        .await
        .expect_err("linked block must wait for the reservation transition lock");
    attaching.commit().await?;
    let blocked_outcome = appending.await.expect("append task remains live");
    assert!(
        blocked_outcome.is_err(),
        "attachment winner must reject the now-stale linked block"
    );
    assert_eq!(
        fixture
            .store
            .load_finding(finding_ref.finding())
            .await?
            .expect("finding remains present")
            .status(),
        ReviewFindingStatus::Accepted
    );
    assert_eq!(
        fixture
            .store
            .load_external_link(link)
            .await?
            .expect("reservation remains present")
            .attachment()
            .expect("non-posting attachment persists")
            .external_object(),
        &key("comment-7b4")
    );
    Ok(())
}

/// attachment returns the canonical aggregate reloaded under the
/// reservation lock, including a publication claim that won the lock first.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn attachment_returns_claim_committed_while_waiting() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let attaching_pass =
        insert_fixture_pass(&fixture, 0x7c0, ReviewPassKind::ImportExternalContext).await;
    let blocked_pass = insert_fixture_pass(&fixture, 0x7c1, ReviewPassKind::Publish).await;
    let attaching_evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[attaching_pass]).await[0].clone();
    let (_, blocked_turn) = start_review_pass(&fixture.store, blocked_pass).await;
    reconcile_review_turn(&pool, blocked_turn).await;

    let link = ReviewExternalLinkId::from_uuid(uuid(0x7c2));
    let reservation = ReviewExternalLink::try_reserve(
        link,
        ReviewExternalLinkAssociation::Target(fixture.target),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
        &fixture.target_snapshot,
    )
    .expect("reservation matches the target");
    fixture
        .store
        .reserve_external_link(reservation.clone())
        .await?;
    let reason = text("provider acknowledgement requires reconciliation");
    let blocked_evidence = pass_evidence(
        blocked_pass,
        ReviewPassKind::Publish,
        ReviewPolicy::version_one(),
        ReviewPassState::Blocked {
            turn: blocked_turn,
            result: Some(ReviewPassResult::ExternalLinkPublicationBlocked(
                ReviewExternalLinkPublicationBlockedResult::new(link, reason.clone()),
            )),
        },
    );
    let blocked_run = run_evidence_for_pass(blocked_evidence.clone());
    let attachment = attachment(link, attaching_evidence, key("comment-7c2"));
    let expected = reservation
        .block_publication(blocked_evidence, blocked_run)
        .expect("blocked pass claims the reservation")
        .attach(attachment.clone())
        .expect("same-target pass may attach after the claim");

    let mut blocking = pool.begin().await?;
    sqlx::query(
        "SELECT external_link_id
           FROM review_external_link
          WHERE external_link_id = $1
          FOR NO KEY UPDATE",
    )
    .bind(link.into_uuid())
    .fetch_one(&mut *blocking)
    .await?;

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let attaching_store = fixture.store.clone();
    let attaching_barrier = barrier.clone();
    let mut attaching = tokio::spawn(async move {
        attaching_barrier.wait().await;
        attaching_store.attach_external_link(link, attachment).await
    });
    barrier.wait().await;
    tokio::time::timeout(std::time::Duration::from_secs(1), &mut attaching)
        .await
        .expect_err("attachment must wait for the reservation transition lock");

    sqlx::query(
        "UPDATE review_pass
            SET state_kind = 'blocked'
          WHERE pass_id = $1",
    )
    .bind(blocked_pass.pass().into_uuid())
    .execute(&mut *blocking)
    .await?;
    sqlx::query(
        "UPDATE review_run
            SET state_kind = 'blocked',
                state_pass_id = $2
          WHERE run_id = $1",
    )
    .bind(blocked_pass.run().run().into_uuid())
    .bind(blocked_pass.pass().into_uuid())
    .execute(&mut *blocking)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'external_link_publication_blocked',
                result_reason = $2,
                result_external_link_id = $3
          WHERE pass_id = $1",
    )
    .bind(blocked_pass.pass().into_uuid())
    .bind(reason.as_str())
    .bind(link.into_uuid())
    .execute(&mut *blocking)
    .await?;
    blocking.commit().await?;

    assert_eq!(
        attaching.await.expect("attachment task remains live")?,
        Some(expected),
        "the returned aggregate must retain the claim committed while waiting"
    );
    Ok(())
}

/// direct attachment and publication-block writers serialize through
/// the reservation root, so the later block observes the winning attachment.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_serializes_attachment_and_publication_block() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let attaching_pass =
        insert_fixture_pass(&fixture, 0x7d0, ReviewPassKind::ImportExternalContext).await;
    let blocked_pass = insert_fixture_pass(&fixture, 0x7d1, ReviewPassKind::Publish).await;
    succeed_fixture_passes(&pool, &fixture.store, &[attaching_pass]).await;
    let (_, blocked_turn) = start_review_pass(&fixture.store, blocked_pass).await;
    reconcile_review_turn(&pool, blocked_turn).await;
    let link = ReviewExternalLinkId::from_uuid(uuid(0x7d2));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                link,
                ReviewExternalLinkAssociation::Target(fixture.target),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &fixture.target_snapshot,
            )
            .expect("reservation matches the target"),
        )
        .await?;

    let mut blocking = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET state_kind = 'blocked'
          WHERE pass_id = $1",
    )
    .bind(blocked_pass.pass().into_uuid())
    .execute(&mut *blocking)
    .await?;
    sqlx::query(
        "UPDATE review_run
            SET state_kind = 'blocked',
                state_pass_id = $2
          WHERE run_id = $1",
    )
    .bind(blocked_pass.run().run().into_uuid())
    .bind(blocked_pass.pass().into_uuid())
    .execute(&mut *blocking)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'external_link_publication_blocked',
                result_reason =
                    'provider acknowledgement requires reconciliation',
                result_external_link_id = $2
          WHERE pass_id = $1",
    )
    .bind(blocked_pass.pass().into_uuid())
    .bind(link.into_uuid())
    .execute(&mut *blocking)
    .await?;

    let mut attaching = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'external_link_attachment',
                result_external_link_id = $2,
                result_external_object_key = 'comment-7d2'
          WHERE pass_id = $1",
    )
    .bind(attaching_pass.pass().into_uuid())
    .bind(link.into_uuid())
    .execute(&mut *attaching)
    .await?;
    sqlx::query(
        "INSERT INTO review_external_link_attachment
            (external_link_id, target_id, pass_run_id, pass_id,
             provider_key, object_kind, external_object_key)
         VALUES (
             $1, $2, $3, $4,
             'example-code-host', 'review_comment', 'comment-7d2'
         )",
    )
    .bind(link.into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(attaching_pass.run().run().into_uuid())
    .bind(attaching_pass.pass().into_uuid())
    .execute(&mut *attaching)
    .await?;

    let committing_block = tokio::spawn(async move { blocking.commit().await });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "deferred publication-block validation waits for the attachment root lock"
    );
    attaching.commit().await?;
    let block_error = committing_block
        .await
        .expect("blocking commit task remains live")
        .expect_err("the later block must observe and reject the attachment");
    assert_sqlstate(&block_error, "23514");
    Ok(())
}

/// a publication-blocked finding reconciles only through
/// the succeeded pass that produced the attached object.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn blocked_publication_reconciles_with_attachment_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x70c, ReviewPassKind::Judge).await;
    let attaching_pass =
        insert_fixture_pass(&fixture, 0x70d, ReviewPassKind::ImportExternalContext).await;
    let other_publish_pass = insert_fixture_pass(&fixture, 0x70e, ReviewPassKind::Publish).await;

    let blocked_session = SessionId::from_uuid(uuid(0x731));
    let blocked_input = AcceptedInputId::from_uuid(uuid(0x732));
    let blocked_turn = TurnId::from_uuid(uuid(0x733));
    insert_active_turn_with_offset(&pool, blocked_session, blocked_input, blocked_turn, 0x4_000)
        .await;
    let blocked_publish_pass = insert_pass_for_target(
        &fixture.store,
        fixture.target,
        0x70f,
        ReviewPassKind::Publish,
        blocked_session,
        blocked_input,
    )
    .await;

    let (running_review, turn) = start_review_pass(&fixture.store, fixture.pass).await;
    let (_, judge_turn) = start_review_pass(&fixture.store, judge_pass).await;
    let (_, attaching_turn) = start_review_pass(&fixture.store, attaching_pass).await;
    let (_, other_publish_turn) = start_review_pass(&fixture.store, other_publish_pass).await;
    start_review_pass(&fixture.store, blocked_publish_pass).await;
    let output_frontier = complete_review_turn(&pool, turn).await;
    let judge_output_frontier = complete_review_turn(&pool, judge_turn).await;
    let attaching_output_frontier = complete_review_turn(&pool, attaching_turn).await;
    let other_publish_output_frontier = complete_review_turn(&pool, other_publish_turn).await;
    reconcile_review_turn(&pool, blocked_turn).await;

    let review_evidence =
        propose_read_only_success(&fixture.store, running_review, output_frontier).await;
    let judge_evidence = conclude_review_pass(
        &fixture.store,
        judge_pass,
        ReviewPassState::Succeeded {
            turn: judge_turn,
            output_frontier: judge_output_frontier,
            result: None,
        },
    )
    .await;
    let attaching_evidence = conclude_review_pass(
        &fixture.store,
        attaching_pass,
        ReviewPassState::Succeeded {
            turn: attaching_turn,
            output_frontier: attaching_output_frontier,
            result: None,
        },
    )
    .await;
    conclude_review_pass(
        &fixture.store,
        other_publish_pass,
        ReviewPassState::Succeeded {
            turn: other_publish_turn,
            output_frontier: other_publish_output_frontier,
            result: None,
        },
    )
    .await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x710)));
    let link = ReviewExternalLinkId::from_uuid(uuid(0x711));
    let blocking_reason = text("publication acknowledgement is unresolved");
    let blocked_evidence = pass_evidence(
        blocked_publish_pass,
        ReviewPassKind::Publish,
        ReviewPolicy::version_one(),
        ReviewPassState::Blocked {
            turn: blocked_turn,
            result: Some(ReviewPassResult::FindingEvent(
                ReviewFindingEventResult::new(
                    finding_ref,
                    ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                    ReviewFindingEventResultKind::BlockedWithReason {
                        reason: blocking_reason.clone(),
                        link: Some(link),
                    },
                ),
            )),
        },
    );
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            review_evidence,
            &fixture.target_snapshot,
        ))
        .await?;
    fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            finding_event(
                finding_ref,
                ReviewEventOrdinal::one(),
                judge_evidence,
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await?;
    let reservation = ReviewExternalLink::try_reserve(
        link,
        ReviewExternalLinkAssociation::Finding(finding_ref),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
        &fixture.target_snapshot,
    )
    .expect("reservation matches the target");
    fixture
        .store
        .reserve_external_link(reservation.clone())
        .await?;
    let blocked_run = run_evidence_for_pass(blocked_evidence.clone());
    let expected_blocked_link = reservation
        .clone()
        .block_publication(blocked_evidence.clone(), blocked_run)
        .expect("finding publication block claims its pending reservation");
    assert_eq!(
        fixture
            .store
            .block_external_link_publication(link, blocked_evidence, blocked_run)
            .await?,
        Some(expected_blocked_link.clone())
    );
    assert_eq!(
        fixture.store.load_external_link(link).await?,
        Some(expected_blocked_link),
        "finding publication-block claims survive aggregate reload"
    );

    let incomplete = fixture
        .store
        .attach_external_link(
            link,
            attachment(
                link,
                attaching_evidence.clone(),
                key("comment-without-posted-event"),
            ),
        )
        .await
        .expect_err("blocked publication cannot attach without an atomic posted event");
    assert!(matches!(
        incomplete,
        ReviewWorkflowStoreError::IncompletePublicationReconciliation
    ));

    let posted_ordinal = ReviewEventOrdinal::try_new(3).expect("positive ordinal");
    let attached = fixture
        .store
        .attach_external_link(
            link,
            posted_attachment(
                link,
                attaching_evidence,
                key("comment-711"),
                finding_ref,
                posted_ordinal,
            ),
        )
        .await?
        .expect("publication attachment persists");

    let mismatched = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, external_link_id,
             external_link_association_kind)
         VALUES ($1, 3, $2, $3, $4, $5, 'posted', NULL, NULL, $6, 'finding')",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(other_publish_pass.pass().into_uuid())
    .bind(other_publish_pass.run().run().into_uuid())
    .bind(link.into_uuid())
    .execute(&pool)
    .await
    .expect_err("posted event pass must equal the attachment producer");
    assert_sqlstate(&mismatched, "23514");

    let attachment_evidence = attached
        .attachment()
        .expect("attached link carries the producing pass")
        .pass_evidence()
        .clone();
    let posted_event = ReviewFindingEvent::new(
        finding_ref,
        posted_ordinal,
        attachment_evidence.reference(),
        attachment_evidence.clone(),
        run_evidence_for_pass(attachment_evidence),
        ReviewFindingEventKind::Posted {
            link: Box::new(
                signalbox_domain::ReviewFindingExternalLinkRef::try_new(finding_ref, &attached)
                    .expect("attached link belongs to the finding"),
            ),
        },
    );
    let posted = fixture
        .store
        .load_finding(finding_ref.finding())
        .await?
        .expect("publication reconciliation persists");
    assert_eq!(
        posted.events().last(),
        Some(&posted_event),
        "attachment commits the exact posting event"
    );
    assert_eq!(posted.status(), ReviewFindingStatus::Posted);

    let replayed_attachment = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, external_link_id,
             external_link_association_kind)
         VALUES ($1, 4, $2, $3, $4, $5, 'posted', NULL, NULL, $6, 'finding')",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(attaching_pass.pass().into_uuid())
    .bind(attaching_pass.run().run().into_uuid())
    .bind(link.into_uuid())
    .execute(&pool)
    .await
    .expect_err("reconciliation cannot replay the first posting's attachment");
    assert_sqlstate(&replayed_attachment, "23514");
    Ok(())
}

/// a frozen review target rejects in-place revision mutation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn target_evidence_is_append_only() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = ReviewWorkflowStore::new(pool.clone());
    let target_id = ReviewTargetId::from_uuid(uuid(0x301));
    let target = ReviewTarget::try_new(
        target_id,
        key("example-code-host"),
        key("example/repository"),
        ReviewTargetSubject::Commit,
        key("0123456789abcdef"),
        None,
        None,
    )
    .expect("fixture target topology is valid");
    store.insert_target(&target).await.expect("target persists");

    let mutation = sqlx::query(
        "UPDATE review_target
            SET head_revision = 'different'
          WHERE target_id = $1",
    )
    .bind(target_id.into_uuid())
    .execute(&pool)
    .await
    .expect_err("target evidence is append-only");
    assert_sqlstate(&mutation, "23514");

    Ok(())
}

/// append-only workflow evidence also rejects statement-
/// level truncation, which bypasses row delete triggers.
async fn assert_review_workflow_truncate_rejected(
    pool: &PgPool,
    table: &'static str,
    statement: &'static str,
) {
    let error = sqlx::query(statement)
        .execute(pool)
        .await
        .expect_err("every workflow table rejects truncate");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.code())
            .as_deref(),
        Some("23514"),
        "{table}",
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_workflow_tables_reject_truncate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_target",
        "TRUNCATE TABLE review_target CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_run",
        "TRUNCATE TABLE review_run CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_pass",
        "TRUNCATE TABLE review_pass CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_finding",
        "TRUNCATE TABLE review_finding CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_pass_produced_finding",
        "TRUNCATE TABLE review_pass_produced_finding CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_pass_finding_inventory_seal",
        "TRUNCATE TABLE review_pass_finding_inventory_seal CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_external_link",
        "TRUNCATE TABLE review_external_link CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_external_object_identity",
        "TRUNCATE TABLE review_external_object_identity CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_external_link_attachment",
        "TRUNCATE TABLE review_external_link_attachment CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_finding_event",
        "TRUNCATE TABLE review_finding_event CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_finding_event_head",
        "TRUNCATE TABLE review_finding_event_head CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_external_link_observation",
        "TRUNCATE TABLE review_external_link_observation CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_workflow_command",
        "TRUNCATE TABLE review_workflow_command CASCADE",
    )
    .await;
    Ok(())
}

/// maximum-size target keys remain persistable without a wide-index
/// size failure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn maximum_target_keys_do_not_overflow_indexes() -> Result<(), Box<dyn Error>> {
    const TARGET_IDENTITY: u128 = 0x750;

    let (_container, pool) = migrated_postgres().await?;
    let store = ReviewWorkflowStore::new(pool);
    let target = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(TARGET_IDENTITY)),
        maximum_width_key(MaximumWidthKeyRole::Provider),
        maximum_width_key(MaximumWidthKeyRole::Repository),
        ReviewTargetSubject::Commit,
        maximum_width_key(MaximumWidthKeyRole::HeadRevision),
        Some(maximum_width_key(MaximumWidthKeyRole::BaseRevision)),
        None,
    )
    .expect("maximum-size target keys are admitted");

    store.insert_target(&target).await?;
    assert_eq!(store.load_target(target.id()).await?, Some(target));
    Ok(())
}

/// exact review-command replay and effect recovery preserve one result.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_workflow_command_receipts_replay_and_recover() -> Result<(), Box<dyn Error>> {
    const TARGET_IDENTITY: u128 = 0x760;
    const RECOVERED_TARGET_IDENTITY: u128 = 0x761;
    const COMMAND_IDENTITY: u128 = 0x762;
    const RECOVERY_COMMAND_IDENTITY: u128 = 0x763;

    let (_container, pool) = migrated_postgres().await?;
    let store = ReviewWorkflowStore::new(pool);
    let target = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(TARGET_IDENTITY)),
        key("provider"),
        key("repository"),
        ReviewTargetSubject::ChangeRequest(
            ReviewChangeRequestNumber::try_new(42).expect("fixture number is positive"),
        ),
        key("head"),
        Some(key("base")),
        None,
    )
    .expect("target fixture is admitted");
    let command_id = DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY));
    let command = ReviewWorkflowCommand::new(
        command_id,
        [7; 32],
        ReviewWorkflowOperation::CreateTarget(target.clone()),
    );
    let mut service = ReviewWorkflowCommandService::new(store.clone());
    let expected =
        ReviewWorkflowCommandOutcome::Recorded(ReviewWorkflowCommandResult::TargetCreated {
            target: target.id(),
        });

    assert_eq!(service.execute(command.clone()).await?, expected);
    assert_eq!(service.execute(command).await?, expected);
    assert_eq!(
        store
            .load_command_outcome(
                command_id,
                [7; 32],
                ReviewWorkflowOperationKind::CreateTarget,
            )
            .await?,
        Some(expected.clone()),
    );
    assert_eq!(store.load_target(target.id()).await?, Some(target.clone()));
    assert_eq!(
        store
            .load_command_outcome(
                command_id,
                [8; 32],
                ReviewWorkflowOperationKind::CreateTarget,
            )
            .await?,
        Some(ReviewWorkflowCommandOutcome::ConflictingReuse { command_id }),
    );
    assert_eq!(
        service
            .execute(ReviewWorkflowCommand::new(
                command_id,
                [8; 32],
                ReviewWorkflowOperation::CreateTarget(target),
            ))
            .await?,
        ReviewWorkflowCommandOutcome::ConflictingReuse { command_id },
    );

    let recovered_target = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(RECOVERED_TARGET_IDENTITY)),
        key("provider"),
        key("repository"),
        ReviewTargetSubject::ChangeRequest(
            ReviewChangeRequestNumber::try_new(43).expect("fixture number is positive"),
        ),
        key("later-head"),
        Some(key("later-base")),
        None,
    )
    .expect("recovery target fixture is admitted");
    store.insert_target(&recovered_target).await?;
    let recovery_command_id = DurableCommandId::from_uuid(uuid(RECOVERY_COMMAND_IDENTITY));
    let recovery_command = ReviewWorkflowCommand::new(
        recovery_command_id,
        [9; 32],
        ReviewWorkflowOperation::CreateTarget(recovered_target.clone()),
    );
    let recovered =
        ReviewWorkflowCommandOutcome::Recorded(ReviewWorkflowCommandResult::TargetCreated {
            target: recovered_target.id(),
        });

    assert_eq!(service.execute(recovery_command.clone()).await?, recovered);
    assert_eq!(service.execute(recovery_command).await?, recovered);
    assert_eq!(
        store.load_target(recovered_target.id()).await?,
        Some(recovered_target),
    );
    Ok(())
}

/// a formerly legal run-only commit remains loadable and its exact
/// command retry completes the admitted pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn start_run_recovers_a_loadable_run_only_commit() -> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x777;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = review_command_admission_fixture(&pool).await;
    fixture.store.insert_run(&fixture.run).await?;
    let partial = fixture
        .store
        .load_run_with_pass(fixture.run.reference().run())
        .await?
        .expect("run-only admission remains loadable");
    assert_eq!(partial.0.reference(), fixture.run.reference());
    assert_eq!(partial.0.workflow(), fixture.run.workflow());
    assert_eq!(partial.0.policy(), fixture.run.policy());
    assert_eq!(partial.0.recorded_pass(), None);
    assert_eq!(partial.1, None);

    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY)),
        [13; 32],
        ReviewWorkflowOperation::StartRun {
            run: fixture.run.clone(),
            pass: fixture.pass.clone(),
        },
    );
    let expected =
        ReviewWorkflowCommandOutcome::Recorded(ReviewWorkflowCommandResult::RunStarted {
            run: fixture.run.reference().run(),
            pass: fixture.pass.reference().pass(),
        });
    let mut service = ReviewWorkflowCommandService::new(fixture.store.clone());

    assert_eq!(service.execute(command).await?, expected);
    assert_eq!(
        fixture
            .store
            .load_run_with_pass(fixture.run.reference().run())
            .await?,
        Some((fixture.run, Some(fixture.pass))),
    );
    Ok(())
}

/// a rejected fresh admission cannot leave a run-only aggregate.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn start_run_rolls_back_run_when_pass_admission_fails() -> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x778;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = review_command_admission_fixture(&pool).await;
    fixture
        .store
        .insert_run_and_pass(&fixture.run, &fixture.pass)
        .await?;
    let run_reference = ReviewRunRef::new(fixture.target, ReviewRunId::from_uuid(uuid(0x779)));
    let pass_reference = ReviewPassRef::new(run_reference, ReviewPassId::from_uuid(uuid(0x77a)));
    let mut rejected_run = ReviewRun::new(
        run_reference,
        ReviewWorkflowKind::ReadOnlyReview,
        ReviewPolicy::version_one(),
    );
    let rejected_pass = ReviewPass::try_new(
        pass_reference,
        ReviewPassKind::ReadOnlyReview,
        &mut rejected_run,
        fixture.session,
        ReviewPassAcceptedInputEvidence::new(
            fixture.accepted_input,
            fixture.session,
            Some(fixture.origin_turn),
        ),
    )
    .expect("the conflicting pass fixture is domain-valid");
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY)),
        [14; 32],
        ReviewWorkflowOperation::StartRun {
            run: rejected_run,
            pass: rejected_pass,
        },
    );
    let mut service = ReviewWorkflowCommandService::new(fixture.store.clone());

    assert!(service.execute(command).await.is_err());
    assert_eq!(fixture.store.load_run(run_reference.run()).await?, None);
    assert_eq!(fixture.store.load_pass(pass_reference.pass()).await?, None);
    Ok(())
}

/// atomic admission rejects a pass owned by another run root.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn atomic_run_pass_admission_rejects_cross_wired_roots() -> Result<(), Box<dyn Error>> {
    const STORED_RUN_IDENTITY: u128 = 0x77b;
    const STORED_PASS_IDENTITY: u128 = 0x77c;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = review_command_admission_fixture(&pool).await;
    let stored_run_reference = ReviewRunRef::new(
        fixture.target,
        ReviewRunId::from_uuid(uuid(STORED_RUN_IDENTITY)),
    );
    let stored_pass_reference = ReviewPassRef::new(
        stored_run_reference,
        ReviewPassId::from_uuid(uuid(STORED_PASS_IDENTITY)),
    );
    let mut stored_run = ReviewRun::new(
        stored_run_reference,
        ReviewWorkflowKind::ReadOnlyReview,
        ReviewPolicy::version_one(),
    );
    let stored_pass = ReviewPass::try_new(
        stored_pass_reference,
        ReviewPassKind::ReadOnlyReview,
        &mut stored_run,
        fixture.session,
        ReviewPassAcceptedInputEvidence::new(
            fixture.accepted_input,
            fixture.session,
            Some(fixture.origin_turn),
        ),
    )
    .expect("the stored-run pass fixture is domain-valid");
    fixture.store.insert_run(&stored_run).await?;

    let error = fixture
        .store
        .insert_run_and_pass(&fixture.run, &stored_pass)
        .await
        .expect_err("cross-wired roots must fail before insertion");

    assert!(matches!(
        error,
        ReviewWorkflowStoreError::InvalidInsertion(ReviewWorkflowInsertionError::RunPassMismatch)
    ));
    assert_eq!(
        fixture
            .store
            .load_run(fixture.run.reference().run())
            .await?,
        None
    );
    assert_eq!(
        fixture
            .store
            .load_pass(stored_pass.reference().pass())
            .await?,
        None
    );
    assert!(
        fixture
            .store
            .load_run(stored_run.reference().run())
            .await?
            .is_some()
    );
    Ok(())
}

/// run admission recovery ignores later lifecycle advancement.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn start_run_receipt_recovers_after_lifecycle_advancement() -> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x764;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let queued_run = fixture
        .store
        .load_run(fixture.run.run())
        .await?
        .expect("queued fixture run exists");
    let queued_pass = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await?
        .expect("queued fixture pass exists");
    let command_id = DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY));
    let command = ReviewWorkflowCommand::new(
        command_id,
        [10; 32],
        ReviewWorkflowOperation::StartRun {
            run: queued_run.clone(),
            pass: queued_pass.clone(),
        },
    );
    let expected =
        ReviewWorkflowCommandOutcome::Recorded(ReviewWorkflowCommandResult::RunStarted {
            run: fixture.run.run(),
            pass: fixture.pass.pass(),
        });

    let (running_pass, _turn) = start_review_pass(&fixture.store, fixture.pass).await;
    let running_run = fixture
        .store
        .load_run(fixture.run.run())
        .await?
        .expect("running fixture run exists");
    assert_ne!(running_run.state(), queued_run.state());
    assert_ne!(running_pass.state(), queued_pass.state());

    let mut service = ReviewWorkflowCommandService::new(fixture.store);
    assert_eq!(service.execute(command.clone()).await?, expected);
    assert_eq!(service.execute(command).await?, expected);
    Ok(())
}

/// a findings receipt cannot omit its stable result count.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn findings_receipt_rejects_missing_count() -> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x765;

    let (_container, pool) = migrated_postgres().await?;
    let error = sqlx::query(
        "INSERT INTO review_workflow_command
            (command_id, command_kind, storage_version, semantic_digest,
             operation_kind, result_kind, result_run_id, result_pass_id)
         VALUES ($1, 'review_workflow', 1, $2, 'record_findings',
                 'findings_recorded', $3, $4)",
    )
    .bind(uuid(COMMAND_IDENTITY))
    .bind([11_u8; 32].as_slice())
    .bind(uuid(0x766))
    .bind(uuid(0x767))
    .execute(&pool)
    .await
    .expect_err("the receipt shape requires a stable finding count");

    assert_sqlstate(&error, "23514");
    Ok(())
}

/// activation recovery recognizes the same pass after completion.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn activation_receipt_recovers_after_pass_completion() -> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x768;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let (running_pass, turn) = start_review_pass(&fixture.store, fixture.pass).await;
    let running_run = fixture
        .store
        .load_run(fixture.run.run())
        .await?
        .expect("running fixture run exists");
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY)),
        [12; 32],
        ReviewWorkflowOperation::ActivatePass {
            run: running_run,
            pass: running_pass,
        },
    );
    let expected =
        ReviewWorkflowCommandOutcome::Recorded(ReviewWorkflowCommandResult::PassActivated {
            run: fixture.run.run(),
            pass: fixture.pass.pass(),
        });

    fail_review_turn(&pool, turn).await;
    conclude_review_pass(
        &fixture.store,
        fixture.pass,
        ReviewPassState::Failed { turn },
    )
    .await;

    let mut service = ReviewWorkflowCommandService::new(fixture.store);
    assert_eq!(service.execute(command.clone()).await?, expected);
    assert_eq!(service.execute(command).await?, expected);
    Ok(())
}

/// findings recovery compares immutable proposals after disposition.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn findings_receipt_recovers_after_later_disposition() -> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x769;
    const JUDGE_PASS_IDENTITY: u128 = 0x76a;
    const FINDING_IDENTITY: u128 = 0x76b;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass =
        insert_fixture_pass(&fixture, JUDGE_PASS_IDENTITY, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(
        fixture.pass,
        ReviewFindingId::from_uuid(uuid(FINDING_IDENTITY)),
    );
    let producing_pass = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    let recorded_finding = finding(
        finding_ref,
        producing_pass.clone(),
        &fixture.target_snapshot,
    );
    let recorded_findings = vec![recorded_finding];
    let finding_count = recorded_findings.len();
    fixture
        .store
        .insert_findings(&producing_pass, &recorded_findings)
        .await?;
    fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            finding_event(
                finding_ref,
                ReviewEventOrdinal::one(),
                evidence[1].clone(),
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await?
        .expect("the canonical finding accepts its disposition");
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY)),
        [13; 32],
        ReviewWorkflowOperation::RecordFindings {
            pass: producing_pass,
            findings: recorded_findings,
        },
    );
    let expected =
        ReviewWorkflowCommandOutcome::Recorded(ReviewWorkflowCommandResult::FindingsRecorded {
            run: fixture.run.run(),
            pass: fixture.pass.pass(),
            finding_count,
        });

    let mut service = ReviewWorkflowCommandService::new(fixture.store);
    assert_eq!(service.execute(command.clone()).await?, expected);
    assert_eq!(service.execute(command).await?, expected);
    Ok(())
}

/// the generic result-free terminal command commits and replays one
/// exact run/pass completion receipt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn complete_pass_commits_and_replays_terminal_status() -> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x790;
    const PASS_IDENTITY: u128 = 0x791;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let pass_ref = insert_fixture_pass(&fixture, PASS_IDENTITY, ReviewPassKind::Judge).await;
    let (running_pass, turn) = start_review_pass(&fixture.store, pass_ref).await;
    let running_run = fixture
        .store
        .load_run(pass_ref.run().run())
        .await?
        .expect("running run exists");
    let output_frontier = complete_review_turn(&pool, turn).await;
    let session = running_pass.session();
    let accepted_input = running_pass.accepted_input();
    let policy = running_run.policy();
    let terminal_pass = running_pass
        .transition(
            ReviewPassState::Succeeded {
                turn,
                output_frontier,
                result: None,
            },
            Some(ReviewPassTurnEvidence::new(
                turn,
                session,
                accepted_input,
                ReviewPassTurnOutcome::Completed,
                Some(output_frontier),
            )),
        )
        .expect("terminal fixture pass is valid");
    let terminal_run = running_run
        .transition(
            ReviewRunState::Succeeded {
                concluding_pass: pass_ref,
            },
            Some(ReviewPassEvidence::from_pass(&terminal_pass, policy)),
        )
        .expect("terminal fixture run is valid");
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY)),
        [0x79; 32],
        ReviewWorkflowOperation::CompletePass {
            run: terminal_run.clone(),
            pass: terminal_pass.clone(),
        },
    );
    let expected =
        ReviewWorkflowCommandOutcome::Recorded(ReviewWorkflowCommandResult::PassCompleted {
            run: pass_ref.run().run(),
            pass: pass_ref.pass(),
            status: ReviewPassCompletionStatus::Succeeded,
        });
    let mut service = ReviewWorkflowCommandService::new(fixture.store.clone());

    assert_eq!(service.execute(command.clone()).await?, expected);
    assert_eq!(service.execute(command).await?, expected);
    assert_eq!(
        fixture.store.load_run(pass_ref.run().run()).await?,
        Some(terminal_run)
    );
    assert_eq!(
        fixture.store.load_pass(pass_ref.pass()).await?,
        Some(terminal_pass)
    );
    Ok(())
}

#[track_caller]
fn expect_new_orchestration_claim(
    claim: ReviewOrchestrationCommandClaim,
) -> ReviewOrchestrationCommandGuard {
    match claim {
        ReviewOrchestrationCommandClaim::New(guard) => {
            assert!(!guard.is_pending());
            guard
        }
        ReviewOrchestrationCommandClaim::ExistingRecorded(_) => {
            panic!("fresh orchestration command unexpectedly replayed")
        }
        ReviewOrchestrationCommandClaim::Conflicting => {
            panic!("fresh orchestration command unexpectedly conflicted")
        }
    }
}

#[track_caller]
fn expect_pending_orchestration_claim(
    claim: ReviewOrchestrationCommandClaim,
) -> ReviewOrchestrationCommandGuard {
    match claim {
        ReviewOrchestrationCommandClaim::New(guard) => {
            assert!(guard.is_pending());
            guard
        }
        ReviewOrchestrationCommandClaim::ExistingRecorded(_) => {
            panic!("pending orchestration command unexpectedly replayed")
        }
        ReviewOrchestrationCommandClaim::Conflicting => {
            panic!("pending orchestration command unexpectedly conflicted")
        }
    }
}

#[track_caller]
fn expect_recorded_orchestration_claim(
    claim: ReviewOrchestrationCommandClaim,
) -> ReviewOrchestrationCommandResult {
    match claim {
        ReviewOrchestrationCommandClaim::ExistingRecorded(result) => result,
        ReviewOrchestrationCommandClaim::New(_) => {
            panic!("recorded orchestration command unexpectedly acquired a new fence")
        }
        ReviewOrchestrationCommandClaim::Conflicting => {
            panic!("recorded orchestration command unexpectedly conflicted")
        }
    }
}

struct PreparedOrchestrationFixture {
    workflow: ReviewWorkflowStore,
    store: PostgresReviewOrchestrationStore,
    attempt_id: ReviewOrchestrationAttemptId,
    attempt: ReviewOrchestrationAttempt,
    import: ReviewImportOutcome,
    claim: ReviewConcernClaim,
    plan: ReviewJudgmentPlan,
    finding_ref: ReviewFindingRef,
    evidence: Vec<ReviewPassEvidence>,
}

async fn prepare_orchestration_fixture(
    pool: &PgPool,
) -> Result<PreparedOrchestrationFixture, Box<dyn Error>> {
    prepare_orchestration_fixture_with_findings(pool, 1).await
}

/// Prepares one sealed attempt carrying `findings` findings on a single target.
///
/// Every finding shares the fixture's target, which is what makes the count a
/// meaningful load parameter: the workflow store reconstructs findings by whole
/// target graph, so a loader that reaches for one finding at a time scales with
/// the product of the claim's members and the target's.
async fn prepare_orchestration_fixture_with_findings(
    pool: &PgPool,
    findings: usize,
) -> Result<PreparedOrchestrationFixture, Box<dyn Error>> {
    const IMPORT_PASS_IDENTITY: u128 = 0x7a0;
    const ANALYSIS_PASS_IDENTITY: u128 = 0x7a1;
    const EFFECT_PASS_IDENTITY: u128 = 0x7a2;
    const FIX_PASS_IDENTITY: u128 = 0x7a3;
    const FINDING_IDENTITY: u128 = 0x7a4;
    const ATTEMPT_IDENTITY: u128 = 0x7a5;
    const ADDITIONAL_FINDING_IDENTITY_BASE: u128 = 0x7c0;

    let fixture = insert_review_pass_fixture(pool).await;
    let import_pass = insert_fixture_pass(
        &fixture,
        IMPORT_PASS_IDENTITY,
        ReviewPassKind::ImportExternalContext,
    )
    .await;
    let analysis_pass =
        insert_fixture_pass(&fixture, ANALYSIS_PASS_IDENTITY, ReviewPassKind::Judge).await;
    let effect_pass =
        insert_fixture_pass(&fixture, EFFECT_PASS_IDENTITY, ReviewPassKind::Judge).await;
    let fix_pass = insert_fixture_pass(&fixture, FIX_PASS_IDENTITY, ReviewPassKind::Fix).await;
    let evidence = succeed_fixture_passes(
        pool,
        &fixture.store,
        &[
            fixture.pass,
            import_pass,
            analysis_pass,
            effect_pass,
            fix_pass,
        ],
    )
    .await;
    let finding_refs = (0..findings)
        .map(|index| {
            let identity = if index == 0 {
                FINDING_IDENTITY
            } else {
                ADDITIONAL_FINDING_IDENTITY_BASE + u128::try_from(index).unwrap_or_default()
            };
            ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(identity)))
        })
        .collect::<Vec<_>>();
    let finding_ref = *finding_refs
        .first()
        .expect("the fixture carries at least one finding");
    let producer = pass_with_produced_findings(finding_refs.clone(), evidence[0].clone());
    let proposed = finding_refs
        .iter()
        .map(|reference| finding(*reference, producer.clone(), &fixture.target_snapshot))
        .collect::<Vec<_>>();
    fixture.store.insert_findings(&producer, &proposed).await?;
    let mut canonical_findings = Vec::with_capacity(finding_refs.len());
    for reference in &finding_refs {
        canonical_findings.push(
            fixture
                .store
                .load_finding(reference.finding())
                .await?
                .expect("canonical finding exists"),
        );
    }

    let attempt_id = ReviewOrchestrationAttemptId::from_uuid(uuid(ATTEMPT_IDENTITY));
    let attempt = ReviewOrchestrationAttempt::try_new(
        attempt_id,
        fixture.target,
        ReviewPolicy::version_one(),
        key("initial-five-v1"),
        ReviewStageTemplateDigests::new(
            ReviewTemplateDigest::new([1; 32]),
            ReviewTemplateDigest::new([2; 32]),
            ReviewTemplateDigest::new([3; 32]),
            ReviewTemplateDigest::new([4; 32]),
        ),
        vec![ReviewConcernSpec::new(
            key("correctness"),
            ReviewTemplateDigest::new([5; 32]),
        )],
    )?;
    let import = ReviewImportOutcome::Succeeded {
        pass: Box::new(evidence[1].clone()),
        run: run_evidence_for_pass(evidence[1].clone()),
        external_link: None,
        template_digest: ReviewTemplateDigest::new([1; 32]),
        context: ReviewImportedContextEvidence::new(import_pass, [6; 32]),
    };
    let claim = ReviewConcernClaim::new(
        key("correctness"),
        ReviewTemplateDigest::new([5; 32]),
        ReviewConcernOutcome::Succeeded(Box::new(ReviewConcernSuccess::new(
            producer,
            run_evidence_for_pass(evidence[0].clone()),
            ReviewTemplateDigest::new([5; 32]),
            canonical_findings,
        ))),
    );
    let plan = ReviewJudgmentPlan::new(
        evidence[2].clone(),
        run_evidence_for_pass(evidence[2].clone()),
        ReviewTemplateDigest::new([2; 32]),
        finding_refs
            .iter()
            .map(|reference| {
                ReviewJudgmentPlanMember::new(*reference, ReviewPlannedDisposition::Accepted)
            })
            .collect(),
    );
    let mut store = PostgresReviewOrchestrationStore::new(pool.clone());
    assert_eq!(
        store.record_attempt(attempt.clone()).await?,
        ReviewDurableSealOutcome::Recorded
    );
    assert_eq!(
        store.record_import(attempt_id, import.clone()).await?,
        ReviewDurableSealOutcome::Recorded
    );
    assert_eq!(
        store
            .record_concern_claim(attempt_id, claim.clone())
            .await?,
        ReviewDurableSealOutcome::Recorded
    );
    assert_eq!(
        store
            .seal_complete_fanout(attempt_id, vec![claim.clone()])
            .await?,
        ReviewDurableSealOutcome::Recorded
    );
    assert_eq!(
        store.seal_judgment_plan(attempt_id, plan.clone()).await?,
        ReviewDurableSealOutcome::Recorded
    );
    Ok(PreparedOrchestrationFixture {
        workflow: fixture.store,
        store,
        attempt_id,
        attempt,
        import,
        claim,
        plan,
        finding_ref,
        evidence,
    })
}

fn orchestration_command(
    fixture: &PreparedOrchestrationFixture,
    identity: u128,
    digest: [u8; 32],
) -> ReviewOrchestrationCommand {
    ReviewOrchestrationCommand {
        command_id: DurableCommandId::from_uuid(uuid(identity)),
        semantic_digest: digest,
        attempt: fixture.attempt_id,
        kind: ReviewOrchestrationCommandKind::JudgmentEffect,
    }
}

async fn incomplete_judgment_result(
    fixture: &PreparedOrchestrationFixture,
) -> Result<ReviewOrchestrationCommandResult, ReviewOrchestrationStoreError> {
    Ok(ReviewOrchestrationCommandResult {
        attempt: fixture.attempt_id,
        stage: ReviewOrchestrationStage::JudgmentIncomplete,
        progress: fixture
            .store
            .load_progress(fixture.attempt_id)
            .await?
            .expect("planned judgment has durable progress"),
    })
}

/// A snapshot reports one database snapshot, not a seam between several.
///
/// The whole projection is reconstructed inside a single read-only
/// `REPEATABLE READ` transaction, so its facts are all drawn from the instant
/// the transaction took its snapshot. This forces the interleave that a torn
/// read needs and shows it cannot happen: the snapshot is stopped after its
/// first read has fixed its MVCC snapshot, a writer then commits an effect, and
/// the snapshot is released to run every remaining loader. Those later loaders
/// must still report the pre-write state.
///
/// The stop is a table lock on `review_orchestration_import`, which the stage
/// ladder reads immediately after the attempt row. It is deliberately not a
/// timing delay: the writer commits only once `pg_stat_activity` shows the
/// reader blocked, so the ordering is observed rather than assumed.
///
/// Before the read became one transaction, each loader opened its own — so the
/// effect written here landed between them and the snapshot reported it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_snapshot_excludes_a_write_committed_after_it_began()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres_with_max_connections(6).await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;

    let mut blocker = pool.begin().await?;
    sqlx::query("LOCK TABLE review_orchestration_import IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await?;

    let reader = fixture.store.clone();
    let attempt_id = fixture.attempt_id;
    let reading = tokio::spawn(async move { reader.load_snapshot(attempt_id).await });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the snapshot must be waiting on the held import lock before the writer commits"
    );

    let mut writer = PostgresReviewOrchestrationStore::new(pool.clone());
    assert_eq!(
        writer
            .record_applied_judgment_effect(ReviewJudgmentEffectId::new(
                attempt_id,
                fixture.finding_ref,
            ))
            .await?,
        ReviewDurableSealOutcome::Recorded,
        "the concurrent writer must commit while the snapshot is still blocked"
    );

    blocker.rollback().await?;
    let facts = tokio::time::timeout(std::time::Duration::from_secs(60), reading)
        .await???
        .expect("the sealed attempt has a snapshot");

    assert!(
        facts.applied_judgment_effects.is_empty(),
        "the snapshot reported an effect committed after it began: {:?}",
        facts.applied_judgment_effects
    );
    assert_eq!(
        facts.current_stage,
        ReviewOrchestrationCurrentStage::AwaitingJudgmentEffects,
        "the reported stage must agree with the effects the same snapshot reports"
    );

    // The write is durable; only this snapshot's view of it was fixed earlier.
    assert_eq!(
        fixture.store.current_stage(attempt_id).await?,
        Some(ReviewOrchestrationCurrentStage::AwaitingRepair)
    );
    Ok(())
}

/// A snapshot under construction blocks no writer.
///
/// An import-table lock pauses `load_snapshot` mid-construction while
/// `insert_active_turn_with_offset` writes `accepted_input` and `turn_lifecycle`.
/// Completing that write before the timeout proves the snapshot blocks no writer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_snapshot_does_not_block_input_or_turns() -> Result<(), Box<dyn Error>>
{
    const WRITER_SESSION: u128 = 0x7e0;
    const WRITER_INPUT: u128 = 0x7e1;
    const WRITER_TURN: u128 = 0x7e2;
    const WRITER_OFFSET: u128 = 0x7e3;

    let (_container, pool) = migrated_postgres_with_max_connections(6).await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;

    let mut blocker = pool.begin().await?;
    sqlx::query("LOCK TABLE review_orchestration_import IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await?;

    let reader = fixture.store.clone();
    let attempt_id = fixture.attempt_id;
    let reading = tokio::spawn(async move { reader.load_snapshot(attempt_id).await });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the snapshot must be mid-construction before the writer runs"
    );

    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        insert_active_turn_with_offset(
            &pool,
            SessionId::from_uuid(uuid(WRITER_SESSION)),
            AcceptedInputId::from_uuid(uuid(WRITER_INPUT)),
            TurnId::from_uuid(uuid(WRITER_TURN)),
            WRITER_OFFSET,
        ),
    )
    .await
    .expect("a snapshot under construction must not delay input submission or turn start");

    blocker.rollback().await?;
    tokio::time::timeout(std::time::Duration::from_secs(60), reading)
        .await???
        .expect("the sealed attempt has a snapshot");
    Ok(())
}

/// The snapshot holds exactly one pooled connection for its whole construction.
///
/// A single connection is a single transaction, which is what makes the read
/// coherent under MVCC rather than under locks. Pinning it against a
/// one-connection pool states that structurally: a construction that reached
/// for a second connection — one holding a guard transaction while its loaders
/// ran elsewhere — could not finish here at all.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_snapshot_completes_on_a_single_connection()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres_with_max_connections(1).await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;

    let facts = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        fixture.store.load_snapshot(fixture.attempt_id),
    )
    .await
    .expect("a one-connection pool must satisfy the snapshot")?
    .expect("the sealed attempt has a snapshot");

    assert_eq!(
        facts.current_stage,
        ReviewOrchestrationCurrentStage::AwaitingJudgmentEffects
    );
    Ok(())
}

/// Snapshot cost grows linearly, not quadratically, in an attempt's findings.
///
/// Two independent defects once made it quadratic. The stage ladder
/// independently re-ran nearly every loader the snapshot ran again on the next
/// line, and each concern finding was fetched with a loader that reconstructs
/// the whole target finding graph in order to return a single row — so the
/// claim's members multiplied the target's. The wire contract admits 1,024
/// findings, where a quadratic is not a constant factor.
///
/// The marginal assertion is the load-bearing one: an absolute ceiling can be
/// met by a quadratic on a small fixture, but a per-finding bound cannot. Under
/// either defect the marginal cost is itself proportional to the finding count,
/// so it fails whichever one returns.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_snapshot_cost_is_linear_in_findings() -> Result<(), Box<dyn Error>> {
    /// Statements one additional finding on the attempt may add.
    ///
    /// A finding costs one projection plus its event and external-link reads.
    /// Measured marginal cost is 8; the budget carries headroom over that and
    /// stays far below what a per-finding target-graph reconstruction needs.
    /// For scale, the same two fixtures measured 77 and 125 statements here
    /// against 172 and 1,180 before this shape was fixed.
    const STATEMENTS_PER_ADDITIONAL_FINDING: i64 = 10;
    const FEW_FINDINGS: usize = 2;
    const MANY_FINDINGS: usize = 8;

    let (_small_container, small_pool) = migrated_postgres_counting_statements().await?;
    let small = prepare_orchestration_fixture_with_findings(&small_pool, FEW_FINDINGS).await?;
    let (small_snapshot, small_statements) =
        statements_executed(&small_pool, small.store.load_snapshot(small.attempt_id)).await?;
    assert!(
        small_snapshot?.is_some(),
        "the sealed attempt must produce a snapshot"
    );

    let (_large_container, large_pool) = migrated_postgres_counting_statements().await?;
    let large = prepare_orchestration_fixture_with_findings(&large_pool, MANY_FINDINGS).await?;
    let (large_snapshot, large_statements) =
        statements_executed(&large_pool, large.store.load_snapshot(large.attempt_id)).await?;
    assert!(
        large_snapshot?.is_some(),
        "the sealed attempt must produce a snapshot"
    );

    let additional_findings = i64::try_from(MANY_FINDINGS - FEW_FINDINGS)?;
    let budget = small_statements + additional_findings * STATEMENTS_PER_ADDITIONAL_FINDING;
    assert!(
        large_statements <= budget,
        "a {MANY_FINDINGS}-finding snapshot executed {large_statements} statements against a \
         {FEW_FINDINGS}-finding baseline of {small_statements}, over the linear budget of \
         {budget}; the snapshot is scaling faster than its findings"
    );
    Ok(())
}

/// Complete stage seals reconstruct one coherent orchestration snapshot.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_store_reconstructs_complete_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let mut fixture = prepare_orchestration_fixture(&pool).await?;
    let accepted = fixture
        .workflow
        .append_finding_event(
            fixture.finding_ref.finding(),
            finding_event(
                fixture.finding_ref,
                ReviewEventOrdinal::one(),
                fixture.evidence[3].clone(),
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await?
        .expect("accepted finding exists");
    let fixed = fixture
        .workflow
        .append_finding_event(
            fixture.finding_ref.finding(),
            finding_event(
                fixture.finding_ref,
                ReviewEventOrdinal::try_new(2).expect("two is positive"),
                fixture.evidence[4].clone(),
                ReviewFindingEventKind::Fixed,
            ),
        )
        .await?
        .expect("fixed finding exists");
    assert_eq!(accepted.status(), ReviewFindingStatus::Accepted);
    let repair = ReviewRepairMemberOutcome::Fixed(Box::new(ReviewRepairSuccess::new(
        fixed.events()[1].clone(),
        ReviewTemplateDigest::new([3; 32]),
    )));
    let applied_effect = ReviewJudgmentEffectId::new(fixture.attempt_id, fixture.finding_ref);
    assert_eq!(
        fixture
            .store
            .record_applied_judgment_effect(applied_effect)
            .await?,
        ReviewDurableSealOutcome::Recorded
    );
    assert_eq!(
        fixture.store.current_stage(fixture.attempt_id).await?,
        Some(ReviewOrchestrationCurrentStage::AwaitingRepair)
    );
    assert_eq!(
        fixture
            .store
            .seal_repair_inventory(fixture.attempt_id, vec![fixture.finding_ref])
            .await?,
        ReviewDurableSealOutcome::Recorded
    );
    assert_eq!(
        fixture
            .store
            .record_repair_outcomes(fixture.attempt_id, vec![repair.clone()])
            .await?,
        ReviewDurableSealOutcome::Recorded
    );
    assert_eq!(
        fixture
            .store
            .seal_publication_inventory(fixture.attempt_id, Vec::new())
            .await?,
        ReviewDurableSealOutcome::Recorded
    );
    assert_eq!(
        fixture
            .store
            .record_publication_outcomes(fixture.attempt_id, Vec::new())
            .await?,
        ReviewDurableSealOutcome::Recorded
    );
    let snapshot = fixture
        .store
        .load_snapshot(fixture.attempt_id)
        .await?
        .expect("completed attempt has a coherent snapshot");

    assert_eq!(snapshot.attempt, fixture.attempt);
    assert_eq!(
        snapshot.current_stage,
        ReviewOrchestrationCurrentStage::Complete
    );
    assert_eq!(snapshot.concern_claims, vec![fixture.claim]);
    assert_eq!(snapshot.judgment_plan, Some(fixture.plan));
    assert_eq!(snapshot.applied_judgment_effects, vec![applied_effect]);
    assert_eq!(snapshot.repair_outcomes, Some(vec![repair]));
    assert_eq!(snapshot.publication_outcomes, Some(Vec::new()));
    Ok(())
}

/// Equal stage seals replay without changing immutable attempt facts.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_store_replays_equal_stage_seals() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let mut fixture = prepare_orchestration_fixture(&pool).await?;

    assert_eq!(
        fixture.store.record_attempt(fixture.attempt).await?,
        ReviewDurableSealOutcome::EqualReplay
    );
    assert_eq!(
        fixture
            .store
            .record_import(fixture.attempt_id, fixture.import)
            .await?,
        ReviewDurableSealOutcome::EqualReplay
    );
    assert_eq!(
        fixture
            .store
            .seal_complete_fanout(fixture.attempt_id, vec![fixture.claim])
            .await?,
        ReviewDurableSealOutcome::EqualReplay
    );
    assert_eq!(
        fixture
            .store
            .seal_judgment_plan(fixture.attempt_id, fixture.plan)
            .await?,
        ReviewDurableSealOutcome::EqualReplay
    );
    Ok(())
}

/// A recovery-only interrupted judgment result remains visible to current-stage
/// and coherent-snapshot reconstruction.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_recovery_preserves_interrupted_judgment_state()
-> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x7a9;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;
    let command = orchestration_command(&fixture, COMMAND_IDENTITY, [10; 32]);
    let result = incomplete_judgment_result(&fixture).await?;
    let guard = expect_new_orchestration_claim(fixture.store.begin_command(command).await?);
    assert_eq!(
        fixture
            .store
            .record_command_recovery(command, result.clone())
            .await?,
        result
    );
    drop(guard);

    assert_eq!(
        fixture.store.current_stage(fixture.attempt_id).await?,
        Some(ReviewOrchestrationCurrentStage::JudgmentIncomplete)
    );
    assert_eq!(
        fixture
            .store
            .load_snapshot(fixture.attempt_id)
            .await?
            .expect("interrupted attempt has a coherent snapshot")
            .current_stage,
        ReviewOrchestrationCurrentStage::JudgmentIncomplete
    );
    Ok(())
}

/// A recovery-only orchestration command reserves its user-global identity and
/// its exact retry materializes the typed receipt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_recovery_reserves_global_command_identity()
-> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x7aa;
    const FOREIGN_TARGET_IDENTITY: u128 = 0x7ab;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;
    let command = orchestration_command(&fixture, COMMAND_IDENTITY, [11; 32]);
    let result = incomplete_judgment_result(&fixture).await?;
    let guard = expect_new_orchestration_claim(fixture.store.begin_command(command).await?);
    assert_eq!(
        fixture
            .store
            .record_command_recovery(command, result.clone())
            .await?,
        result
    );
    drop(guard);
    let foreign_target = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(FOREIGN_TARGET_IDENTITY)),
        key("provider"),
        key("foreign-repository"),
        ReviewTargetSubject::Commit,
        key("foreign-head"),
        None,
        None,
    )
    .expect("foreign target fixture is valid");
    let foreign_command = ReviewWorkflowCommand::new(
        command.command_id,
        [12; 32],
        ReviewWorkflowOperation::CreateTarget(foreign_target.clone()),
    );
    let mut workflow_commands = ReviewWorkflowCommandService::new(fixture.workflow.clone());

    assert_eq!(
        workflow_commands.execute(foreign_command).await?,
        ReviewWorkflowCommandOutcome::ConflictingReuse {
            command_id: command.command_id
        }
    );
    assert_eq!(
        fixture.workflow.load_target(foreign_target.id()).await?,
        None
    );
    assert_eq!(
        expect_recorded_orchestration_claim(fixture.store.begin_command(command).await?),
        result
    );
    Ok(())
}

/// A pending orchestration intent immediately rejects a cross-kind registry row.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_intent_blocks_cross_kind_registry_insert()
-> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x7af;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;
    let command = orchestration_command(&fixture, COMMAND_IDENTITY, [18; 32]);
    let result = incomplete_judgment_result(&fixture).await?;
    let guard = expect_new_orchestration_claim(fixture.store.begin_command(command).await?);
    let mut transaction = pool.begin().await?;
    let error = sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'review_workflow', 1, transaction_timestamp(), 'operator')",
    )
    .bind(command.command_id.into_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("intent already reserves the user-global command identity");
    assert_sqlstate(&error, "23505");
    transaction.rollback().await?;
    drop(guard);

    assert_eq!(
        fixture
            .store
            .record_command_recovery(command, result.clone())
            .await?,
        result
    );
    assert_eq!(
        expect_recorded_orchestration_claim(fixture.store.begin_command(command).await?),
        result
    );
    Ok(())
}

/// A committed orchestration receipt replays exactly and conflicting semantic
/// reuse is rejected.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_command_receipt_replays_and_conflicts() -> Result<(), Box<dyn Error>>
{
    const COMMAND_IDENTITY: u128 = 0x7ac;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;
    let command = orchestration_command(&fixture, COMMAND_IDENTITY, [13; 32]);
    let result = incomplete_judgment_result(&fixture).await?;
    let guard = expect_new_orchestration_claim(fixture.store.begin_command(command).await?);
    assert_eq!(guard.record(result.clone()).await?, result);

    assert_eq!(
        expect_recorded_orchestration_claim(fixture.store.begin_command(command).await?),
        result
    );
    assert!(matches!(
        fixture
            .store
            .begin_command(ReviewOrchestrationCommand {
                semantic_digest: [14; 32],
                ..command
            })
            .await?,
        ReviewOrchestrationCommandClaim::Conflicting
    ));
    Ok(())
}

/// An equal concurrent orchestration claim observes the durable pending intent and replays.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_equal_command_claim_resumes_pending_and_replays()
-> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x7ad;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;
    let command = orchestration_command(&fixture, COMMAND_IDENTITY, [15; 32]);
    let result = incomplete_judgment_result(&fixture).await?;
    let winner = expect_new_orchestration_claim(fixture.store.begin_command(command).await?);
    let contender = expect_pending_orchestration_claim(fixture.store.begin_command(command).await?);
    assert_eq!(winner.record(result.clone()).await?, result);
    assert_eq!(contender.record(result.clone()).await?, result);
    Ok(())
}

/// A conflicting command immediately rejects against the durable pending intent.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_conflicting_command_rejects_pending_intent()
-> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x7ae;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;
    let command = orchestration_command(&fixture, COMMAND_IDENTITY, [16; 32]);
    let result = incomplete_judgment_result(&fixture).await?;
    let winner = expect_new_orchestration_claim(fixture.store.begin_command(command).await?);
    let conflicting = ReviewOrchestrationCommand {
        semantic_digest: [17; 32],
        ..command
    };
    assert!(matches!(
        fixture.store.begin_command(conflicting).await?,
        ReviewOrchestrationCommandClaim::Conflicting
    ));
    assert_eq!(winner.record(result).await?.attempt, fixture.attempt_id);
    Ok(())
}

/// An existing attempt identity rejects different immutable frozen input.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_attempt_rejects_conflicting_frozen_input()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let mut fixture = prepare_orchestration_fixture(&pool).await?;
    let conflicting_attempt = ReviewOrchestrationAttempt::try_new(
        fixture.attempt_id,
        fixture.attempt.target(),
        ReviewPolicy::version_one(),
        key("different-version"),
        fixture.attempt.stage_templates(),
        fixture.attempt.concerns().to_vec(),
    )?;

    assert_eq!(
        fixture.store.record_attempt(conflicting_attempt).await?,
        ReviewDurableSealOutcome::Conflict
    );
    Ok(())
}

/// Concurrent coherent snapshots stay within a configured two-connection pool
/// and the database's matching hard connection limit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_snapshots_respect_configured_connection_capacity()
-> Result<(), Box<dyn Error>> {
    const ATTEMPT_IDENTITY: u128 = 0x7b0;

    let (_container, pool) = migrated_postgres_with_max_connections(2).await?;
    sqlx::query("ALTER ROLE signalbox CONNECTION LIMIT 2")
        .execute(&pool)
        .await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let attempt_id = ReviewOrchestrationAttemptId::from_uuid(uuid(ATTEMPT_IDENTITY));
    let attempt = ReviewOrchestrationAttempt::try_new(
        attempt_id,
        fixture.target,
        ReviewPolicy::version_one(),
        key("capacity-proof-v1"),
        ReviewStageTemplateDigests::new(
            ReviewTemplateDigest::new([11; 32]),
            ReviewTemplateDigest::new([12; 32]),
            ReviewTemplateDigest::new([13; 32]),
            ReviewTemplateDigest::new([14; 32]),
        ),
        vec![ReviewConcernSpec::new(
            key("correctness"),
            ReviewTemplateDigest::new([15; 32]),
        )],
    )?;
    let mut store = PostgresReviewOrchestrationStore::new(pool);
    assert_eq!(
        store.record_attempt(attempt.clone()).await?,
        ReviewDurableSealOutcome::Recorded
    );
    let first_store = store.clone();
    let second_store = store;
    let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(10), async move {
        tokio::join!(
            first_store.load_snapshot(attempt_id),
            second_store.load_snapshot(attempt_id)
        )
    })
    .await
    .expect("two admitted snapshots finish within configured capacity");
    let expected_stage = ReviewOrchestrationCurrentStage::AwaitingImport;
    assert_eq!(first?.expect("first snapshot exists").attempt, attempt);
    assert_eq!(
        second?.expect("second snapshot exists").current_stage,
        expected_stage
    );
    Ok(())
}
