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
    ReviewExternalLinkAttachment, ReviewExternalLinkId, ReviewExternalLinkObservation,
    ReviewExternalLinkTransitionError, ReviewExternalObjectKind, ReviewExternalObjectState,
    ReviewFinding, ReviewFindingContent, ReviewFindingDiffSide, ReviewFindingEvent,
    ReviewFindingEventKind, ReviewFindingId, ReviewFindingLocation, ReviewFindingProposal,
    ReviewFindingRef, ReviewFindingSeverity, ReviewFindingStatus, ReviewFindingTransitionFailure,
    ReviewKey, ReviewLineRange, ReviewPass, ReviewPassEvidence, ReviewPassId, ReviewPassKind,
    ReviewPassRef, ReviewPassState, ReviewPassTurnEvidence, ReviewPassTurnOutcome, ReviewPolicy,
    ReviewPolicyVersion, ReviewRun, ReviewRunId, ReviewRunRef, ReviewRunState, ReviewTarget,
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
        .max_connections(4)
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

fn succeeded_pass(reference: ReviewPassRef, kind: ReviewPassKind) -> ReviewPassEvidence {
    ReviewPassEvidence::new(
        reference,
        kind,
        ReviewPolicy::version_one(),
        ReviewPassState::Succeeded {
            turn: TurnId::from_uuid(uuid(0x203)),
            output_frontier: ContextFrontierId::from_uuid(uuid(0x131)),
        },
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
    finding_with_side(
        reference,
        producing_pass,
        target,
        Some(ReviewFindingDiffSide::Right),
    )
}

fn finding_with_side(
    reference: ReviewFindingRef,
    producing_pass: ReviewPassEvidence,
    target: &ReviewTarget,
    diff_side: Option<ReviewFindingDiffSide>,
) -> ReviewFinding {
    ReviewFinding::new(
        ReviewFindingProposal::try_new(
            reference,
            producing_pass,
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
                ReviewConfidence::try_from_basis_points(9_000)
                    .expect("fixture confidence is bounded"),
                key("correctness"),
                Some(text("Bind the transition to the complete pass reference.")),
            ),
        )
        .expect("fixture pass belongs to the finding run"),
    )
}

struct PersistedReviewPassFixture {
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
    store
        .insert_run(&ReviewRun::new(
            run,
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        ))
        .await
        .expect("queued run persists");
    store
        .insert_pass(
            &ReviewPass::try_new(
                pass,
                ReviewPassKind::ReadOnlyReview,
                ReviewWorkflowKind::ReadOnlyReview,
                session,
                accepted_input,
                session,
            )
            .expect("accepted input belongs to the fixture session"),
        )
        .await
        .expect("queued pass persists");

