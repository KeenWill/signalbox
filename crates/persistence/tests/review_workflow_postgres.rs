#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::error::Error;

use signalbox_domain::{
    AcceptedInputId, CancelledModelCallTurnIdentities, ContextFrontierId, CreateSession,
    DeliveryRequest, DirectModelSelection, DurableCommandId, ModelSelectionOverride,
    ModelSelectionRequest, PerInputConfigurationChoices, ReviewChangeRequestNumber,
    ReviewConfidence, ReviewEventOrdinal, ReviewExternalLink, ReviewExternalLinkAssociation,
    ReviewExternalLinkAttachment, ReviewExternalLinkId, ReviewExternalLinkObservation,
    ReviewExternalObjectKind, ReviewExternalObjectState, ReviewFinding, ReviewFindingContent,
    ReviewFindingDiffSide, ReviewFindingEvent, ReviewFindingEventKind, ReviewFindingId,
    ReviewFindingLocation, ReviewFindingProposal, ReviewFindingRef, ReviewFindingSeverity,
    ReviewKey, ReviewLineRange, ReviewPass, ReviewPassId, ReviewPassKind, ReviewPassRef,
    ReviewPassState, ReviewPolicy, ReviewRun, ReviewRunId, ReviewRunRef, ReviewRunState,
    ReviewTarget, ReviewTargetId, ReviewTargetSubject, ReviewText, ReviewWorkflowKind,
    SemanticTranscriptEntryId, SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
    SessionCreationCause, SessionCreationProvenance, SessionId, SubmitInput, TranscriptAncestry,
    TurnId, UserContent,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    local_test_connection_options, migrate,
    review_workflow::{
        ReserveExternalLinkOutcome, ReviewWorkflowInsertionError, ReviewWorkflowStore,
        ReviewWorkflowStoreError,
    },
    submit_input::SubmitInputRepository,
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

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

async fn insert_queued_turn(
    pool: &PgPool,
    session: SessionId,
    accepted_input: AcceptedInputId,
    turn: TurnId,
) {
    let create = CreateSession::new(
        DurableCommandId::from_uuid(uuid(0x101)),
        SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::None,
        ),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(uuid(0x102)),
        )),
    )
    .prepare(session)
    .expect("owner-created fixture session is preparable");
    CreateSessionRepository::new(pool.clone())
        .handle(create)
        .await
        .expect("fixture session persists");

    let submit = SubmitInput::new(
        DurableCommandId::from_uuid(uuid(0x103)),
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
                SemanticTranscriptEntryId::from_uuid(uuid(0x104)),
                ContextFrontierId::from_uuid(uuid(0x105)),
            ),
            |_| TurnId::from_uuid(uuid(0x106)),
            |requests| {
                (
                    requests
                        .iter()
                        .enumerate()
                        .map(|(index, _)| {
                            SemanticTranscriptEntryId::from_uuid(uuid(
                                0x110 + u128::try_from(index).expect("small fixture"),
                            ))
                        })
                        .collect(),
                    ContextFrontierId::from_uuid(uuid(0x120)),
                )
            },
        )
        .await
        .expect("fixture input and queued turn persist");
}

fn finding(reference: ReviewFindingRef, producing_pass: ReviewPassRef) -> ReviewFinding {
    ReviewFinding::new(
        ReviewFindingProposal::try_new(
            reference,
            producing_pass,
            ReviewFindingContent::new(
                ReviewFindingLocation::new(
                    key("src/review.rs"),
                    Some(ReviewLineRange::try_new(11, 14).expect("ordered fixture range")),
                    ReviewFindingDiffSide::Right,
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
    run: ReviewRunRef,
    pass: ReviewPassRef,
}

async fn insert_review_pass_fixture(pool: &PgPool) -> PersistedReviewPassFixture {
    let store = ReviewWorkflowStore::new(pool.clone());
    let session = SessionId::from_uuid(uuid(0x201));
    let accepted_input = AcceptedInputId::from_uuid(uuid(0x202));
    let turn = TurnId::from_uuid(uuid(0x203));
    insert_queued_turn(pool, session, accepted_input, turn).await;

    let target = ReviewTargetId::from_uuid(uuid(0x301));
    store
        .insert_target(
            &ReviewTarget::try_new(
                target,
                key("example-code-host"),
                key("example/repository"),
                ReviewTargetSubject::Commit,
                key("0123456789abcdef"),
                None,
                None,
            )
            .expect("fixture target topology is valid"),
        )
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
        .insert_pass(&ReviewPass::new(
            pass,
            ReviewPassKind::ReadOnlyReview,
            session,
            accepted_input,
        ))
        .await
        .expect("queued pass persists");

    PersistedReviewPassFixture {
        store,
        target,
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
    insert_queued_turn(&pool, session, accepted_input, turn).await;

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
    let pass = ReviewPass::new(
        pass_ref,
        ReviewPassKind::ReadOnlyReview,
        session,
        accepted_input,
    );
    store.insert_run(&run).await.expect("queued run persists");
    store
        .insert_pass(&pass)
        .await
        .expect("queued pass persists");

    let running_pass = pass
        .clone()
        .transition(ReviewPassState::Running { turn })
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
        .transition(ReviewRunState::Running {
            active_pass: pass_ref,
        })
        .expect("queued run activates");
    assert_eq!(
        store
            .transition_run(run_ref.run(), running_run.state())
            .await
            .expect("run transition persists"),
        Some(running_run)
    );

    let finding_ref = ReviewFindingRef::new(run_ref, ReviewFindingId::from_uuid(uuid(0x304)));
    let open_finding = finding(finding_ref, pass_ref);
    store
        .insert_finding(&open_finding)
        .await
        .expect("open finding persists");
    let accepted_event = ReviewFindingEvent::new(
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
        Some(accepted_finding)
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
        .transition(ReviewPassState::Running { turn })
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
        .transition(ReviewRunState::Running {
            active_pass: fixture.pass,
        })
        .expect("queued run activates");
    assert!(matches!(
        fixture.store.insert_run(&running_run).await,
        Err(ReviewWorkflowStoreError::InvalidInsertion(
            ReviewWorkflowInsertionError::RunNotQueued { .. }
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
        .insert_finding(&finding(second_finding_ref, fixture.pass))
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
                None,
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
