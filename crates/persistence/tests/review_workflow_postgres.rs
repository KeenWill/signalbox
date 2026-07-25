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
    ReviewExternalObjectKind, ReviewExternalObjectState, ReviewFinding, ReviewFindingContent,
    ReviewFindingDiffSide, ReviewFindingEvent, ReviewFindingEventKind, ReviewFindingId,
    ReviewFindingLocation, ReviewFindingProposal, ReviewFindingRef, ReviewFindingSeverity,
    ReviewFindingTransitionFailure, ReviewKey, ReviewLineRange, ReviewPass, ReviewPassId,
    ReviewPassKind, ReviewPassRef, ReviewPassState, ReviewPassTurnEvidence, ReviewPassTurnOutcome,
    ReviewPolicy, ReviewRun, ReviewRunId, ReviewRunPassEvidence, ReviewRunRef, ReviewRunState,
    ReviewTarget, ReviewTargetId, ReviewTargetSubject, ReviewText, ReviewWorkflowKind,
    SemanticTranscriptEntryId, SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
    SessionCreationCause, SessionCreationProvenance, SessionId, SubmitInput, TranscriptAncestry,
    TurnAttemptId, TurnId, UserContent,
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
    producing_pass: ReviewPassRef,
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
    producing_pass: ReviewPassRef,
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

    let running_pass = pass
        .clone()
        .transition(
            ReviewPassState::Running { turn },
            Some(ReviewPassTurnEvidence::new(
                turn,
                session,
                accepted_input,
                ReviewPassTurnOutcome::Active,
                None,
            )),
        )
        .expect("queued pass activates");
    assert_eq!(
        store
            .transition_pass(pass_ref.pass(), running_pass.state())
            .await
            .expect("pass transition persists"),
        Some(running_pass.clone())
    );
    let running_run = run
        .clone()
        .transition(
            ReviewRunState::Running {
                active_pass: pass_ref,
            },
            Some(ReviewRunPassEvidence::new(pass_ref, running_pass.state())),
        )
        .expect("queued run activates");
    assert_eq!(
        store
            .transition_run(run_ref.run(), running_run.state())
            .await
            .expect("run transition persists"),
        Some(running_run)
    );

    let finding_ref = ReviewFindingRef::new(run_ref, ReviewFindingId::from_uuid(uuid(0x304)));
    let open_finding = finding(finding_ref, pass_ref, &target);
    store
        .insert_finding(&open_finding)
        .await
        .expect("open finding persists");
    let accepted_event = ReviewFindingEvent::new(
        finding_ref,
        ReviewEventOrdinal::one(),
        pass_ref,
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

    let attachment = ReviewExternalLinkAttachment::new(pass_ref, key("comment-84"));
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
        pass_ref,
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
        ReviewEventOrdinal::one(),
        pass_ref,
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

/// INV-040: the adapter supplies canonical accepted-input and turn rows to
/// domain reconstitution instead of trusting repeated pass-row identifiers.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_pass_loader_rejects_cross_wired_canonical_evidence() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let pass = fixture.pass.pass();
    let original_session = SessionId::from_uuid(uuid(0x201));
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

    sqlx::query(
        "UPDATE review_pass
            SET session_id = $2
          WHERE pass_id = $1",
    )
    .bind(pass.into_uuid())
    .bind(original_session.into_uuid())
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
    fixture
        .store
        .transition_pass(fixture.pass.pass(), ReviewPassState::Running { turn })
        .await?
        .expect("fixture pass exists");
    fixture
        .store
        .transition_run(
            fixture.run.run(),
            ReviewRunState::Running {
                active_pass: fixture.pass,
            },
        )
        .await?
        .expect("fixture run exists");

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
    fixture
        .store
        .transition_pass(fixture.pass.pass(), ReviewPassState::Running { turn })
        .await?
        .expect("fixture pass exists");

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
    let first = ReviewFindingRef::new(fixture.run, ReviewFindingId::from_uuid(uuid(0x330)));
    let second = ReviewFindingRef::new(fixture.run, ReviewFindingId::from_uuid(uuid(0x331)));
    fixture
        .store
        .insert_finding(&finding(first, fixture.pass, &fixture.target_snapshot))
        .await?;
    fixture
        .store
        .insert_finding(&finding(second, fixture.pass, &fixture.target_snapshot))
        .await?;

    let error = fixture
        .store
        .append_finding_event(
            first.finding(),
            ReviewFindingEvent::new(
                second,
                ReviewEventOrdinal::one(),
                fixture.pass,
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
                SessionId::from_uuid(uuid(0x201)),
                AcceptedInputId::from_uuid(uuid(0x202)),
                SessionId::from_uuid(uuid(0x201)),
            )
            .expect("fixture input belongs to its session"),
        )
        .await?;
    let file_relative = ReviewFindingRef::new(run, ReviewFindingId::from_uuid(uuid(0x343)));
    let file_relative = finding_with_side(file_relative, pass, &target, None);
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

/// INV-040 / INV-041: new rows admit only queued run/pass state and a pending
/// pre-effect external-link reservation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_inv041_new_records_require_initial_shapes() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let turn = TurnId::from_uuid(uuid(0x203));
    let running_pass = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await
        .expect("pass loads")
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
        .expect("queued pass activates");
    assert!(matches!(
        fixture.store.insert_pass(&running_pass).await,
        Err(ReviewWorkflowStoreError::InvalidInsertion(
            ReviewWorkflowInsertionError::PassNotQueued { .. }
        ))
    ));

    let running_run = fixture
        .store
        .load_run(fixture.run.run())
        .await
        .expect("run loads")
        .expect("fixture run exists")
        .transition(
            ReviewRunState::Running {
                active_pass: fixture.pass,
            },
            Some(ReviewRunPassEvidence::new(
                fixture.pass,
                running_pass.state(),
            )),
        )
        .expect("queued run activates");
    assert!(matches!(
        fixture.store.insert_run(&running_run).await,
        Err(ReviewWorkflowStoreError::InvalidInsertion(
            ReviewWorkflowInsertionError::RunNotQueued { .. }
        ))
    ));

    let finding_ref = ReviewFindingRef::new(fixture.run, ReviewFindingId::from_uuid(uuid(0x306)));
    let accepted_finding = finding(finding_ref, fixture.pass, &fixture.target_snapshot)
        .apply(ReviewFindingEvent::new(
            finding_ref,
            ReviewEventOrdinal::one(),
            fixture.pass,
            ReviewFindingEventKind::Accepted,
        ))
        .expect("open finding accepts judgment");
    assert!(matches!(
        fixture.store.insert_finding(&accepted_finding).await,
        Err(ReviewWorkflowStoreError::InvalidInsertion(
            ReviewWorkflowInsertionError::FindingNotOpen { .. }
        ))
    ));

    let attached_reservation = ReviewExternalLink::reserve(
        ReviewExternalLinkId::from_uuid(uuid(0x307)),
        ReviewExternalLinkAssociation::Target(fixture.target),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
    )
    .attach(ReviewExternalLinkAttachment::new(
        fixture.pass,
        key("comment-85"),
    ))
    .expect("same-target pass may attach");
    assert!(matches!(
        fixture
            .store
            .reserve_external_link(attached_reservation)
            .await,
        Err(ReviewWorkflowStoreError::InvalidInsertion(
            ReviewWorkflowInsertionError::ExternalLinkNotPending
        ))
    ));

    Ok(())
}

