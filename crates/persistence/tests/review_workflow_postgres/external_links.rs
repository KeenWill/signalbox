//! External links coverage.

use super::*;

const ARBITRARY_REVIEW_INTERRUPT_CONTENT: &str = "Continue after review reconciliation";

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