    PersistedReviewPassFixture {
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
    insert_pass_for_target(
        &fixture.store,
        fixture.target,
        identity,
        kind,
        SessionId::from_uuid(uuid(0x201)),
        AcceptedInputId::from_uuid(uuid(0x202)),
    )
    .await
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
    store
        .insert_run(&ReviewRun::new(run, workflow_for_pass(kind), policy))
        .await
        .expect("additional fixture run persists");
    store
        .insert_pass(
            &ReviewPass::try_new(
                pass,
                kind,
                workflow_for_pass(kind),
                session,
                accepted_input,
                session,
            )
            .expect("fixture input belongs to its session"),
        )
        .await
        .expect("additional fixture pass persists");
    pass
}

async fn start_review_pass(
    store: &ReviewWorkflowStore,
    reference: ReviewPassRef,
    turn: TurnId,
) -> ReviewPass {
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
    pass
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
    ReviewPassEvidence::new(pass.reference(), pass.kind(), policy, pass.state())
}

async fn succeed_fixture_passes(
    pool: &PgPool,
    store: &ReviewWorkflowStore,
    references: &[ReviewPassRef],
) -> Vec<ReviewPassEvidence> {
    let turn = TurnId::from_uuid(uuid(0x203));
    for reference in references {
        start_review_pass(store, *reference, turn).await;
    }
    let output_frontier = synthetically_terminalize_turn(pool, turn, "completed").await;
    let mut evidence = Vec::with_capacity(references.len());
    for reference in references {
        evidence.push(
            conclude_review_pass(
                store,
                *reference,
                ReviewPassState::Succeeded {
                    turn,
                    output_frontier,
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

/// INV-040 / INV-041: the store reconstructs complete workflow evidence,
/// including the canonical reservation, attachment, and observation sequence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_inv041_review_workflow_store_reconstructs_complete_evidence()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
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
    let run = ReviewRun::new(
        run_ref,
        ReviewWorkflowKind::ReadOnlyReview,
        ReviewPolicy::version_one(),
    );
    let pass = ReviewPass::try_new(
        pass_ref,
        ReviewPassKind::ReadOnlyReview,
        ReviewWorkflowKind::ReadOnlyReview,
        session,
        accepted_input,
        session,
    )
    .expect("accepted input belongs to the fixture session");
    store.insert_run(&run).await.expect("queued run persists");
    store
        .insert_pass(&pass)
        .await
        .expect("queued pass persists");
    let judge_pass = insert_pass_for_target(
        &store,
        target_id,
        0x306,
        ReviewPassKind::Judge,
        session,
        accepted_input,
    )
    .await;
    let publish_pass = insert_pass_for_target(
        &store,
        target_id,
        0x307,
        ReviewPassKind::Publish,
        session,
        accepted_input,
    )
    .await;
    let import_pass = insert_pass_for_target(
        &store,
        target_id,
        0x308,
        ReviewPassKind::ImportExternalContext,
        session,
        accepted_input,
    )
    .await;
    for reference in [pass_ref, judge_pass, publish_pass, import_pass] {
        start_review_pass(&store, reference, turn).await;
    }
    let output_frontier = synthetically_terminalize_turn(&pool, turn, "completed").await;
    let review_evidence = conclude_review_pass(
        &store,
        pass_ref,
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
        },
    )
    .await;
    let judge_evidence = conclude_review_pass(
        &store,
        judge_pass,
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
        },
    )
    .await;
    let publish_evidence = conclude_review_pass(
        &store,
        publish_pass,
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
        },
    )
    .await;
    let import_evidence = conclude_review_pass(
        &store,
        import_pass,
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
        },
    )
    .await;

    let finding_ref = ReviewFindingRef::new(pass_ref, ReviewFindingId::from_uuid(uuid(0x304)));
    let open_finding = finding(finding_ref, review_evidence, &target);
    store
        .insert_finding(&open_finding)
        .await
        .expect("open finding persists");
    let accepted_event = ReviewFindingEvent::new(
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
    let reservation = ReviewExternalLink::reserve(
        link_id,
        ReviewExternalLinkAssociation::Finding(finding_ref),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
    );
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
    let conflicting = ReviewExternalLink::reserve(
        link_id,
        ReviewExternalLinkAssociation::Finding(finding_ref),
        key("another-code-host"),
        ReviewExternalObjectKind::ReviewComment,
    );
    assert!(matches!(
        store.reserve_external_link(conflicting).await,
        Err(ReviewWorkflowStoreError::ReservationConflict(_))
    ));

    let attachment =
        ReviewExternalLinkAttachment::new(link_id, publish_evidence, key("comment-84"));
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
    let posted_event = ReviewFindingEvent::new(
        finding_ref,
        ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
        publish_evidence,
        ReviewFindingEventKind::Posted {
            link: signalbox_domain::ReviewFindingExternalLinkRef::try_new(finding_ref, &attached)
                .expect("attached canonical link belongs to the finding"),
        },
    );
    let posted_finding = accepted_finding
        .apply(posted_event.clone())
        .expect("accepted finding may record an attached publication");
    assert_eq!(
        store
            .append_finding_event(finding_ref.finding(), posted_event)
            .await
            .expect("posted event persists after attachment"),
        Some(posted_finding)
    );
    let observation = ReviewExternalLinkObservation::new(
        link_id,
        ReviewEventOrdinal::one(),
        import_evidence,
        ReviewExternalObjectState::Current,
    );
    let observed = attached
        .observe(observation)
        .expect("first observation is contiguous");
    assert_eq!(
        store
            .append_external_observation(link_id, observation)
            .await
            .expect("observation persists"),
        Some(observed.clone())
    );
    assert_eq!(
        store
            .load_external_link(link_id)
            .await
            .expect("complete link loads"),
        Some(observed)
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
        turn_error.detail().contains("TurnSessionMismatch"),
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
    let turn = TurnId::from_uuid(uuid(0x203));
    start_review_pass(&fixture.store, fixture.pass, turn).await;

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

/// INV-040: a pass projection may report only the canonical outcome of its
/// referenced turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_pass_projection_rejects_noncanonical_turn_outcome() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let turn = TurnId::from_uuid(uuid(0x203));
    start_review_pass(&fixture.store, fixture.pass, turn).await;

    let guarded = sqlx::query(
        "UPDATE review_pass
            SET state_kind = 'failed'
          WHERE pass_id = $1",
    )
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("pass failure requires a canonically failed or refused turn");
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

/// INV-040: appending an event through another same-run finding fails before
/// persistence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_finding_event_rejects_foreign_owner() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let review_evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass]).await[0];
    let first = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x330)));
    let second = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x331)));
    fixture
        .store
        .insert_finding(&finding(first, review_evidence, &fixture.target_snapshot))
        .await?;
    fixture
        .store
        .insert_finding(&finding(second, review_evidence, &fixture.target_snapshot))
        .await?;

    let error = fixture
        .store
        .append_finding_event(
            first.finding(),
            ReviewFindingEvent::new(
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
    let review_evidence = evidence[0];
    let dedupe_evidence = evidence[1];
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x330)));
    let canonical_ref =
        ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x331)));
    let open = finding(finding_ref, review_evidence, &fixture.target_snapshot);
    fixture.store.insert_finding(&open).await?;
    fixture
        .store
        .insert_finding(&finding(
            canonical_ref,
            review_evidence,
            &fixture.target_snapshot,
        ))
        .await?;
    let event = ReviewFindingEvent::new(
        finding_ref,
        ReviewEventOrdinal::one(),
        dedupe_evidence,
        ReviewFindingEventKind::Duplicate {
            canonical: canonical_ref,
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
        .insert_finding(&finding(finding_ref, evidence[0], &fixture.target_snapshot))
        .await?;

    let error = fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            ReviewFindingEvent::new(
                finding_ref,
                ReviewEventOrdinal::one(),
                ReviewPassEvidence::new(
                    publish_pass,
                    ReviewPassKind::Judge,
                    evidence[1].policy(),
                    evidence[1].state(),
                ),
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await
        .expect_err("canonical publication pass cannot accept a finding");
    let ReviewWorkflowStoreError::Database(error) = error else {
        panic!("canonical pass-kind mismatch must be a database rejection");
    };
    assert_sqlstate(&error, "23514");
    Ok(())
}

/// INV-040: finding events cannot cross the frozen policy boundary of the
/// producing review run.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_finding_event_rejects_different_run_policy() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let policy_two = ReviewPolicy::try_new(
        ReviewPolicyVersion::try_new(2).expect("version two is positive"),
        ReviewConfidence::try_from_basis_points(7_500).expect("confidence is bounded"),
        ReviewConfidence::try_from_basis_points(8_500).expect("confidence is bounded"),
    )
    .expect("version-two fixture policy is ordered");
    let judge_pass = insert_pass_for_target_with_policy(
        &fixture.store,
        fixture.target,
        0x3340,
        ReviewPassKind::Judge,
        SessionId::from_uuid(uuid(0x201)),
        AcceptedInputId::from_uuid(uuid(0x202)),
        policy_two,
    )
    .await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x3341)));
    fixture
        .store
        .insert_finding(&finding(finding_ref, evidence[0], &fixture.target_snapshot))
        .await?;

    let mismatch = sqlx::query(
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
    .expect_err("event policy must equal the finding producer policy");
    assert_sqlstate(&mismatch, "23514");
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
        .reserve_external_link(ReviewExternalLink::reserve(
            first,
            ReviewExternalLinkAssociation::Target(fixture.target),
            key("example-code-host"),
            ReviewExternalObjectKind::ReviewComment,
        ))
        .await?;

    let error = fixture
        .store
        .attach_external_link(
            first,
            ReviewExternalLinkAttachment::new(
                second,
                succeeded_pass(fixture.pass, ReviewPassKind::Publish),
                key("comment-337"),
            ),
        )
        .await
        .expect_err("attachment owner must equal the loaded external link");
    assert!(matches!(
        error,
        ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::ExternalLink(
            ReviewExternalLinkTransitionError::ForeignAttachmentLink
        ))
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
    let publish_pass = insert_fixture_pass(&fixture, 0x338, ReviewPassKind::Publish).await;
    let import_pass =
        insert_fixture_pass(&fixture, 0x339, ReviewPassKind::ImportExternalContext).await;
    let evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[publish_pass, import_pass]).await;
    let publish_evidence = evidence[0];
    let import_evidence = evidence[1];
    let first = ReviewExternalLinkId::from_uuid(uuid(0x335));
    let second = ReviewExternalLinkId::from_uuid(uuid(0x336));
    fixture
        .store
        .reserve_external_link(ReviewExternalLink::reserve(
            first,
            ReviewExternalLinkAssociation::Target(fixture.target),
            key("example-code-host"),
            ReviewExternalObjectKind::ReviewComment,
        ))
        .await?;
    fixture
        .store
        .attach_external_link(
            first,
            ReviewExternalLinkAttachment::new(first, publish_evidence, key("comment-335")),
        )
        .await?;
    fixture
        .store
        .reserve_external_link(ReviewExternalLink::reserve(
            second,
            ReviewExternalLinkAssociation::Target(fixture.target),
            key("example-code-host"),
            ReviewExternalObjectKind::ReviewComment,
        ))
        .await?;
    fixture
        .store
        .attach_external_link(
            second,
            ReviewExternalLinkAttachment::new(second, publish_evidence, key("comment-336")),
        )
        .await?;

    let error = fixture
        .store
        .append_external_observation(
            first,
            ReviewExternalLinkObservation::new(
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
        ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::ExternalLink(
            ReviewExternalLinkTransitionError::ForeignObservationLink
        ))
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
    let run = ReviewRunRef::new(target.id(), ReviewRunId::from_uuid(uuid(0x341)));
    let pass = ReviewPassRef::new(run, ReviewPassId::from_uuid(uuid(0x342)));
    fixture
        .store
        .insert_run(&ReviewRun::new(
            run,
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        ))
        .await?;
    fixture
        .store
        .insert_pass(
            &ReviewPass::try_new(
                pass,
                ReviewPassKind::ReadOnlyReview,
                ReviewWorkflowKind::ReadOnlyReview,
                SessionId::from_uuid(uuid(0x201)),
                AcceptedInputId::from_uuid(uuid(0x202)),
                SessionId::from_uuid(uuid(0x201)),
            )
            .expect("fixture input belongs to its session"),
        )
        .await?;
    let review_evidence = succeed_fixture_passes(&pool, &fixture.store, &[pass]).await[0];
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
            Some(ReviewPassEvidence::new(
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
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x306)));
    let accepted = finding(
        finding_ref,
        succeeded_pass(fixture.pass, ReviewPassKind::ReadOnlyReview),
        &fixture.target_snapshot,
    )
    .apply(ReviewFindingEvent::new(
        finding_ref,
        ReviewEventOrdinal::one(),
        succeeded_pass(fixture.pass, ReviewPassKind::Judge),
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
    let attached = ReviewExternalLink::reserve(
        link,
        ReviewExternalLinkAssociation::Target(fixture.target),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
    )
    .attach(ReviewExternalLinkAttachment::new(
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
             accepted_input_id, state_kind, turn_id, output_frontier_id)
         VALUES (
             $1, $2, $3, 'read_only_review', $4,
             $5, 'failed', $6, NULL
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
            evidence[0],
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

/// INV-041: a posted event requires an attached canonical external link.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_schema_requires_attachment_before_posted_event() -> Result<(), Box<dyn Error>> {
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
            evidence[0],
            &fixture.target_snapshot,
        ))
        .await
        .expect("posted-shape fixture persists");
    fixture
        .store
        .append_finding_event(
            posted_finding.finding(),
            ReviewFindingEvent::new(
                posted_finding,
                ReviewEventOrdinal::one(),
                evidence[1],
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await
        .expect("accepted event persists");
    let pending_link = ReviewExternalLinkId::from_uuid(uuid(0x606));
    fixture
        .store
        .reserve_external_link(ReviewExternalLink::reserve(
            pending_link,
            ReviewExternalLinkAssociation::Finding(posted_finding),
            key("example-code-host"),
            ReviewExternalObjectKind::ReviewComment,
        ))
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
    Ok(())
}

/// INV-040: cancelling a running run cannot erase its active pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_schema_running_run_cancellation_retains_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let turn = TurnId::from_uuid(uuid(0x203));
    start_review_pass(&fixture.store, fixture.pass, turn).await;

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

/// INV-040: cancelling a running pass cannot erase its active turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_schema_running_pass_cancellation_retains_turn() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let turn = TurnId::from_uuid(uuid(0x203));
    start_review_pass(&fixture.store, fixture.pass, turn).await;
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
    let reservation = ReviewExternalLink::reserve(
        link,
        ReviewExternalLinkAssociation::Target(fixture.target),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
    );
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
        .insert_finding(&finding(finding_ref, evidence[0], &fixture.target_snapshot))
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
            evidence[0],
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
    let review_evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass]).await[0];
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
        .insert_finding(&finding(finding_ref, evidence[0], &fixture.target_snapshot))
        .await?;
    fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            ReviewFindingEvent::new(
                finding_ref,
                ReviewEventOrdinal::one(),
                evidence[1],
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

/// INV-041: one provider/kind/object identity has at most one attachment.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_external_object_attachment_is_unique() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let publish_pass = insert_fixture_pass(&fixture, 0x503, ReviewPassKind::Publish).await;
    let publish_evidence = succeed_fixture_passes(&pool, &fixture.store, &[publish_pass]).await[0];
    let first_link = ReviewExternalLinkId::from_uuid(uuid(0x501));
    let second_link = ReviewExternalLinkId::from_uuid(uuid(0x502));
    let first_reservation = ReviewExternalLink::reserve(
        first_link,
        ReviewExternalLinkAssociation::Target(fixture.target),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
    );
    let second_reservation = ReviewExternalLink::reserve(
        second_link,
        ReviewExternalLinkAssociation::Target(fixture.target),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
    );
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
            ReviewExternalLinkAttachment::new(first_link, publish_evidence, key("comment-84")),
        )
        .await
        .expect("first attachment persists");

    let duplicate = fixture
        .store
        .attach_external_link(
            second_link,
            ReviewExternalLinkAttachment::new(second_link, publish_evidence, key("comment-84")),
        )
        .await
        .expect_err("one external object identity cannot attach twice");
    let ReviewWorkflowStoreError::Database(duplicate) = duplicate else {
        panic!("external-object uniqueness must be a database rejection")
    };
    assert_sqlstate(&duplicate, "23505");

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
             'commit', NULL, '1122334455667788', NULL, $2
         )",
    )
    .bind(uuid(0x701))
    .bind(fixture.target.into_uuid())
    .execute(&pool)
    .await
    .expect_err("stack parent must be in the target repository");
    assert_sqlstate(&foreign_repository_parent, "23503");
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
             accepted_input_id, state_kind, turn_id, output_frontier_id)
         VALUES ($1, $2, $3, 'read_only_review', $4, $5, 'queued', NULL, NULL)",
    )
    .bind(uuid(0x703))
    .bind(run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(uuid(0x201))
    .bind(uuid(0x202))
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
             accepted_input_id, state_kind, turn_id, output_frontier_id)
         VALUES ($1, $2, $3, 'read_only_review', $4, $5, 'queued', NULL, NULL)",
    )
    .bind(uuid(0x704))
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(uuid(0x201))
    .bind(uuid(0x202))
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
    assert_eq!(pass.state(), ReviewPassState::Cancelled { turn: None });
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
    start_review_pass(&fixture.store, fixture.pass, turn).await;
    synthetically_terminalize_turn(&pool, turn, "completed").await;
    let loaded = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await?
        .expect("running pass still loads after its turn concludes");
    assert_eq!(loaded.state(), ReviewPassState::Running { turn });
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
    start_review_pass(&fixture.store, fixture.pass, turn).await;
    start_review_pass(&fixture.store, judge_pass, other_turn).await;
    let output_frontier = synthetically_terminalize_turn(&pool, turn, "completed").await;
    synthetically_terminalize_turn(&pool, other_turn, "failed").await;
    let review_evidence = conclude_review_pass(
        &fixture.store,
        fixture.pass,
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
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
        .reserve_external_link(ReviewExternalLink::reserve(
            link,
            ReviewExternalLinkAssociation::Target(fixture.target),
            key("example-code-host"),
            ReviewExternalObjectKind::ReviewComment,
        ))
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

/// INV-041: observation insertion authenticates a succeeded import pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_observation_rejects_queued_import_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let publish_pass = insert_fixture_pass(&fixture, 0x709, ReviewPassKind::Publish).await;
    let import_pass =
        insert_fixture_pass(&fixture, 0x70a, ReviewPassKind::ImportExternalContext).await;
    let publish_evidence = succeed_fixture_passes(&pool, &fixture.store, &[publish_pass]).await[0];
    let link = ReviewExternalLinkId::from_uuid(uuid(0x70b));
    fixture
        .store
        .reserve_external_link(ReviewExternalLink::reserve(
            link,
            ReviewExternalLinkAssociation::Target(fixture.target),
            key("example-code-host"),
            ReviewExternalObjectKind::ReviewComment,
        ))
        .await?;
    fixture
        .store
        .attach_external_link(
            link,
            ReviewExternalLinkAttachment::new(link, publish_evidence, key("comment-70b")),
        )
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

    let turn = TurnId::from_uuid(uuid(0x203));
    for reference in [fixture.pass, judge_pass, attaching_pass, other_publish_pass] {
        start_review_pass(&fixture.store, reference, turn).await;
    }
    start_review_pass(&fixture.store, blocked_publish_pass, blocked_turn).await;
    let output_frontier = synthetically_terminalize_turn(&pool, turn, "completed").await;
    synthetically_terminalize_turn(&pool, blocked_turn, "reconciliation_required").await;

    let mut succeeded = Vec::new();
    for reference in [fixture.pass, judge_pass, attaching_pass, other_publish_pass] {
        succeeded.push(
            conclude_review_pass(
                &fixture.store,
                reference,
                ReviewPassState::Succeeded {
                    turn,
                    output_frontier,
                },
            )
            .await,
        );
    }
    let blocked_evidence = conclude_review_pass(
        &fixture.store,
        blocked_publish_pass,
        ReviewPassState::Blocked { turn: blocked_turn },
    )
    .await;

    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x710)));
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            succeeded[0],
            &fixture.target_snapshot,
        ))
        .await?;
    fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            ReviewFindingEvent::new(
                finding_ref,
                ReviewEventOrdinal::one(),
                succeeded[1],
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await?;
    fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            ReviewFindingEvent::new(
                finding_ref,
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                blocked_evidence,
                ReviewFindingEventKind::BlockedWithReason {
                    reason: text("publication acknowledgement is unresolved"),
                },
            ),
        )
        .await?;

    let link = ReviewExternalLinkId::from_uuid(uuid(0x711));
    let reservation = ReviewExternalLink::reserve(
        link,
        ReviewExternalLinkAssociation::Finding(finding_ref),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
    );
    fixture
        .store
        .reserve_external_link(reservation.clone())
        .await?;
    let attached = fixture
        .store
        .attach_external_link(
            link,
            ReviewExternalLinkAttachment::new(link, succeeded[2], key("comment-711")),
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

    let posted = fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            ReviewFindingEvent::new(
                finding_ref,
                ReviewEventOrdinal::try_new(3).expect("positive ordinal"),
                succeeded[2],
                ReviewFindingEventKind::Posted {
                    link: signalbox_domain::ReviewFindingExternalLinkRef::try_new(
                        finding_ref,
                        &attached,
                    )
                    .expect("attached link belongs to the finding"),
                },
            ),
        )
        .await?
        .expect("publication reconciliation persists");
    assert_eq!(posted.status(), ReviewFindingStatus::Posted);
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