/// INV-040 / INV-041: nullable SQL shapes, exact version-one policy, and
/// evidence-preserving transition guards reject representations the domain
/// cannot construct.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv040_inv041_schema_closes_review_workflow_shapes() -> Result<(), Box<dyn Error>> {
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

    let reason_finding =
        ReviewFindingRef::new(fixture.run, ReviewFindingId::from_uuid(uuid(0x604)));
    fixture
        .store
        .insert_finding(&finding(
            reason_finding,
            fixture.pass,
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
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("rejected events require a reason");
    assert_sqlstate(&missing_reason, "23514");

    let posted_finding =
        ReviewFindingRef::new(fixture.run, ReviewFindingId::from_uuid(uuid(0x605)));
    fixture
        .store
        .insert_finding(&finding(
            posted_finding,
            fixture.pass,
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
                fixture.pass,
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
         VALUES ($1, 2, $2, $3, $4, $2, 'posted', NULL, NULL, $5, 'finding')",
    )
    .bind(posted_finding.finding().into_uuid())
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(fixture.pass.pass().into_uuid())
    .bind(pending_link.into_uuid())
    .execute(&pool)
    .await
    .expect_err("posted status requires attached external-effect evidence");
    assert_sqlstate(&posted_without_attachment, "23503");

    let turn = TurnId::from_uuid(uuid(0x203));
    fixture
        .store
        .transition_pass(fixture.pass.pass(), ReviewPassState::Running { turn })
        .await
        .expect("pass activation persists");
    fixture
        .store
        .transition_run(
            fixture.run.run(),
            ReviewRunState::Running {
                active_pass: fixture.pass,
            },
        )
        .await
        .expect("run activation persists");

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
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.pass.pass().into_uuid())
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
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.pass.pass().into_uuid())
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
    let finding_ref = ReviewFindingRef::new(fixture.run, ReviewFindingId::from_uuid(uuid(0x309)));
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            fixture.pass,
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
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, external_link_id,
             external_link_association_kind)
         VALUES ($1, 1, $2, $3, $4, $2, 'accepted', NULL, NULL, NULL, NULL)",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(fixture.pass.pass().into_uuid())
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
    let second_finding_ref =
        ReviewFindingRef::new(fixture.run, ReviewFindingId::from_uuid(uuid(0x306)));
    fixture
        .store
        .insert_finding(&finding(
            second_finding_ref,
            fixture.pass,
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
         VALUES ($1, 2, $2, $3, $4, $2, 'accepted', NULL, NULL, NULL, NULL)",
    )
    .bind(second_finding_ref.finding().into_uuid())
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(fixture.pass.pass().into_uuid())
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
    .expect_err("producing pass must belong to the finding run and target");
    assert_sqlstate(&cross_wired, "23503");

    Ok(())
}

/// INV-041: one provider/kind/object identity has at most one attachment.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv041_external_object_attachment_is_unique() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
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
            ReviewExternalLinkAttachment::new(fixture.pass, key("comment-84")),
        )
        .await
        .expect("first attachment persists");

    let duplicate = fixture
        .store
        .attach_external_link(
            second_link,
            ReviewExternalLinkAttachment::new(fixture.pass, key("comment-84")),
        )
        .await
        .expect_err("one external object identity cannot attach twice");
    let ReviewWorkflowStoreError::Database(duplicate) = duplicate else {
        panic!("external-object uniqueness must be a database rejection")
    };
    assert_sqlstate(&duplicate, "23505");

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
