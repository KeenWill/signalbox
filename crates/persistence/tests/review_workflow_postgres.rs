#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

mod support;

use std::error::Error;

use signalbox_application::{
    StartEligibleTurnIdGenerator, StartEligibleTurnOutcome, StartEligibleTurnService,
};
use signalbox_domain::{
    AcceptedInputId, CancelledModelCallTurnIdentities, ContextFrontierId, CreateSession,
    DeliveryRequest, DirectModelSelection, DurableCommandId, ModelSelectionOverride,
    ModelSelectionRequest, PerInputConfigurationChoices, ReviewChangeRequestNumber,
    ReviewConfidence, ReviewEventOrdinal, ReviewExternalLink, ReviewExternalLinkAssociation,
    ReviewExternalLinkAttachment, ReviewExternalLinkAttachmentResult, ReviewExternalLinkId,
    ReviewExternalLinkNoChangeResult, ReviewExternalLinkObservation,
    ReviewExternalLinkObservationResult, ReviewExternalLinkPublicationBlockedResult,
    ReviewExternalLinkTransitionFailure, ReviewExternalObjectKind, ReviewExternalObjectState,
    ReviewFinding, ReviewFindingContent, ReviewFindingDiffSide, ReviewFindingEvent,
    ReviewFindingEventKind, ReviewFindingEventResult, ReviewFindingEventResultKind,
    ReviewFindingId, ReviewFindingLocation, ReviewFindingPendingExternalLinkRef,
    ReviewFindingProposal, ReviewFindingRef, ReviewFindingSeverity, ReviewFindingStatus,
    ReviewFindingTransitionFailure, ReviewKey, ReviewLineRange, ReviewPass,
    ReviewPassAcceptedInputEvidence, ReviewPassEvidence, ReviewPassId, ReviewPassKind,
    ReviewPassRef, ReviewPassResult, ReviewPassState, ReviewPassTurnEvidence,
    ReviewPassTurnOutcome, ReviewPolicy, ReviewProducedFindings, ReviewReferencedFindingEvidence,
    ReviewRun, ReviewRunEvidence, ReviewRunId, ReviewRunRef, ReviewRunState, ReviewTarget,
    ReviewTargetId, ReviewTargetSubject, ReviewText, ReviewWorkflowKind, SemanticTranscriptEntryId,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionCreationCause,
    SessionCreationProvenance, SessionId, SubmitInput, TranscriptAncestry, TurnAttemptId, TurnId,
    UserContent,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    local_test_connection_options, migrate,
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

use support::blocked_backends_reached;

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_review_workflow_integration";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

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
        .with_fsync_enabled()
        .with_tag(POSTGRES_IMAGE_TAG)
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
    if kind == ReviewPassKind::ReadOnlyReview
        && matches!(&state, ReviewPassState::Succeeded { result: None, .. })
    {
        let mut run =
            ReviewRun::try_reconstitute(signalbox_domain::ReviewRunReconstitutionInput::new(
                reference.run(),
                workflow_for_pass(kind),
                policy,
                ReviewRunState::Queued,
                None,
            ))
            .expect("fixture run is queued");
        let pass = ReviewPass::try_new(
            reference,
            kind,
            &mut run,
            session,
            ReviewPassAcceptedInputEvidence::new(accepted_input, session, Some(origin_turn)),
        )
        .expect("fixture pass is queued")
        .transition(
            ReviewPassState::Running { turn: origin_turn },
            Some(ReviewPassTurnEvidence::new(
                origin_turn,
                session,
                accepted_input,
                ReviewPassTurnOutcome::Active,
                None,
            )),
        )
        .expect("fixture pass starts")
        .transition(state, turn_evidence)
        .expect("fixture pass reaches its transient terminal state");
        return ReviewPassEvidence::from_pass(&pass, policy);
    }
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
        SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::None,
        ),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(uuid(0x102 + offset)),
        )),
    )
    .prepare(session)
    .expect("owner-created fixture session is preparable");
    CreateSessionRepository::new(pool.clone())
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
}

fn finding(
    reference: ReviewFindingRef,
    producing_pass: ReviewPassEvidence,
    target: &ReviewTarget,
) -> ReviewFinding {
    finding_with_confidence_and_side(
        reference,
        producing_pass,
        target,
        9_000,
        Some(ReviewFindingDiffSide::Right),
    )
}

fn finding_with_confidence(
    reference: ReviewFindingRef,
    producing_pass: ReviewPassEvidence,
    target: &ReviewTarget,
    confidence: u16,
) -> ReviewFinding {
    finding_with_confidence_and_side(
        reference,
        producing_pass,
        target,
        confidence,
        Some(ReviewFindingDiffSide::Right),
    )
}

fn finding_with_side(
    reference: ReviewFindingRef,
    producing_pass: ReviewPassEvidence,
    target: &ReviewTarget,
    diff_side: Option<ReviewFindingDiffSide>,
) -> ReviewFinding {
    finding_with_confidence_and_side(reference, producing_pass, target, 9_000, diff_side)
}

fn finding_with_confidence_and_side(
    reference: ReviewFindingRef,
    producing_pass: ReviewPassEvidence,
    target: &ReviewTarget,
    confidence: u16,
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
                Some(result @ ReviewPassResult::ProducedFindings(_)) => Some(result.clone()),
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
                ReviewConfidence::try_from_basis_points(confidence)
                    .expect("fixture confidence is bounded"),
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

async fn synthetically_terminalize_turn(
    pool: &PgPool,
    turn: TurnId,
    disposition: &str,
) -> ContextFrontierId {
    let frontier: Uuid = sqlx::query_scalar(
        "SELECT starting_frontier_id
           FROM turn_lifecycle
          WHERE turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_one(pool)
    .await
    .expect("fixture turn has a starting frontier");
    sqlx::query(
        "ALTER TABLE turn_lifecycle
         DROP CONSTRAINT IF EXISTS turn_lifecycle_state_payload_shape",
    )
    .execute(pool)
    .await
    .expect("focused fixture may relax the unrelated terminal payload check");
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER USER")
        .execute(pool)
        .await
        .expect("focused fixture may suspend unrelated session triggers");
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                terminal_frontier_id = starting_frontier_id,
                active_phase_kind = NULL,
                current_attempt_id = NULL,
                terminal_disposition_kind = $2,
                recovery_model_call_id = NULL,
                terminal_attempt_id = NULL,
                terminal_model_call_id = NULL
          WHERE turn_id = $1",
    )
    .bind(turn.into_uuid())
    .bind(disposition)
    .execute(pool)
    .await
    .expect("focused fixture projects the canonical terminal turn outcome");
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER USER")
        .execute(pool)
        .await
        .expect("session triggers are restored after fixture projection");
    ContextFrontierId::from_uuid(frontier)
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

async fn succeed_fixture_passes(
    pool: &PgPool,
    store: &ReviewWorkflowStore,
    references: &[ReviewPassRef],
) -> Vec<ReviewPassEvidence> {
    let mut terminal = Vec::with_capacity(references.len());
    for reference in references {
        let (_, turn) = start_review_pass(store, *reference).await;
        let output_frontier = synthetically_terminalize_turn(pool, turn, "completed").await;
        terminal.push((*reference, turn, output_frontier));
    }
    let mut evidence = Vec::with_capacity(terminal.len());
    for (reference, turn, output_frontier) in terminal {
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
    evidence
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

/// INV-040 / INV-041: the store reconstructs complete workflow evidence,
/// including the canonical reservation, attachment, and observation sequence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_inv041_review_workflow_store_reconstructs_complete_evidence()
-> Result<(), Box<dyn Error>> {
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
    start_review_pass(&store, pass_ref).await;
    start_review_pass(&store, judge_pass).await;
    start_review_pass(&store, publish_pass).await;
    start_review_pass(&store, import_pass).await;
    start_review_pass(&store, unchanged_import_pass).await;
    let output_frontier = synthetically_terminalize_turn(&pool, turn, "completed").await;
    let judge_output_frontier =
        synthetically_terminalize_turn(&pool, judge_turn, "completed").await;
    let publish_output_frontier =
        synthetically_terminalize_turn(&pool, publish_turn, "completed").await;
    let import_output_frontier =
        synthetically_terminalize_turn(&pool, import_turn, "completed").await;
    let unchanged_import_output_frontier =
        synthetically_terminalize_turn(&pool, unchanged_import_turn, "completed").await;
    let review_evidence = conclude_review_pass(
        &store,
        pass_ref,
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            result: None,
        },
    )
    .await;
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
    let later_output_frontier =
        synthetically_terminalize_turn(&pool, later_import_turn, "completed").await;
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

/// INV-040: pass loading validates the accepted input's canonical session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_pass_loader_rejects_cross_wired_accepted_input() -> Result<(), Box<dyn Error>> {
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

/// INV-040: pass loading authenticates the queued pass's exact origin turn,
/// independently of its accepted-input snapshot.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_pass_loader_rejects_missing_origin_turn() -> Result<(), Box<dyn Error>> {
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

/// INV-040: a pass whose canonical target row is missing is corruption, even
/// when its run row remains present.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_pass_loader_rejects_missing_target() -> Result<(), Box<dyn Error>> {
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

/// INV-040: an accepted orchestration input is owned by at most one review
/// pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_accepted_input_owns_at_most_one_review_pass() -> Result<(), Box<dyn Error>> {
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

/// INV-040: pass loading validates the referenced turn's canonical session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_pass_loader_rejects_cross_wired_turn() -> Result<(), Box<dyn Error>> {
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

/// INV-040: a run projection may report only the canonical outcome of its
/// referenced pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_run_projection_rejects_noncanonical_pass_outcome() -> Result<(), Box<dyn Error>> {
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

/// INV-040: loading a pass validates the canonical state projection of its run.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_pass_loader_rejects_noncanonical_run_projection() -> Result<(), Box<dyn Error>> {
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

/// INV-040: a pass projection may report only the canonical outcome of its
/// referenced turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_pass_projection_rejects_noncanonical_turn_outcome() -> Result<(), Box<dyn Error>> {
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

/// INV-040: pass failure is the workflow-operation outcome and may follow a
/// canonically completed turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_failed_pass_accepts_completed_turn_evidence() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let (_, turn) = start_review_pass(&fixture.store, fixture.pass).await;
    synthetically_terminalize_turn(&pool, turn, "completed").await;

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

/// INV-040: lifecycle-only transition APIs reject an effect result before
/// changing the pass or run projection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_generic_transition_rejects_effect_result() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let (_, turn) = start_review_pass(&fixture.store, fixture.pass).await;
    let output_frontier = synthetically_terminalize_turn(&pool, turn, "completed").await;
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

/// INV-040: a completed read-only pass may atomically bind an exact empty
/// produced-finding inventory.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_empty_finding_inventory_binds_exact_result() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let succeeded = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass]).await[0].clone();
    let no_finding_references = Vec::new();
    let evidence = pass_with_produced_findings(no_finding_references, succeeded);
    let no_findings = Vec::<ReviewFinding>::new();

    fixture
        .store
        .insert_findings(&evidence, &no_findings)
        .await?;
    assert_eq!(
        fixture
            .store
            .load_pass(fixture.pass.pass())
            .await?
            .expect("pass with empty result inventory loads")
            .state(),
        evidence.state()
    );
    Ok(())
}

/// INV-040: once a produced-finding inventory is sealed, later canonical
/// finding inserts cannot expand the result—even when the inventory was empty.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_sealed_finding_inventory_cannot_expand() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let succeeded = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass]).await[0].clone();
    let evidence = pass_with_produced_findings(Vec::new(), succeeded);
    fixture.store.insert_findings(&evidence, &[]).await?;

    let expansion = sqlx::query(
        "INSERT INTO review_finding
            (finding_id, run_id, target_id, producing_pass_id, file_path,
             line_start, line_end, diff_side, title, body, severity,
             confidence, category, recommended_fix)
         VALUES (
             $1, $2, $3, $4, 'src/lib.rs',
             1, 1, 'right', 'Late finding', 'Body', 'high',
             9000, 'correctness', NULL
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

/// INV-040: pass loading authenticates both directions of the sealed
/// produced-finding inventory against canonical finding rows.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_pass_loader_rejects_incomplete_finding_inventory() -> Result<(), Box<dyn Error>> {
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

/// INV-040: finding-event validation reuses its held transaction connection,
/// so a one-connection pool cannot self-deadlock while loading current history.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_finding_event_uses_held_transaction_connection() -> Result<(), Box<dyn Error>> {
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

/// INV-040: appending an event through another same-run finding fails before
/// persistence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_finding_event_rejects_foreign_owner() -> Result<(), Box<dyn Error>> {
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

/// INV-040: a referenced finding reconstitutes with its exact producing pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_referenced_finding_retains_producing_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let dedupe_pass = insert_fixture_pass(&fixture, 0x333, ReviewPassKind::Dedupe).await;
    let evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, dedupe_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x330)));
    let canonical_ref =
        ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x331)));
    let review_evidence =
        pass_with_produced_findings(vec![finding_ref, canonical_ref], evidence[0].clone());
    let dedupe_evidence = evidence[1].clone();
    let open = finding(
        finding_ref,
        review_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_findings(
            &review_evidence,
            &[
                open.clone(),
                finding(
                    canonical_ref,
                    review_evidence.clone(),
                    &fixture.target_snapshot,
                ),
            ],
        )
        .await?;
    let event = finding_event(
        finding_ref,
        ReviewEventOrdinal::one(),
        dedupe_evidence,
        ReviewFindingEventKind::Duplicate {
            canonical: ReviewReferencedFindingEvidence::try_reconstitute(
                canonical_ref,
                ReviewFindingStatus::Open,
            )
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

    assert_eq!(
        fixture.store.load_finding(finding_ref.finding()).await?,
        Some(expected)
    );
    Ok(())
}

/// INV-040: duplicate/superseded references cannot close a cycle by
/// referencing a finding whose current status is already terminal.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_finding_references_reject_cycles() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let first_dedupe = insert_fixture_pass(&fixture, 0x336, ReviewPassKind::Dedupe).await;
    let second_dedupe = insert_fixture_pass(&fixture, 0x337, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, first_dedupe, second_dedupe],
    )
    .await;
    let first = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x338)));
    let second = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x339)));
    let review_evidence = pass_with_produced_findings(vec![first, second], evidence[0].clone());
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
    fixture
        .store
        .append_finding_event(
            first.finding(),
            finding_event(
                first,
                ReviewEventOrdinal::one(),
                evidence[1].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_reconstitute(
                        second,
                        ReviewFindingStatus::Open,
                    )
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
                evidence[2].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_reconstitute(
                        first,
                        ReviewFindingStatus::Open,
                    )
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

/// INV-040: a referenced finding's missing canonical producer is corruption,
/// even when the aggregate finding's own producer remains intact.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_finding_load_rejects_missing_referenced_producer() -> Result<(), Box<dyn Error>> {
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
    fixture
        .store
        .insert_findings(
            &review_evidence,
            &[
                finding(
                    finding_ref,
                    review_evidence.clone(),
                    &fixture.target_snapshot,
                ),
                finding(
                    canonical_ref,
                    review_evidence.clone(),
                    &fixture.target_snapshot,
                ),
            ],
        )
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
                    canonical: ReviewReferencedFindingEvidence::try_reconstitute(
                        canonical_ref,
                        ReviewFindingStatus::Open,
                    )
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
         DROP CONSTRAINT review_pass_result_referenced_finding_fk",
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

    let error = fixture
        .store
        .load_finding(finding_ref.finding())
        .await
        .expect_err("missing referenced producer must fail closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed produced-finding inventory corruption");
    };
    assert_eq!(error.aggregate(), "review_pass_produced_finding");
    assert!(error.detail().contains("no canonical finding"));
    Ok(())
}

/// INV-040: direct SQL cannot admit a judgment below the finding producer's
/// frozen minimum confidence threshold.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_schema_enforces_judge_confidence_threshold() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x33e, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x33f)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    fixture
        .store
        .insert_finding(&finding_with_confidence(
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

/// INV-040 / INV-041: direct SQL cannot publish a finding below the producer's
/// frozen publication threshold, even with matching attachment evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_inv041_schema_enforces_publication_confidence_threshold()
-> Result<(), Box<dyn Error>> {
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
        .insert_finding(&finding_with_confidence(
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

/// INV-040: event compatibility is checked against the canonical persisted
/// pass kind, not only the kind carried by the in-memory event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_canonical_pass_kind_rejects_misclassified_event() -> Result<(), Box<dyn Error>> {
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

/// INV-040: the event row must exactly match the finding result committed by
/// its terminal pass, including ordinal and event type.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_finding_event_requires_exact_pass_result() -> Result<(), Box<dyn Error>> {
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

/// INV-040: an effect result cannot be changed after its first atomic binding.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_bound_pass_result_is_immutable() -> Result<(), Box<dyn Error>> {
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

/// INV-040: persistence rejects review policy versions that the domain cannot
/// reconstitute.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_schema_rejects_unsupported_policy_version() -> Result<(), Box<dyn Error>> {
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

/// INV-041: an attachment carried through another same-target reservation
/// fails before persistence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_external_attachment_rejects_foreign_owner() -> Result<(), Box<dyn Error>> {
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

/// INV-040: appending an observation through another same-target external link
/// fails before persistence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_external_observation_rejects_foreign_owner() -> Result<(), Box<dyn Error>> {
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

/// INV-040: file-relative findings admit no diff side, while a diff-relative
/// location requires a canonical target base.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_finding_diff_side_requires_target_base() -> Result<(), Box<dyn Error>> {
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
             confidence, category, recommended_fix)
         VALUES (
             $1, $2, $3, $4, 'src/lib.rs',
             1, 1, 'right', 'Finding', 'Body', 'high',
             9000, 'correctness', NULL
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

/// INV-040: the store refuses to insert a run projection after transition.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_run_insert_requires_queued_state() -> Result<(), Box<dyn Error>> {
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

/// INV-040: the store refuses to insert a pass projection after transition.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_pass_insert_requires_queued_state() -> Result<(), Box<dyn Error>> {
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

/// INV-040: the store refuses to insert a finding carrying event history.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_finding_insert_requires_open_state() -> Result<(), Box<dyn Error>> {
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

/// INV-041: reservation insertion refuses post-effect attachment evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_reservation_insert_requires_pending_state() -> Result<(), Box<dyn Error>> {
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

/// INV-041: a blocked publication pass is consumed by the exact pending
/// reservation and its nonempty reconciliation reason.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_blocked_publication_binds_pending_reservation() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let publish_pass = insert_fixture_pass(&fixture, 0x30a, ReviewPassKind::Publish).await;
    let (_, turn) = start_review_pass(&fixture.store, publish_pass).await;
    synthetically_terminalize_turn(&pool, turn, "reconciliation_required").await;
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

/// INV-041: a finding-associated reservation authenticates the finding's exact
/// canonical producing pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_reservation_rejects_forged_finding_producer() -> Result<(), Box<dyn Error>> {
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

/// INV-041: raw reservation inserts cannot diverge from the canonical target
/// provider.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_reservation_requires_canonical_target_provider() -> Result<(), Box<dyn Error>> {
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

/// INV-041: the identity registry is derivable attachment evidence, not an
/// independently writable claim.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_external_identity_requires_establishing_attachment() -> Result<(), Box<dyn Error>> {
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

/// INV-040 / INV-041: the canonical pass/finding and external-claim lookup
/// paths remain indexed by their leading filter columns.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_inv041_review_lookup_indexes_are_pinned() -> Result<(), Box<dyn Error>> {
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
    Ok(())
}

/// INV-040: raw run rows must begin queued.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_schema_requires_new_run_to_be_queued() -> Result<(), Box<dyn Error>> {
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

/// INV-040: raw pass rows must begin queued.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_schema_requires_new_pass_to_be_queued() -> Result<(), Box<dyn Error>> {
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

/// S29 / INV-040: change-request targets require a frozen comparison
/// revision.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s29_inv040_schema_requires_change_request_base() -> Result<(), Box<dyn Error>> {
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

/// INV-040: policy version one has one canonical threshold tuple.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_schema_requires_canonical_version_one_policy() -> Result<(), Box<dyn Error>> {
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

/// INV-040: finding line ranges are absent or complete.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_schema_rejects_half_populated_line_range() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let half_populated_range = sqlx::query(
        "INSERT INTO review_finding
            (finding_id, run_id, target_id, producing_pass_id, file_path,
             line_start, line_end, diff_side, title, body, severity,
             confidence, category, recommended_fix)
         VALUES (
             $1, $2, $3, $4, 'src/lib.rs',
             1, NULL, 'right', 'Finding', 'Body', 'high',
             9000, 'correctness', NULL
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

/// INV-040: rejected finding events require their exact reason.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_schema_requires_rejection_reason() -> Result<(), Box<dyn Error>> {
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

/// INV-041: a posted event requires attached external review content.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_schema_authenticates_posted_external_review_content() -> Result<(), Box<dyn Error>>
{
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

/// INV-040: cancelling a running run cannot erase its active pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_schema_running_run_cancellation_retains_pass() -> Result<(), Box<dyn Error>> {
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

/// INV-040: loading a queued run retains its already-recorded pass, so the
/// store rejects a passless cancellation before issuing an update.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_queued_run_cannot_discard_recorded_pass() -> Result<(), Box<dyn Error>> {
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

/// INV-040: cancelling a running pass cannot erase its active turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_schema_running_pass_cancellation_retains_turn() -> Result<(), Box<dyn Error>> {
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

/// INV-041: a multi-row external-link load observes one database snapshot
/// while a concurrent attachment and observation commit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_external_link_load_is_one_repeatable_snapshot() -> Result<(), Box<dyn Error>> {
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

/// INV-040: finding-event serialization remains compatible with the key-share
/// lock PostgreSQL takes while checking a foreign finding reference.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_finding_event_serialization_is_fk_compatible() -> Result<(), Box<dyn Error>> {
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

/// INV-041: observation ordinal serialization remains compatible with the
/// key-share lock used by external-link foreign-key checks.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_external_observation_serialization_is_fk_compatible() -> Result<(), Box<dyn Error>>
{
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

/// INV-040: PostgreSQL rejects an event history that does not begin at one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_gapped_finding_history_is_rejected() -> Result<(), Box<dyn Error>> {
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

/// INV-040: PostgreSQL rejects a producing pass from another target/run edge.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_cross_wired_pass_ancestry_is_rejected() -> Result<(), Box<dyn Error>> {
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
             confidence, category, recommended_fix)
         VALUES (
             $1, $2, $3, $4, 'src/lib.rs',
             1, 1, 'right', 'Finding', 'Body', 'high',
             9000, 'correctness', NULL
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

/// INV-040: a missing immutable finding producer is corruption, not absence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_finding_load_rejects_missing_producing_pass() -> Result<(), Box<dyn Error>> {
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

/// INV-040: a finding-associated external link cannot load when its finding's
/// canonical producing pass is missing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_external_link_load_rejects_missing_finding_producer() -> Result<(), Box<dyn Error>>
{
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

/// INV-041: a missing attachment-pass run is corruption, not absence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_external_link_load_rejects_missing_attachment_run() -> Result<(), Box<dyn Error>> {
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

/// INV-041: a missing observation-pass run is corruption, not a shortened
/// observation history.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_external_link_load_rejects_missing_observation_run() -> Result<(), Box<dyn Error>> {
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

/// INV-040: a missing finding-event pass is corruption, not a silently
/// shortened history.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_finding_load_rejects_missing_event_pass() -> Result<(), Box<dyn Error>> {
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

/// INV-041: one provider/kind/object identity has at most one attachment per
/// frozen target, cannot move to an unrelated logical target, and may follow
/// one change request across refreshed snapshots.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_external_object_attachment_is_unique() -> Result<(), Box<dyn Error>> {
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
    let refreshed_frontier =
        synthetically_terminalize_turn(&pool, refreshed_turn, "completed").await;
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
    let later_frontier = synthetically_terminalize_turn(&pool, later_turn, "completed").await;
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

/// INV-041: concurrent first attachments serialize on canonical object identity,
/// so unrelated targets cannot both establish ownership.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_concurrent_external_object_attachment_has_one_logical_target()
-> Result<(), Box<dyn Error>> {
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
    let first_frontier = synthetically_terminalize_turn(&pool, first_turn, "completed").await;
    let second_frontier = synthetically_terminalize_turn(&pool, second_turn, "completed").await;
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

/// INV-040: target loading reconstructs the complete stack ancestry, and the
/// schema rejects a logical change request repeated anywhere in that chain.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_target_stack_ancestry_is_complete_and_nonrepeating() -> Result<(), Box<dyn Error>> {
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

/// INV-040: stack parents are confined to the target's provider and
/// repository.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_stack_parent_requires_same_repository() -> Result<(), Box<dyn Error>> {
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

/// INV-040: a stack edge joins the child base to the canonical parent head.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_stack_parent_requires_canonical_revision() -> Result<(), Box<dyn Error>> {
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

/// INV-040: a pass kind is the exact one-to-one projection of its run
/// workflow.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_pass_kind_requires_matching_run_workflow() -> Result<(), Box<dyn Error>> {
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

/// INV-040: one run owns at most one pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_run_rejects_second_pass() -> Result<(), Box<dyn Error>> {
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

/// INV-040: changing only a pass cannot commit an unprojected run/pass state.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_unprojected_pass_transition_is_rejected() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let error = fixture
        .store
        .transition_pass(
            fixture.pass.pass(),
            ReviewPassState::Running {
                turn: TurnId::from_uuid(uuid(0x203)),
            },
        )
        .await
        .expect_err("pass-only activation cannot commit");
    let ReviewWorkflowStoreError::Database(error) = error else {
        panic!("projection guard must be a database rejection");
    };
    assert_sqlstate(&error, "23514");
    Ok(())
}

/// INV-040: pre-start cancellation updates the run and pass atomically.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_queued_run_and_pass_cancel_together() -> Result<(), Box<dyn Error>> {
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

/// INV-040: a running pass may load while its canonical turn has reached a
/// terminal outcome not yet projected into the pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_running_pass_admits_monotonic_terminal_turn_lag() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let turn = TurnId::from_uuid(uuid(0x203));
    start_review_pass(&fixture.store, fixture.pass).await;
    synthetically_terminalize_turn(&pool, turn, "completed").await;
    let loaded = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await?
        .expect("running pass still loads after its turn concludes");
    assert_eq!(loaded.state(), &ReviewPassState::Running { turn });
    Ok(())
}

/// INV-040: finding insertion authenticates a succeeded read-only-review
/// producer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_finding_rejects_queued_producer() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let unauthorized = sqlx::query(
        "INSERT INTO review_finding
            (finding_id, run_id, target_id, producing_pass_id, file_path,
             line_start, line_end, diff_side, title, body, severity,
             confidence, category, recommended_fix)
         VALUES (
             $1, $2, $3, $4, 'src/lib.rs',
             NULL, NULL, NULL, 'Finding', 'Body', 'high',
             9000, 'correctness', NULL
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

/// INV-040: a finding event cannot claim a failed disposition pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_finding_event_rejects_failed_pass() -> Result<(), Box<dyn Error>> {
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
    start_review_pass(&fixture.store, fixture.pass).await;
    start_review_pass(&fixture.store, judge_pass).await;
    let output_frontier = synthetically_terminalize_turn(&pool, turn, "completed").await;
    synthetically_terminalize_turn(&pool, other_turn, "failed").await;
    let review_evidence = conclude_review_pass(
        &fixture.store,
        fixture.pass,
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            result: None,
        },
    )
    .await;
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

/// INV-041: attachment insertion rejects an otherwise canonical pass of the
/// wrong kind.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_attachment_rejects_read_only_review_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass]).await;
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

/// INV-040 / INV-041: terminal pass effects require their exact finding-event,
/// attachment, or observation child row in the same transaction.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_inv041_pass_results_require_exact_child_rows() -> Result<(), Box<dyn Error>> {
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

/// INV-041: observation insertion authenticates a succeeded import pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_observation_rejects_queued_import_pass() -> Result<(), Box<dyn Error>> {
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

/// INV-040 / INV-041: a linked publication block and a non-posting attachment
/// serialize on their shared reservation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_inv041_linked_block_serializes_with_non_posting_attachment()
-> Result<(), Box<dyn Error>> {
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
    synthetically_terminalize_turn(&pool, blocked_turn, "reconciliation_required").await;
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

/// INV-041: attachment returns the canonical aggregate reloaded under the
/// reservation lock, including a publication claim that won the lock first.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_attachment_returns_claim_committed_while_waiting() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let attaching_pass =
        insert_fixture_pass(&fixture, 0x7c0, ReviewPassKind::ImportExternalContext).await;
    let blocked_pass = insert_fixture_pass(&fixture, 0x7c1, ReviewPassKind::Publish).await;
    let attaching_evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[attaching_pass]).await[0].clone();
    let (_, blocked_turn) = start_review_pass(&fixture.store, blocked_pass).await;
    synthetically_terminalize_turn(&pool, blocked_turn, "reconciliation_required").await;

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

/// INV-040 / INV-041: a publication-blocked finding reconciles only through
/// the succeeded pass that produced the attached object.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_inv041_blocked_publication_reconciles_with_attachment_pass()
-> Result<(), Box<dyn Error>> {
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

    let (_, turn) = start_review_pass(&fixture.store, fixture.pass).await;
    let (_, judge_turn) = start_review_pass(&fixture.store, judge_pass).await;
    let (_, attaching_turn) = start_review_pass(&fixture.store, attaching_pass).await;
    let (_, other_publish_turn) = start_review_pass(&fixture.store, other_publish_pass).await;
    start_review_pass(&fixture.store, blocked_publish_pass).await;
    let output_frontier = synthetically_terminalize_turn(&pool, turn, "completed").await;
    let judge_output_frontier =
        synthetically_terminalize_turn(&pool, judge_turn, "completed").await;
    let attaching_output_frontier =
        synthetically_terminalize_turn(&pool, attaching_turn, "completed").await;
    let other_publish_output_frontier =
        synthetically_terminalize_turn(&pool, other_publish_turn, "completed").await;
    synthetically_terminalize_turn(&pool, blocked_turn, "reconciliation_required").await;

    let review_evidence = conclude_review_pass(
        &fixture.store,
        fixture.pass,
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            result: None,
        },
    )
    .await;
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

/// INV-040: a frozen review target rejects in-place revision mutation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_target_evidence_is_append_only() -> Result<(), Box<dyn Error>> {
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

/// INV-040 / INV-041: append-only workflow evidence also rejects statement-
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
async fn inv040_inv041_review_workflow_tables_reject_truncate() -> Result<(), Box<dyn Error>> {
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
        "review_external_link_observation",
        "TRUNCATE TABLE review_external_link_observation CASCADE",
    )
    .await;
    Ok(())
}

/// INV-040: maximum-size target keys remain persistable without a wide-index
/// size failure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_maximum_target_keys_do_not_overflow_indexes() -> Result<(), Box<dyn Error>> {
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
