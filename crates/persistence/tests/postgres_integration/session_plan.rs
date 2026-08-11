//! Session plan append, projection, and dependency graph invariants.

use crate::*;

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

/// Appending a dependency reads only the invoking session's graph even when a
/// different session uses the same entry ordinals.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_dependency_append_ignores_other_session_edges() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first = dependency_plan_fixture(&pool, Vec::new()).await?;
    let second = dependency_plan_fixture(&pool, Vec::new()).await?;
    let second_page = second
        .repository
        .read(PlanReadRequest::new(second.session, None, None))
        .await?;
    let second_dependent = second_page
        .entries()
        .get(1)
        .expect("the second session's dependent entry is projected");
    let expected_dependencies = vec![second.prerequisite];

    assert_ne!(first.session, second.session);
    assert_eq!(
        second_dependent.dependencies(),
        expected_dependencies.as_slice()
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A prerequisite status change recomputes the dependent entry from waiting to
/// ready without changing its closed plan status.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_dependency_readiness_tracks_completion() -> Result<(), Box<dyn Error>> {
    const EXPECTED_ENTRY_COUNT: usize = 2;
    let (container, pool, _database_url) = migrated_postgres().await?;
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let completed_status = PlanStatus::Completed;
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![status_plan_arguments(prerequisite, completed_status)],
    )
    .await?;
    let expected_dependencies = vec![fixture.prerequisite];
    let waiting = fixture
        .repository
        .read(PlanReadRequest::new(fixture.session, None, None))
        .await?;
    let waiting_entry = waiting
        .entries()
        .get(1)
        .expect("the dependent entry is projected");

    assert_eq!(waiting.entries().len(), EXPECTED_ENTRY_COUNT);
    assert_eq!(
        waiting_entry.dependencies(),
        expected_dependencies.as_slice()
    );
    assert_eq!(waiting_entry.readiness(), PlanReadiness::Waiting);

    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::SetStatus {
            entry: fixture.prerequisite,
            status: completed_status,
        },
    )
    .await?;
    let ready = fixture
        .repository
        .read(PlanReadRequest::new(fixture.session, None, None))
        .await?;
    let ready_entry = ready
        .entries()
        .get(1)
        .expect("the dependent entry remains projected");

    assert_eq!(ready_entry.dependencies(), expected_dependencies.as_slice());
    assert_eq!(ready_entry.readiness(), PlanReadiness::Ready);

    pool.close().await;
    drop(container);
    Ok(())
}

/// The optional history projection retains the dependency event exactly.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_dependency_event_is_retained_in_history() -> Result<(), Box<dyn Error>> {
    const HISTORY_LIMIT: usize = 10;
    const EXPECTED_HISTORY_EVENT_COUNT: usize = 3;
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = dependency_plan_fixture(&pool, Vec::new()).await?;
    let page = fixture
        .repository
        .read(PlanReadRequest::new(
            fixture.session,
            None,
            Some(HISTORY_LIMIT),
        ))
        .await?;
    let history = page
        .history()
        .expect("the requested dependency history is returned");
    let dependency_event = history
        .events()
        .last()
        .expect("the dependency event closes the fixture history");

    assert_eq!(history.events().len(), EXPECTED_HISTORY_EVENT_COUNT);
    assert_eq!(
        dependency_edge(dependency_event),
        (fixture.dependent, fixture.prerequisite)
    );
    assert_eq!(history.completeness(), PlanPageCompleteness::Complete);

    pool.close().await;
    drop(container);
    Ok(())
}

/// The schema independently rejects a raw dependency back-edge.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_schema_trigger_rejects_a_dependency_cycle() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let mut fixture =
        dependency_plan_fixture(&pool, vec![depends_plan_arguments(prerequisite, dependent)])
            .await?;
    let cycle_attempt = fixture.batch.authorize_next().await?;

    let error = insert_direct_dependency_event(&pool, &fixture, &cycle_attempt)
        .await
        .expect_err("the schema trigger rejects a dependency cycle");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("session_plan_dependency_cycle")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The declarative event-shape constraint rejects a dependency without its
/// target even when the append trigger is disabled.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_shape_rejects_a_dependency_without_a_target() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let mut fixture =
        dependency_plan_fixture(&pool, vec![depends_plan_arguments(prerequisite, dependent)])
            .await?;
    let attempt = fixture.batch.authorize_next().await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    let error = insert_dependency_without_target(&pool, &fixture, &attempt)
        .await
        .expect_err("the event-shape constraint rejects the missing dependency");
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("session_plan_event_shape")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Repeated physical edge events retain history while the bounded current
/// projection stores the relationship once.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_current_projection_deduplicates_edges_when_rejecting_cycle()
-> Result<(), Box<dyn Error>> {
    const EXPECTED_PROJECTED_EDGE_COUNT: i64 = 1;
    let (container, pool, _database_url) = migrated_postgres().await?;
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            depends_plan_arguments(dependent, prerequisite),
            depends_plan_arguments(prerequisite, dependent),
        ],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::DependsOn {
            entry: fixture.dependent,
            dependency: fixture.prerequisite,
        },
    )
    .await?;
    let projected_edge_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM session_plan_current_dependency
          WHERE session_id = $1
            AND entry_ordinal = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(fixture.dependent.as_u64()))
    .fetch_one(&pool)
    .await?;
    let cycle_attempt = fixture.batch.authorize_next().await?;
    let outcome = fixture
        .repository
        .append(PlanAppendRequest::new(
            PlanEventProvenance::from_invocation(cycle_attempt.correlation()),
            PlanEventDraft::DependsOn {
                entry: fixture.prerequisite,
                dependency: fixture.dependent,
            },
        ))
        .await?;
    let cycle = expect_dependency_cycle(outcome);
    let expected_path = vec![
        fixture.prerequisite,
        fixture.dependent,
        fixture.prerequisite,
    ];

    assert_eq!(projected_edge_count, EXPECTED_PROJECTED_EDGE_COUNT);
    assert_eq!(cycle.entry(), fixture.prerequisite);
    assert_eq!(cycle.dependency(), fixture.dependent);
    assert_eq!(cycle.path(), expected_path.as_slice());

    pool.close().await;
    drop(container);
    Ok(())
}

/// The head-to-edge chain keeps every current dependency row reachable by
/// foreign key, so bypassing immutability still cannot lose a projection row.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_dependency_head_prevents_projection_loss() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = dependency_plan_fixture(&pool, Vec::new()).await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         DISABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(&pool)
    .await?;

    let deletion = sqlx::query(
        "DELETE FROM session_plan_current_dependency
          WHERE session_id = $1
            AND first_event_ordinal = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(3_u64))
    .execute(&pool)
    .await
    .expect_err("the dependency head retains the projected edge");

    assert_eq!(
        deletion
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("session_plan_head_session_id_dependency_event_ordinal_fkey")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Bypassing projection immutability cannot rewrite the dependency head to
/// skip the immediately preceding distinct edge.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_dependency_predecessor_cannot_skip_an_edge() -> Result<(), Box<dyn Error>> {
    const NEW_DEPENDENT_TEXT: &str = "preserve the dependency predecessor";
    const NEW_DEPENDENT_EVENT_ORDINAL: u64 = 4;
    const NEW_DEPENDENCY_EVENT_ORDINAL: u64 = 5;
    let new_dependent = PlanEntryId::try_from_u64(NEW_DEPENDENT_EVENT_ORDINAL)
        .expect("the new dependent fixture identity is positive");
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            create_plan_arguments(NEW_DEPENDENT_TEXT),
            depends_plan_arguments(new_dependent, prerequisite),
        ],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(NEW_DEPENDENT_TEXT),
        },
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::DependsOn {
            entry: new_dependent,
            dependency: fixture.prerequisite,
        },
    )
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         DISABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(&pool)
    .await?;

    let rewrite = sqlx::query(
        "UPDATE session_plan_current_dependency
            SET prior_first_event_ordinal = NULL
          WHERE session_id = $1
            AND first_event_ordinal = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(NEW_DEPENDENCY_EVENT_ORDINAL))
    .execute(&pool)
    .await
    .expect_err("the newest edge must retain its immediate predecessor");
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         ENABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(&pool)
    .await?;

    assert_eq!(
        rewrite
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("session_plan_dependency_predecessor")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Reintroducing a missing middle projection edge cannot leave its existing
/// immediate successor pointing past it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_dependency_middle_insert_requires_immediate_successor()
-> Result<(), Box<dyn Error>> {
    const THIRD_ENTRY_TEXT: &str = "insert the missing middle dependency";
    const FOURTH_ENTRY_TEXT: &str = "retain the later dependency successor";
    const THIRD_ENTRY_ORDINAL: u64 = 4;
    const FOURTH_ENTRY_ORDINAL: u64 = 5;
    const FIRST_DEPENDENCY_EVENT_ORDINAL: u64 = 3;
    const MIDDLE_DEPENDENCY_EVENT_ORDINAL: u64 = 6;
    const SUCCESSOR_DEPENDENCY_EVENT_ORDINAL: u64 = 7;
    let third_entry = PlanEntryId::try_from_u64(THIRD_ENTRY_ORDINAL)
        .expect("the third entry fixture identity is positive");
    let fourth_entry = PlanEntryId::try_from_u64(FOURTH_ENTRY_ORDINAL)
        .expect("the fourth entry fixture identity is positive");
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            create_plan_arguments(THIRD_ENTRY_TEXT),
            create_plan_arguments(FOURTH_ENTRY_TEXT),
            depends_plan_arguments(third_entry, prerequisite),
            depends_plan_arguments(fourth_entry, prerequisite),
        ],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(THIRD_ENTRY_TEXT),
        },
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(FOURTH_ENTRY_TEXT),
        },
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::DependsOn {
            entry: third_entry,
            dependency: fixture.prerequisite,
        },
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::DependsOn {
            entry: fourth_entry,
            dependency: fixture.prerequisite,
        },
    )
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         DISABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         DISABLE TRIGGER session_plan_current_dependency_predecessor_guard",
    )
    .execute(&pool)
    .await?;
    let skipped = sqlx::query(
        "UPDATE session_plan_current_dependency
            SET prior_first_event_ordinal = $1
          WHERE session_id = $2
            AND first_event_ordinal = $3",
    )
    .bind(Decimal::from(FIRST_DEPENDENCY_EVENT_ORDINAL))
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(SUCCESSOR_DEPENDENCY_EVENT_ORDINAL))
    .execute(&pool)
    .await?;
    let removed = sqlx::query(
        "DELETE FROM session_plan_current_dependency
          WHERE session_id = $1
            AND first_event_ordinal = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(MIDDLE_DEPENDENCY_EVENT_ORDINAL))
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         ENABLE TRIGGER session_plan_current_dependency_predecessor_guard",
    )
    .execute(&pool)
    .await?;

    let insertion = sqlx::query(
        "INSERT INTO session_plan_current_dependency
            (session_id, entry_ordinal, dependency_ordinal,
             first_event_ordinal, prior_first_event_ordinal)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(third_entry.as_u64()))
    .bind(Decimal::from(fixture.prerequisite.as_u64()))
    .bind(Decimal::from(MIDDLE_DEPENDENCY_EVENT_ORDINAL))
    .bind(Decimal::from(FIRST_DEPENDENCY_EVENT_ORDINAL))
    .execute(&pool)
    .await
    .expect_err("a middle edge cannot be inserted beneath a skipping successor");
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         ENABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(&pool)
    .await?;

    assert_eq!(skipped.rows_affected(), EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(removed.rows_affected(), EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        insertion
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("session_plan_dependency_successor")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Only the event append projection trigger may populate the current dependency
/// table, even when a direct row satisfies every relational constraint.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_current_dependency_rejects_direct_insert() -> Result<(), Box<dyn Error>> {
    const PRIOR_DEPENDENCY_EVENT_ORDINAL: u64 = 3;
    const UNRELATED_STATUS_EVENT_ORDINAL: u64 = 4;
    let (container, pool, _database_url) = migrated_postgres().await?;
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![status_plan_arguments(prerequisite, PlanStatus::Completed)],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::SetStatus {
            entry: prerequisite,
            status: PlanStatus::Completed,
        },
    )
    .await?;

    let insertion = sqlx::query(
        "INSERT INTO session_plan_current_dependency (
             session_id, entry_ordinal, dependency_ordinal,
             first_event_ordinal, prior_first_event_ordinal
         )
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(fixture.prerequisite.as_u64()))
    .bind(Decimal::from(fixture.dependent.as_u64()))
    .bind(Decimal::from(UNRELATED_STATUS_EVENT_ORDINAL))
    .bind(Decimal::from(PRIOR_DEPENDENCY_EVENT_ORDINAL))
    .execute(&pool)
    .await
    .expect_err("a direct caller cannot populate the current projection");

    assert_eq!(
        insertion
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("session_plan_current_dependency_maintenance")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A read rejects an unprojected dependency event even when no history was
/// requested and both append-time triggers were deliberately bypassed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_read_rejects_an_unprojected_dependency_event() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let mut fixture =
        dependency_plan_fixture(&pool, vec![depends_plan_arguments(prerequisite, dependent)])
            .await?;
    let corrupt_attempt = fixture.batch.authorize_next().await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_advances_projection",
    )
    .execute(&pool)
    .await?;
    insert_direct_dependency_event(&pool, &fixture, &corrupt_attempt).await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_advances_projection",
    )
    .execute(&pool)
    .await?;
    fixture.batch.finish(corrupt_attempt).await?;

    let error = fixture
        .repository
        .read(PlanReadRequest::new(fixture.session, None, None))
        .await
        .expect_err("the unprojected dependency invalidates the certified head");

    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The projection trigger independently rechecks cycles when the before-insert
/// append guard is deliberately bypassed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_projection_rechecks_cycle_after_append_guard_bypass()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let mut fixture =
        dependency_plan_fixture(&pool, vec![depends_plan_arguments(prerequisite, dependent)])
            .await?;
    let corrupt_attempt = fixture.batch.authorize_next().await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;

    let trigger_error = insert_direct_dependency_event(&pool, &fixture, &corrupt_attempt)
        .await
        .expect_err("the projection trigger independently rejects the cycle");

    assert_eq!(
        trigger_error
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("session_plan_dependency_cycle")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The projection trigger rejects a new edge that reaches a corrupt
/// pre-existing cycle.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_projection_rejects_new_edge_reaching_preexisting_cycle()
-> Result<(), Box<dyn Error>> {
    const OUTSIDE_ENTRY_ORDINAL: u64 = 1;
    const FIRST_CYCLE_ENTRY_ORDINAL: u64 = 2;
    const SECOND_CYCLE_ENTRY_ORDINAL: u64 = 3;
    const FIRST_DEPENDENCY_EVENT_ORDINAL: u64 = 4;
    const CORRUPT_EVENT_ORDINAL: u64 = 5;
    const PROPOSED_EVENT_ORDINAL: u64 = 6;
    const EXPECTED_MUTATED_ROW_COUNT: u64 = 1;
    const OUTSIDE_TEXT: &str = "outside the corrupt component";
    const FIRST_CYCLE_TEXT: &str = "first corrupt component entry";
    const SECOND_CYCLE_TEXT: &str = "second corrupt component entry";
    let (container, pool, _database_url) = migrated_postgres().await?;
    let outside = PlanEntryId::try_from_u64(OUTSIDE_ENTRY_ORDINAL)
        .expect("the outside fixture identity is positive");
    let first_cycle = PlanEntryId::try_from_u64(FIRST_CYCLE_ENTRY_ORDINAL)
        .expect("the first cycle fixture identity is positive");
    let second_cycle = PlanEntryId::try_from_u64(SECOND_CYCLE_ENTRY_ORDINAL)
        .expect("the second cycle fixture identity is positive");
    let arguments = vec![
        create_plan_arguments(OUTSIDE_TEXT),
        create_plan_arguments(FIRST_CYCLE_TEXT),
        create_plan_arguments(SECOND_CYCLE_TEXT),
        depends_plan_arguments(first_cycle, second_cycle),
        depends_plan_arguments(second_cycle, first_cycle),
        depends_plan_arguments(outside, first_cycle),
    ];
    let (session, mut batch) = authorize_plan_writes(&pool, &arguments).await?;
    let repository = SessionPlanRepository::new(pool.clone());
    append_plan_write(
        &mut batch,
        &repository,
        PlanEventDraft::Create {
            text: plan_text(OUTSIDE_TEXT),
        },
    )
    .await?;
    append_plan_write(
        &mut batch,
        &repository,
        PlanEventDraft::Create {
            text: plan_text(FIRST_CYCLE_TEXT),
        },
    )
    .await?;
    append_plan_write(
        &mut batch,
        &repository,
        PlanEventDraft::Create {
            text: plan_text(SECOND_CYCLE_TEXT),
        },
    )
    .await?;
    append_plan_write(
        &mut batch,
        &repository,
        PlanEventDraft::DependsOn {
            entry: first_cycle,
            dependency: second_cycle,
        },
    )
    .await?;
    let mut fixture = DependencyPlanFixture {
        session,
        batch,
        repository,
        prerequisite: first_cycle,
        dependent: second_cycle,
    };
    let corrupt_attempt = fixture.batch.authorize_next().await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_advances_projection",
    )
    .execute(&pool)
    .await?;
    insert_direct_dependency_event_at(
        &pool,
        &fixture,
        &corrupt_attempt,
        FIRST_DEPENDENCY_EVENT_ORDINAL,
        CORRUPT_EVENT_ORDINAL,
        second_cycle,
        first_cycle,
    )
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_advances_projection",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         DISABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(&pool)
    .await?;
    let projected = sqlx::query(
        "INSERT INTO session_plan_current_dependency
            (session_id, entry_ordinal, dependency_ordinal,
             first_event_ordinal, prior_first_event_ordinal)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(session.into_uuid())
    .bind(Decimal::from(second_cycle.as_u64()))
    .bind(Decimal::from(first_cycle.as_u64()))
    .bind(Decimal::from(CORRUPT_EVENT_ORDINAL))
    .bind(Decimal::from(FIRST_DEPENDENCY_EVENT_ORDINAL))
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         ENABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_head
         DISABLE TRIGGER session_plan_head_maintenance_guard",
    )
    .execute(&pool)
    .await?;
    let advanced = sqlx::query(
        "UPDATE session_plan_head
            SET event_ordinal = $1,
                dependency_event_ordinal = $1
          WHERE session_id = $2",
    )
    .bind(Decimal::from(CORRUPT_EVENT_ORDINAL))
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_head
         ENABLE TRIGGER session_plan_head_maintenance_guard",
    )
    .execute(&pool)
    .await?;
    fixture.batch.finish(corrupt_attempt).await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    let proposed_attempt = fixture.batch.authorize_next().await?;
    let trigger_error = insert_direct_dependency_event_at(
        &pool,
        &fixture,
        &proposed_attempt,
        CORRUPT_EVENT_ORDINAL,
        PROPOSED_EVENT_ORDINAL,
        outside,
        first_cycle,
    )
    .await
    .expect_err("the projection trigger rejects the pre-existing cycle");
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;

    assert_eq!(projected.rows_affected(), EXPECTED_MUTATED_ROW_COUNT);
    assert_eq!(advanced.rows_affected(), EXPECTED_MUTATED_ROW_COUNT);
    assert_eq!(
        trigger_error
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("session_plan_dependency_graph_cycle")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The projection trigger inspects a duplicate edge against a corrupt
/// pre-existing cycle before deduplicating current relationships.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_projection_rechecks_duplicate_edge_against_preexisting_cycle()
-> Result<(), Box<dyn Error>> {
    const FIRST_CYCLE_ENTRY_ORDINAL: u64 = 1;
    const SECOND_CYCLE_ENTRY_ORDINAL: u64 = 2;
    const FIRST_DEPENDENCY_EVENT_ORDINAL: u64 = 3;
    const CORRUPT_EVENT_ORDINAL: u64 = 4;
    const DUPLICATE_EVENT_ORDINAL: u64 = 5;
    const EXPECTED_MUTATED_ROW_COUNT: u64 = 1;
    const FIRST_CYCLE_TEXT: &str = "first corrupt component entry";
    const SECOND_CYCLE_TEXT: &str = "second corrupt component entry";
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first_cycle = PlanEntryId::try_from_u64(FIRST_CYCLE_ENTRY_ORDINAL)
        .expect("the first cycle fixture identity is positive");
    let second_cycle = PlanEntryId::try_from_u64(SECOND_CYCLE_ENTRY_ORDINAL)
        .expect("the second cycle fixture identity is positive");
    let arguments = vec![
        create_plan_arguments(FIRST_CYCLE_TEXT),
        create_plan_arguments(SECOND_CYCLE_TEXT),
        depends_plan_arguments(first_cycle, second_cycle),
        depends_plan_arguments(second_cycle, first_cycle),
        depends_plan_arguments(second_cycle, first_cycle),
    ];
    let (session, mut batch) = authorize_plan_writes(&pool, &arguments).await?;
    let repository = SessionPlanRepository::new(pool.clone());
    append_plan_write(
        &mut batch,
        &repository,
        PlanEventDraft::Create {
            text: plan_text(FIRST_CYCLE_TEXT),
        },
    )
    .await?;
    append_plan_write(
        &mut batch,
        &repository,
        PlanEventDraft::Create {
            text: plan_text(SECOND_CYCLE_TEXT),
        },
    )
    .await?;
    append_plan_write(
        &mut batch,
        &repository,
        PlanEventDraft::DependsOn {
            entry: first_cycle,
            dependency: second_cycle,
        },
    )
    .await?;
    let mut fixture = DependencyPlanFixture {
        session,
        batch,
        repository,
        prerequisite: first_cycle,
        dependent: second_cycle,
    };
    let corrupt_attempt = fixture.batch.authorize_next().await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_advances_projection",
    )
    .execute(&pool)
    .await?;
    insert_direct_dependency_event_at(
        &pool,
        &fixture,
        &corrupt_attempt,
        FIRST_DEPENDENCY_EVENT_ORDINAL,
        CORRUPT_EVENT_ORDINAL,
        second_cycle,
        first_cycle,
    )
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_advances_projection",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         DISABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(&pool)
    .await?;
    let projected = sqlx::query(
        "INSERT INTO session_plan_current_dependency
            (session_id, entry_ordinal, dependency_ordinal,
             first_event_ordinal, prior_first_event_ordinal)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(session.into_uuid())
    .bind(Decimal::from(second_cycle.as_u64()))
    .bind(Decimal::from(first_cycle.as_u64()))
    .bind(Decimal::from(CORRUPT_EVENT_ORDINAL))
    .bind(Decimal::from(FIRST_DEPENDENCY_EVENT_ORDINAL))
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         ENABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_head
         DISABLE TRIGGER session_plan_head_maintenance_guard",
    )
    .execute(&pool)
    .await?;
    let advanced = sqlx::query(
        "UPDATE session_plan_head
            SET event_ordinal = $1,
                dependency_event_ordinal = $1
          WHERE session_id = $2",
    )
    .bind(Decimal::from(CORRUPT_EVENT_ORDINAL))
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_head
         ENABLE TRIGGER session_plan_head_maintenance_guard",
    )
    .execute(&pool)
    .await?;
    fixture.batch.finish(corrupt_attempt).await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    let duplicate_attempt = fixture.batch.authorize_next().await?;
    let trigger_error = insert_direct_dependency_event_at(
        &pool,
        &fixture,
        &duplicate_attempt,
        CORRUPT_EVENT_ORDINAL,
        DUPLICATE_EVENT_ORDINAL,
        second_cycle,
        first_cycle,
    )
    .await
    .expect_err("the projection trigger rejects the duplicate edge against a cycle");
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    fixture.batch.finish(duplicate_attempt).await?;

    assert_eq!(projected.rows_affected(), EXPECTED_MUTATED_ROW_COUNT);
    assert_eq!(advanced.rows_affected(), EXPECTED_MUTATED_ROW_COUNT);
    assert_eq!(
        trigger_error
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("session_plan_dependency_graph_cycle")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Complete dependency-prefix certification rejects an untrusted creation root
/// on an existing edge before append-specific target validation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_append_certification_rejects_an_untrusted_projected_root()
-> Result<(), Box<dyn Error>> {
    const ROOT_EVENT_ORDINAL: u64 = 1;
    const CORRUPTED_TEXT: &str = "rewritten without durable request authority";
    let (container, pool, _database_url) = migrated_postgres().await?;
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let mut fixture =
        dependency_plan_fixture(&pool, vec![depends_plan_arguments(dependent, prerequisite)])
            .await?;
    let append_attempt = fixture.batch.authorize_next().await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_immutable",
    )
    .execute(&pool)
    .await?;
    let corrupted = sqlx::query(
        "UPDATE session_plan_event
            SET entry_text = $1
          WHERE session_id = $2
            AND event_ordinal = $3",
    )
    .bind(CORRUPTED_TEXT)
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(ROOT_EVENT_ORDINAL))
    .execute(&pool)
    .await?;
    assert_eq!(corrupted.rows_affected(), 1);
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_immutable",
    )
    .execute(&pool)
    .await?;

    let repository_error = fixture
        .repository
        .append(PlanAppendRequest::new(
            PlanEventProvenance::from_invocation(append_attempt.correlation()),
            PlanEventDraft::DependsOn {
                entry: dependent,
                dependency: prerequisite,
            },
        ))
        .await
        .expect_err("certification rejects the untrusted projected dependency root");

    assert_eq!(
        plan_repository_error_kind(repository_error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The projection trigger independently rejects an untrusted dependency root.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_dependency_projection_rejects_an_untrusted_root() -> Result<(), Box<dyn Error>>
{
    const ROOT_EVENT_ORDINAL: u64 = 1;
    const CORRUPTED_TEXT: &str = "rewritten without durable request authority";
    let (container, pool, _database_url) = migrated_postgres().await?;
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let mut fixture =
        dependency_plan_fixture(&pool, vec![depends_plan_arguments(dependent, prerequisite)])
            .await?;
    let append_attempt = fixture.batch.authorize_next().await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_immutable",
    )
    .execute(&pool)
    .await?;
    let corrupted = sqlx::query(
        "UPDATE session_plan_event
            SET entry_text = $1
          WHERE session_id = $2
            AND event_ordinal = $3",
    )
    .bind(CORRUPTED_TEXT)
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(ROOT_EVENT_ORDINAL))
    .execute(&pool)
    .await?;
    assert_eq!(corrupted.rows_affected(), 1);
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_immutable",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    let trigger_error = insert_direct_dependency_event_between(
        &pool,
        &fixture,
        &append_attempt,
        dependent,
        prerequisite,
    )
    .await
    .expect_err("the projection trigger independently rejects the untrusted root");
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;

    assert_eq!(
        trigger_error
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("session_plan_dependency_target")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A repository append rejects a dependency event that escaped both append
/// triggers because the durable event and dependency heads no longer agree.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_append_rejects_an_uncertified_projection() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            depends_plan_arguments(prerequisite, dependent),
            depends_plan_arguments(dependent, prerequisite),
        ],
    )
    .await?;
    let corrupt_attempt = fixture.batch.authorize_next().await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_advances_projection",
    )
    .execute(&pool)
    .await?;
    insert_direct_dependency_event(&pool, &fixture, &corrupt_attempt).await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_advances_projection",
    )
    .execute(&pool)
    .await?;
    fixture.batch.finish(corrupt_attempt).await?;
    let append_attempt = fixture.batch.authorize_next().await?;

    let repository_error = fixture
        .repository
        .append(PlanAppendRequest::new(
            PlanEventProvenance::from_invocation(append_attempt.correlation()),
            PlanEventDraft::DependsOn {
                entry: fixture.dependent,
                dependency: fixture.prerequisite,
            },
        ))
        .await
        .expect_err("repository validation rejects the uncertified projection");

    assert_eq!(
        plan_repository_error_kind(repository_error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A paged read checks the projection certificate before applying its cursor,
/// so an uncertified hidden edge cannot be mistaken for bounded current truth.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_read_rejects_an_invalid_edge_reached_outside_the_page()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let invalid_dependency =
        PlanEntryId::try_from_u64(3).expect("the non-creation fixture ordinal is positive");
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![depends_plan_arguments(prerequisite, invalid_dependency)],
    )
    .await?;
    let corrupt_attempt = fixture.batch.authorize_next().await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_advances_projection",
    )
    .execute(&pool)
    .await?;
    insert_direct_dependency_event_between(
        &pool,
        &fixture,
        &corrupt_attempt,
        prerequisite,
        invalid_dependency,
    )
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_advances_projection",
    )
    .execute(&pool)
    .await?;
    fixture.batch.finish(corrupt_attempt).await?;

    let error = fixture
        .repository
        .read(PlanReadRequest::new(
            fixture.session,
            Some(prerequisite),
            None,
        ))
        .await
        .expect_err("the traversed non-creation target is corruption");

    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The current projection rejects a creation carrying dependency payload without
/// requiring the optional history projection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_read_rejects_malformed_current_creation_payload() -> Result<(), Box<dyn Error>>
{
    const CREATION_EVENT_ORDINAL: u64 = 1;
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = dependency_plan_fixture(&pool, Vec::new()).await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DROP CONSTRAINT session_plan_event_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_immutable",
    )
    .execute(&pool)
    .await?;
    let corrupted = sqlx::query(
        "UPDATE session_plan_event
            SET dependency_ordinal = $1
          WHERE session_id = $2
            AND event_ordinal = $3",
    )
    .bind(Decimal::from(fixture.dependent.as_u64()))
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(CREATION_EVENT_ORDINAL))
    .execute(&pool)
    .await?;
    assert_eq!(corrupted.rows_affected(), 1);
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_immutable",
    )
    .execute(&pool)
    .await?;

    let error = fixture
        .repository
        .read(PlanReadRequest::new(fixture.session, None, None))
        .await
        .expect_err("a creation carrying dependency payload is corruption");

    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::CurrentCreation
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A current read rejects an older dependency event whose predecessor no
/// longer forms the certified append prefix.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_read_rejects_malformed_dependency_predecessor() -> Result<(), Box<dyn Error>>
{
    const DEPENDENCY_EVENT_ORDINAL: u64 = 3;
    const MALFORMED_PRIOR_EVENT_ORDINAL: u64 = 1;
    const LATER_ENTRY_TEXT: &str = "keep the malformed edge below the head";
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture =
        dependency_plan_fixture(&pool, vec![create_plan_arguments(LATER_ENTRY_TEXT)]).await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(LATER_ENTRY_TEXT),
        },
    )
    .await?;
    let corrupted = corrupt_plan_event_predecessor(
        &pool,
        &fixture,
        DEPENDENCY_EVENT_ORDINAL,
        Some(MALFORMED_PRIOR_EVENT_ORDINAL),
    )
    .await?;

    let error = fixture
        .repository
        .read(PlanReadRequest::new(fixture.session, None, None))
        .await
        .expect_err("the current projection rejects the malformed edge predecessor");

    assert_eq!(corrupted, EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A current read authenticates the complete predecessor shape of the status
/// event from which it derives a prerequisite's readiness.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_read_rejects_malformed_dependency_status_predecessor()
-> Result<(), Box<dyn Error>> {
    const STATUS_EVENT_ORDINAL: u64 = 4;
    const MALFORMED_PRIOR_EVENT_ORDINAL: u64 = 2;
    const LATER_ENTRY_TEXT: &str = "keep the malformed status below the head";
    let (container, pool, _database_url) = migrated_postgres().await?;
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            status_plan_arguments(prerequisite, PlanStatus::Completed),
            create_plan_arguments(LATER_ENTRY_TEXT),
        ],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::SetStatus {
            entry: prerequisite,
            status: PlanStatus::Completed,
        },
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(LATER_ENTRY_TEXT),
        },
    )
    .await?;
    let corrupted = corrupt_plan_event_predecessor(
        &pool,
        &fixture,
        STATUS_EVENT_ORDINAL,
        Some(MALFORMED_PRIOR_EVENT_ORDINAL),
    )
    .await?;

    let error = fixture
        .repository
        .read(PlanReadRequest::new(fixture.session, None, None))
        .await
        .expect_err("readiness rejects the malformed status predecessor");

    assert_eq!(corrupted, EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// An included history authenticates the predecessor shape of duplicate
/// dependency events even though only the first edge event is projected.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_history_rejects_malformed_duplicate_dependency_predecessor()
-> Result<(), Box<dyn Error>> {
    const DUPLICATE_EVENT_ORDINAL: u64 = 4;
    const MALFORMED_PRIOR_EVENT_ORDINAL: u64 = 2;
    const HISTORY_LIMIT: usize = 10;
    const LATER_ENTRY_TEXT: &str = "keep the malformed duplicate below the head";
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            depends_plan_arguments(dependent, prerequisite),
            create_plan_arguments(LATER_ENTRY_TEXT),
        ],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::DependsOn {
            entry: fixture.dependent,
            dependency: fixture.prerequisite,
        },
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(LATER_ENTRY_TEXT),
        },
    )
    .await?;
    let corrupted = corrupt_plan_event_predecessor(
        &pool,
        &fixture,
        DUPLICATE_EVENT_ORDINAL,
        Some(MALFORMED_PRIOR_EVENT_ORDINAL),
    )
    .await?;

    let error = fixture
        .repository
        .read(PlanReadRequest::new(
            fixture.session,
            None,
            Some(HISTORY_LIMIT),
        ))
        .await
        .expect_err("history rejects the malformed duplicate predecessor");

    assert_eq!(corrupted, EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A dependency append rejects an older reachable edge whose predecessor no
/// longer forms the certified append prefix.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_append_rejects_malformed_reachable_dependency_predecessor()
-> Result<(), Box<dyn Error>> {
    const DEPENDENCY_EVENT_ORDINAL: u64 = 3;
    const MALFORMED_PRIOR_EVENT_ORDINAL: u64 = 1;
    const OUTSIDE_ENTRY_ORDINAL: u64 = 4;
    const OUTSIDE_ENTRY_TEXT: &str = "depend on the existing component";
    let outside = PlanEntryId::try_from_u64(OUTSIDE_ENTRY_ORDINAL)
        .expect("the outside fixture identity is positive");
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            create_plan_arguments(OUTSIDE_ENTRY_TEXT),
            depends_plan_arguments(outside, dependent),
        ],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(OUTSIDE_ENTRY_TEXT),
        },
    )
    .await?;
    let corrupted = corrupt_plan_event_predecessor(
        &pool,
        &fixture,
        DEPENDENCY_EVENT_ORDINAL,
        Some(MALFORMED_PRIOR_EVENT_ORDINAL),
    )
    .await?;
    let append_attempt = fixture.batch.authorize_next().await?;

    let error = fixture
        .repository
        .append(PlanAppendRequest::new(
            PlanEventProvenance::from_invocation(append_attempt.correlation()),
            PlanEventDraft::DependsOn {
                entry: outside,
                dependency: fixture.dependent,
            },
        ))
        .await
        .expect_err("graph loading rejects the malformed reachable predecessor");

    assert_eq!(corrupted, EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The projection trigger rejects a proposed edge that reaches an existing
/// dependency event without its original request authority.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_projection_rejects_untrusted_reachable_dependency_edge()
-> Result<(), Box<dyn Error>> {
    const DEPENDENCY_EVENT_ORDINAL: u64 = 3;
    const OUTSIDE_ENTRY_ORDINAL: u64 = 4;
    const PROPOSED_EVENT_ORDINAL: u64 = 5;
    const OUTSIDE_ENTRY_TEXT: &str = "reach the untrusted interior edge";
    let outside = PlanEntryId::try_from_u64(OUTSIDE_ENTRY_ORDINAL)
        .expect("the outside fixture identity is positive");
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            create_plan_arguments(OUTSIDE_ENTRY_TEXT),
            depends_plan_arguments(outside, dependent),
        ],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(OUTSIDE_ENTRY_TEXT),
        },
    )
    .await?;
    let mismatched_arguments = depends_plan_arguments(outside, dependent);
    let corrupted = corrupt_dependency_event_authority(
        &pool,
        fixture.session,
        DEPENDENCY_EVENT_ORDINAL,
        &mismatched_arguments,
    )
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    let proposed_attempt = fixture.batch.authorize_next().await?;
    let trigger_error = insert_direct_dependency_event_at(
        &pool,
        &fixture,
        &proposed_attempt,
        OUTSIDE_ENTRY_ORDINAL,
        PROPOSED_EVENT_ORDINAL,
        outside,
        fixture.dependent,
    )
    .await
    .expect_err("the projection trigger rejects the untrusted reachable edge");
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    fixture.batch.finish(proposed_attempt).await?;

    assert_eq!(corrupted, EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        trigger_error
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("session_plan_dependency_graph_authority")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The projection trigger authenticates creation rows for interior and leaf
/// nodes reached through the existing graph, not only the proposed endpoints.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_projection_rejects_malformed_reachable_creation() -> Result<(), Box<dyn Error>>
{
    const PREREQUISITE_EVENT_ORDINAL: u64 = 1;
    const MALFORMED_PRIOR_EVENT_ORDINAL: u64 = 2;
    const OUTSIDE_ENTRY_ORDINAL: u64 = 4;
    const PROPOSED_EVENT_ORDINAL: u64 = 5;
    const OUTSIDE_ENTRY_TEXT: &str = "reach the malformed leaf creation";
    let outside = PlanEntryId::try_from_u64(OUTSIDE_ENTRY_ORDINAL)
        .expect("the outside fixture identity is positive");
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            create_plan_arguments(OUTSIDE_ENTRY_TEXT),
            depends_plan_arguments(outside, dependent),
        ],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(OUTSIDE_ENTRY_TEXT),
        },
    )
    .await?;
    let corrupted = corrupt_plan_event_predecessor(
        &pool,
        &fixture,
        PREREQUISITE_EVENT_ORDINAL,
        Some(MALFORMED_PRIOR_EVENT_ORDINAL),
    )
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    let proposed_attempt = fixture.batch.authorize_next().await?;
    let trigger_error = insert_direct_dependency_event_at(
        &pool,
        &fixture,
        &proposed_attempt,
        OUTSIDE_ENTRY_ORDINAL,
        PROPOSED_EVENT_ORDINAL,
        outside,
        fixture.dependent,
    )
    .await
    .expect_err("the projection trigger rejects the malformed reachable creation");
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    fixture.batch.finish(proposed_attempt).await?;

    assert_eq!(corrupted, EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        trigger_error
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("session_plan_dependency_graph_shape")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The projection trigger validates the proposed dependency event's own
/// predecessor shape when the append guard and schema check are bypassed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_projection_rejects_malformed_new_dependency_predecessor()
-> Result<(), Box<dyn Error>> {
    const MALFORMED_PRIOR_EVENT_ORDINAL: u64 = 2;
    const PROPOSED_EVENT_ORDINAL: u64 = 4;
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture =
        dependency_plan_fixture(&pool, vec![depends_plan_arguments(dependent, prerequisite)])
            .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DROP CONSTRAINT session_plan_event_predecessor_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    let proposed_attempt = fixture.batch.authorize_next().await?;
    let trigger_error = insert_direct_dependency_event_at(
        &pool,
        &fixture,
        &proposed_attempt,
        MALFORMED_PRIOR_EVENT_ORDINAL,
        PROPOSED_EVENT_ORDINAL,
        fixture.dependent,
        fixture.prerequisite,
    )
    .await
    .expect_err("the projection trigger rejects the proposed malformed predecessor");
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    fixture.batch.finish(proposed_attempt).await?;

    assert_eq!(
        trigger_error
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("session_plan_dependency_shape")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The projection trigger rejects a proposed edge when a reachable node
/// already exceeds the implemented dependency bound.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_projection_rejects_reachable_over_limit_node() -> Result<(), Box<dyn Error>> {
    const OUTSIDE_ENTRY_ORDINAL: u64 = 4;
    const PROPOSED_EVENT_ORDINAL: u64 = 5;
    const SYNTHETIC_EDGE_COUNT: i64 = 32;
    const EXPECTED_INSERTED_EDGE_COUNT: u64 = 32;
    const OUTSIDE_ENTRY_TEXT: &str = "reach the over-limit component";
    let outside = PlanEntryId::try_from_u64(OUTSIDE_ENTRY_ORDINAL)
        .expect("the outside fixture identity is positive");
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            create_plan_arguments(OUTSIDE_ENTRY_TEXT),
            depends_plan_arguments(outside, dependent),
        ],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(OUTSIDE_ENTRY_TEXT),
        },
    )
    .await?;
    let inserted = insert_synthetic_dependency_projection(
        &pool,
        fixture.session,
        fixture.dependent,
        SYNTHETIC_EDGE_COUNT,
    )
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    let proposed_attempt = fixture.batch.authorize_next().await?;
    let trigger_error = insert_direct_dependency_event_at(
        &pool,
        &fixture,
        &proposed_attempt,
        OUTSIDE_ENTRY_ORDINAL,
        PROPOSED_EVENT_ORDINAL,
        outside,
        fixture.dependent,
    )
    .await
    .expect_err("the projection trigger rejects the over-limit reachable node");
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    fixture.batch.finish(proposed_attempt).await?;

    assert_eq!(inserted, EXPECTED_INSERTED_EDGE_COUNT);
    assert_eq!(
        trigger_error
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("session_plan_dependency_limit")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Complete dependency-prefix certification rejects a duplicate identity even
/// when both rows name valid dependency events in one predecessor chain.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_append_certification_rejects_duplicate_dependency_identity()
-> Result<(), Box<dyn Error>> {
    const DUPLICATE_EVENT_ORDINAL: u64 = 4;
    const LATER_ENTRY_TEXT: &str = "must not extend duplicate dependency identities";
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            depends_plan_arguments(dependent, prerequisite),
            create_plan_arguments(LATER_ENTRY_TEXT),
        ],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::DependsOn {
            entry: fixture.dependent,
            dependency: fixture.prerequisite,
        },
    )
    .await?;
    let (inserted, certified) =
        install_duplicate_dependency_projection(&pool, &fixture, DUPLICATE_EVENT_ORDINAL).await?;
    let append_attempt = fixture.batch.authorize_next().await?;

    let error = fixture
        .repository
        .append(PlanAppendRequest::new(
            PlanEventProvenance::from_invocation(append_attempt.correlation()),
            PlanEventDraft::Create {
                text: plan_text(LATER_ENTRY_TEXT),
            },
        ))
        .await
        .expect_err("a later append cannot extend duplicate dependency identities");

    assert_eq!(inserted, EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(certified, EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Complete dependency-prefix certification rejects a pre-existing cycle
/// before a non-dependency append can advance the certified head.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_append_certification_rejects_preexisting_dependency_cycle()
-> Result<(), Box<dyn Error>> {
    const FIRST_DEPENDENCY_EVENT_ORDINAL: u64 = 3;
    const CYCLE_EVENT_ORDINAL: u64 = 4;
    const LATER_ENTRY_TEXT: &str = "must not extend a cyclic dependency graph";
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            depends_plan_arguments(prerequisite, dependent),
            create_plan_arguments(LATER_ENTRY_TEXT),
        ],
    )
    .await?;
    let corrupt_attempt = fixture.batch.authorize_next().await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_advances_projection",
    )
    .execute(&pool)
    .await?;
    insert_direct_dependency_event_at(
        &pool,
        &fixture,
        &corrupt_attempt,
        FIRST_DEPENDENCY_EVENT_ORDINAL,
        CYCLE_EVENT_ORDINAL,
        prerequisite,
        dependent,
    )
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_advances_projection",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         DISABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(&pool)
    .await?;
    let projected = sqlx::query(
        "INSERT INTO session_plan_current_dependency
            (session_id, entry_ordinal, dependency_ordinal,
             first_event_ordinal, prior_first_event_ordinal)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(prerequisite.as_u64()))
    .bind(Decimal::from(dependent.as_u64()))
    .bind(Decimal::from(CYCLE_EVENT_ORDINAL))
    .bind(Decimal::from(FIRST_DEPENDENCY_EVENT_ORDINAL))
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_current_dependency
         ENABLE TRIGGER session_plan_current_dependency_immutable",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_head
         DISABLE TRIGGER session_plan_head_maintenance_guard",
    )
    .execute(&pool)
    .await?;
    let certified = sqlx::query(
        "UPDATE session_plan_head
            SET event_ordinal = $1,
                dependency_event_ordinal = $1
          WHERE session_id = $2",
    )
    .bind(Decimal::from(CYCLE_EVENT_ORDINAL))
    .bind(fixture.session.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_head
         ENABLE TRIGGER session_plan_head_maintenance_guard",
    )
    .execute(&pool)
    .await?;
    fixture.batch.finish(corrupt_attempt).await?;
    let append_attempt = fixture.batch.authorize_next().await?;

    let error = fixture
        .repository
        .append(PlanAppendRequest::new(
            PlanEventProvenance::from_invocation(append_attempt.correlation()),
            PlanEventDraft::Create {
                text: plan_text(LATER_ENTRY_TEXT),
            },
        ))
        .await
        .expect_err("a later append cannot certify a cyclic dependency graph");

    assert_eq!(projected.rows_affected(), EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(certified.rows_affected(), EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Complete dependency-prefix certification requires each edge to point to
/// its immediate chronological predecessor, not merely a covering chain.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_append_certification_rejects_reordered_dependency_chain()
-> Result<(), Box<dyn Error>> {
    const THIRD_ENTRY_ORDINAL: u64 = 4;
    const FOURTH_ENTRY_ORDINAL: u64 = 6;
    const OLDEST_DEPENDENCY_EVENT_ORDINAL: u64 = 3;
    const MIDDLE_DEPENDENCY_EVENT_ORDINAL: u64 = 5;
    const NEWEST_DEPENDENCY_EVENT_ORDINAL: u64 = 7;
    const EXPECTED_REORDERED_EDGE_COUNT: u64 = 3;
    const THIRD_ENTRY_TEXT: &str = "third entry in predecessor certification";
    const FOURTH_ENTRY_TEXT: &str = "fourth entry in predecessor certification";
    const LATER_ENTRY_TEXT: &str = "must not extend reordered dependencies";
    let third_entry = PlanEntryId::try_from_u64(THIRD_ENTRY_ORDINAL)
        .expect("the third fixture entry identity is positive");
    let fourth_entry = PlanEntryId::try_from_u64(FOURTH_ENTRY_ORDINAL)
        .expect("the fourth fixture entry identity is positive");
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            create_plan_arguments(THIRD_ENTRY_TEXT),
            depends_plan_arguments(third_entry, prerequisite),
            create_plan_arguments(FOURTH_ENTRY_TEXT),
            depends_plan_arguments(fourth_entry, prerequisite),
            create_plan_arguments(LATER_ENTRY_TEXT),
        ],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(THIRD_ENTRY_TEXT),
        },
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::DependsOn {
            entry: third_entry,
            dependency: fixture.prerequisite,
        },
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(FOURTH_ENTRY_TEXT),
        },
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::DependsOn {
            entry: fourth_entry,
            dependency: fixture.prerequisite,
        },
    )
    .await?;
    let reordered = reorder_dependency_projection_chain(
        &pool,
        fixture.session,
        OLDEST_DEPENDENCY_EVENT_ORDINAL,
        MIDDLE_DEPENDENCY_EVENT_ORDINAL,
        NEWEST_DEPENDENCY_EVENT_ORDINAL,
    )
    .await?;
    let append_attempt = fixture.batch.authorize_next().await?;

    let error = fixture
        .repository
        .append(PlanAppendRequest::new(
            PlanEventProvenance::from_invocation(append_attempt.correlation()),
            PlanEventDraft::Create {
                text: plan_text(LATER_ENTRY_TEXT),
            },
        ))
        .await
        .expect_err("a later append cannot certify a reordered dependency chain");

    assert_eq!(reordered, EXPECTED_REORDERED_EDGE_COUNT);
    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A first append fails closed when an orphan dependency projection already
/// exists without either durable events or a certifying plan head.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_first_append_rejects_orphan_dependency_projection()
-> Result<(), Box<dyn Error>> {
    const CREATED_TEXT: &str = "must not adopt an orphan dependency projection";
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (session, mut batch) =
        authorize_plan_writes(&pool, &[create_plan_arguments(CREATED_TEXT)]).await?;
    let repository = SessionPlanRepository::new(pool.clone());
    let inserted = insert_orphan_dependency_projection(&pool, session).await?;
    let append_attempt = batch.authorize_next().await?;

    let error = repository
        .append(PlanAppendRequest::new(
            PlanEventProvenance::from_invocation(append_attempt.correlation()),
            PlanEventDraft::Create {
                text: plan_text(CREATED_TEXT),
            },
        ))
        .await
        .expect_err("the first append cannot certify an orphan dependency projection");

    assert_eq!(inserted, EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A current-plan read fails closed when an orphan dependency projection
/// exists without either durable events or a certifying plan head.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_read_rejects_orphan_dependency_projection() -> Result<(), Box<dyn Error>> {
    const UNUSED_CREATED_TEXT: &str = "authorize the orphan projection fixture";
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (session, _batch) =
        authorize_plan_writes(&pool, &[create_plan_arguments(UNUSED_CREATED_TEXT)]).await?;
    let repository = SessionPlanRepository::new(pool.clone());
    let inserted = insert_orphan_dependency_projection(&pool, session).await?;

    let error = repository
        .read(PlanReadRequest::new(session, None, None))
        .await
        .expect_err("a read cannot accept an orphan dependency projection");

    assert_eq!(inserted, EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The projection trigger independently rejects duplicate physical rows when
/// its database uniqueness guard has been deliberately bypassed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_projection_rejects_duplicate_physical_dependency_edge()
-> Result<(), Box<dyn Error>> {
    const DUPLICATE_EVENT_ORDINAL: u64 = 4;
    const OUTSIDE_ENTRY_ORDINAL: u64 = 5;
    const PROPOSED_EVENT_ORDINAL: u64 = 6;
    const OUTSIDE_ENTRY_TEXT: &str = "project through duplicate physical edges";
    let outside = PlanEntryId::try_from_u64(OUTSIDE_ENTRY_ORDINAL)
        .expect("the outside fixture identity is positive");
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            depends_plan_arguments(dependent, prerequisite),
            create_plan_arguments(OUTSIDE_ENTRY_TEXT),
            depends_plan_arguments(outside, dependent),
        ],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::DependsOn {
            entry: fixture.dependent,
            dependency: fixture.prerequisite,
        },
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(OUTSIDE_ENTRY_TEXT),
        },
    )
    .await?;
    let (inserted, certified) =
        install_duplicate_dependency_projection(&pool, &fixture, DUPLICATE_EVENT_ORDINAL).await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    let proposed_attempt = fixture.batch.authorize_next().await?;
    let trigger_error = insert_direct_dependency_event_at(
        &pool,
        &fixture,
        &proposed_attempt,
        OUTSIDE_ENTRY_ORDINAL,
        PROPOSED_EVENT_ORDINAL,
        outside,
        fixture.dependent,
    )
    .await
    .expect_err("the projection trigger rejects duplicate physical dependency rows");
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    fixture.batch.finish(proposed_attempt).await?;

    assert_eq!(inserted, EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(certified, EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        trigger_error
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("session_plan_dependency_graph_shape")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A current read rejects an endpoint creation whose predecessor no longer
/// forms the certified append prefix.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_read_rejects_malformed_dependency_root_creation() -> Result<(), Box<dyn Error>>
{
    const ROOT_EVENT_ORDINAL: u64 = 1;
    const MALFORMED_PRIOR_EVENT_ORDINAL: u64 = 1;
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = dependency_plan_fixture(&pool, Vec::new()).await?;
    let corrupted = corrupt_plan_event_predecessor(
        &pool,
        &fixture,
        ROOT_EVENT_ORDINAL,
        Some(MALFORMED_PRIOR_EVENT_ORDINAL),
    )
    .await?;

    let error = fixture
        .repository
        .read(PlanReadRequest::new(fixture.session, None, None))
        .await
        .expect_err("the current projection rejects the malformed endpoint creation");

    assert_eq!(corrupted, EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A repository append rejects a dependency endpoint creation whose
/// predecessor no longer forms the certified append prefix.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_append_rejects_malformed_dependency_root_creation()
-> Result<(), Box<dyn Error>> {
    const ROOT_EVENT_ORDINAL: u64 = 1;
    const MALFORMED_PRIOR_EVENT_ORDINAL: u64 = 1;
    const OUTSIDE_ENTRY_ORDINAL: u64 = 4;
    const OUTSIDE_ENTRY_TEXT: &str = "name the malformed dependency root";
    let outside = PlanEntryId::try_from_u64(OUTSIDE_ENTRY_ORDINAL)
        .expect("the outside fixture identity is positive");
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            create_plan_arguments(OUTSIDE_ENTRY_TEXT),
            depends_plan_arguments(outside, prerequisite),
        ],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(OUTSIDE_ENTRY_TEXT),
        },
    )
    .await?;
    let corrupted = corrupt_plan_event_predecessor(
        &pool,
        &fixture,
        ROOT_EVENT_ORDINAL,
        Some(MALFORMED_PRIOR_EVENT_ORDINAL),
    )
    .await?;
    let append_attempt = fixture.batch.authorize_next().await?;

    let error = fixture
        .repository
        .append(PlanAppendRequest::new(
            PlanEventProvenance::from_invocation(append_attempt.correlation()),
            PlanEventDraft::DependsOn {
                entry: outside,
                dependency: fixture.prerequisite,
            },
        ))
        .await
        .expect_err("append target validation rejects the malformed dependency root");

    assert_eq!(corrupted, EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The projection trigger rejects an entry endpoint creation whose predecessor
/// no longer forms the certified append prefix.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_projection_rejects_malformed_entry_root_creation()
-> Result<(), Box<dyn Error>> {
    const ENTRY_EVENT_ORDINAL: u64 = 2;
    const DEPENDENCY_EVENT_ORDINAL: u64 = 3;
    const PROPOSED_EVENT_ORDINAL: u64 = 4;
    let dependent =
        PlanEntryId::try_from_u64(2).expect("the dependent fixture identity is positive");
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture =
        dependency_plan_fixture(&pool, vec![depends_plan_arguments(dependent, prerequisite)])
            .await?;
    let corrupted =
        corrupt_plan_event_predecessor(&pool, &fixture, ENTRY_EVENT_ORDINAL, None).await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    let proposed_attempt = fixture.batch.authorize_next().await?;
    let trigger_error = insert_direct_dependency_event_at(
        &pool,
        &fixture,
        &proposed_attempt,
        DEPENDENCY_EVENT_ORDINAL,
        PROPOSED_EVENT_ORDINAL,
        fixture.dependent,
        fixture.prerequisite,
    )
    .await
    .expect_err("the projection trigger rejects the malformed entry root");
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    fixture.batch.finish(proposed_attempt).await?;

    assert_eq!(corrupted, EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        trigger_error
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("session_plan_dependency_entry")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Head certification prevents a later append from extending a malformed
/// dependency event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_append_rejects_malformed_dependency_head() -> Result<(), Box<dyn Error>> {
    const LATER_ENTRY_TEXT: &str = "must not append after malformed history";
    const DEPENDENCY_EVENT_ORDINAL: u64 = 3;
    const MALFORMED_EDGE_TEXT: &str = "dependency event must not carry text";
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture =
        dependency_plan_fixture(&pool, vec![create_plan_arguments(LATER_ENTRY_TEXT)]).await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DROP CONSTRAINT session_plan_event_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_immutable",
    )
    .execute(&pool)
    .await?;
    let corrupted = sqlx::query(
        "UPDATE session_plan_event
            SET entry_text = $1
          WHERE session_id = $2
            AND event_ordinal = $3",
    )
    .bind(MALFORMED_EDGE_TEXT)
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(DEPENDENCY_EVENT_ORDINAL))
    .execute(&pool)
    .await?;
    assert_eq!(corrupted.rows_affected(), EXPECTED_PLAN_MUTATED_ROW_COUNT);
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_immutable",
    )
    .execute(&pool)
    .await?;

    let append_attempt = fixture.batch.authorize_next().await?;
    let append_error = fixture
        .repository
        .append(PlanAppendRequest::new(
            PlanEventProvenance::from_invocation(append_attempt.correlation()),
            PlanEventDraft::Create {
                text: plan_text(LATER_ENTRY_TEXT),
            },
        ))
        .await
        .expect_err("a later append cannot extend the malformed dependency head");

    assert_eq!(
        plan_repository_error_kind(append_error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A current-plan read rejects forbidden payload on its certified dependency
/// head without requiring history.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_read_rejects_malformed_dependency_head() -> Result<(), Box<dyn Error>> {
    const DEPENDENCY_EVENT_ORDINAL: u64 = 3;
    const MALFORMED_EDGE_TEXT: &str = "dependency event must not carry text";
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = dependency_plan_fixture(&pool, Vec::new()).await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DROP CONSTRAINT session_plan_event_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_immutable",
    )
    .execute(&pool)
    .await?;
    let corrupted = sqlx::query(
        "UPDATE session_plan_event
            SET entry_text = $1
          WHERE session_id = $2
            AND event_ordinal = $3",
    )
    .bind(MALFORMED_EDGE_TEXT)
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(DEPENDENCY_EVENT_ORDINAL))
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_immutable",
    )
    .execute(&pool)
    .await?;

    let error = fixture
        .repository
        .read(PlanReadRequest::new(fixture.session, None, None))
        .await
        .expect_err("a dependency edge carrying text is corruption");

    assert_eq!(corrupted.rows_affected(), EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A cursor-hidden prerequisite status event must retain the complete certified
/// shape before the repository derives dependency readiness from it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_read_rejects_malformed_hidden_dependency_status() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![status_plan_arguments(prerequisite, PlanStatus::Completed)],
    )
    .await?;
    let corrupt_attempt = fixture.batch.authorize_next().await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DROP CONSTRAINT session_plan_event_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_append_guard",
    )
    .execute(&pool)
    .await?;
    insert_direct_malformed_status_event(&pool, &fixture, &corrupt_attempt).await?;
    fixture.batch.finish(corrupt_attempt).await?;

    let error = fixture
        .repository
        .read(PlanReadRequest::new(
            fixture.session,
            Some(prerequisite),
            None,
        ))
        .await
        .expect_err("the hidden malformed status payload is corruption");

    assert_eq!(
        plan_repository_error_kind(error),
        PlanRepositoryErrorKind::EventSequence
    );

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
        PlanRepositoryErrorKind::AppendProvenance
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
        PlanRepositoryErrorKind::EventSequence
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
        PlanRepositoryErrorKind::AppendProvenance
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
        PlanRepositoryErrorKind::EventSequence
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

/// Head certification independently authenticates an older dependency tip
/// after a valid non-dependency event becomes the session event head.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_append_rejects_malformed_distinct_dependency_head()
-> Result<(), Box<dyn Error>> {
    const FIRST_LATER_ENTRY_TEXT: &str = "advance beyond the dependency tip";
    const SECOND_LATER_ENTRY_TEXT: &str = "must not extend a malformed dependency tip";
    const DEPENDENCY_EVENT_ORDINAL: u64 = 3;
    const MALFORMED_EDGE_TEXT: &str = "dependency event must remain shape-valid";
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            create_plan_arguments(FIRST_LATER_ENTRY_TEXT),
            create_plan_arguments(SECOND_LATER_ENTRY_TEXT),
        ],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(FIRST_LATER_ENTRY_TEXT),
        },
    )
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DROP CONSTRAINT session_plan_event_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_immutable",
    )
    .execute(&pool)
    .await?;
    let corrupted = sqlx::query(
        "UPDATE session_plan_event
            SET entry_text = $1
          WHERE session_id = $2
            AND event_ordinal = $3",
    )
    .bind(MALFORMED_EDGE_TEXT)
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(DEPENDENCY_EVENT_ORDINAL))
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_immutable",
    )
    .execute(&pool)
    .await?;
    let append_attempt = fixture.batch.authorize_next().await?;

    let append_error = fixture
        .repository
        .append(PlanAppendRequest::new(
            PlanEventProvenance::from_invocation(append_attempt.correlation()),
            PlanEventDraft::Create {
                text: plan_text(SECOND_LATER_ENTRY_TEXT),
            },
        ))
        .await
        .expect_err("a later event cannot hide a malformed dependency tip");

    assert_eq!(corrupted.rows_affected(), EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        plan_repository_error_kind(append_error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Complete dependency-prefix certification rejects malformed non-tip edges
/// before a later non-dependency append can extend the event history.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_append_rejects_malformed_non_tip_dependency_edge()
-> Result<(), Box<dyn Error>> {
    const SECOND_DEPENDENT_TEXT: &str = "create a later dependency edge";
    const LATER_ENTRY_TEXT: &str = "must not extend a malformed older edge";
    const SECOND_DEPENDENT_ORDINAL: u64 = 4;
    const OLDER_DEPENDENCY_EVENT_ORDINAL: u64 = 3;
    const MALFORMED_EDGE_TEXT: &str = "older dependency event must remain valid";
    let second_dependent = PlanEntryId::try_from_u64(SECOND_DEPENDENT_ORDINAL)
        .expect("the second dependent fixture identity is positive");
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            create_plan_arguments(SECOND_DEPENDENT_TEXT),
            depends_plan_arguments(second_dependent, prerequisite),
            create_plan_arguments(LATER_ENTRY_TEXT),
        ],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(SECOND_DEPENDENT_TEXT),
        },
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::DependsOn {
            entry: second_dependent,
            dependency: fixture.prerequisite,
        },
    )
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DROP CONSTRAINT session_plan_event_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         DISABLE TRIGGER session_plan_event_immutable",
    )
    .execute(&pool)
    .await?;
    let corrupted = sqlx::query(
        "UPDATE session_plan_event
            SET entry_text = $1
          WHERE session_id = $2
            AND event_ordinal = $3",
    )
    .bind(MALFORMED_EDGE_TEXT)
    .bind(fixture.session.into_uuid())
    .bind(Decimal::from(OLDER_DEPENDENCY_EVENT_ORDINAL))
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_plan_event
         ENABLE TRIGGER session_plan_event_immutable",
    )
    .execute(&pool)
    .await?;
    let append_attempt = fixture.batch.authorize_next().await?;

    let append_error = fixture
        .repository
        .append(PlanAppendRequest::new(
            PlanEventProvenance::from_invocation(append_attempt.correlation()),
            PlanEventDraft::Create {
                text: plan_text(LATER_ENTRY_TEXT),
            },
        ))
        .await
        .expect_err("a later append cannot hide a malformed non-tip dependency edge");

    assert_eq!(corrupted.rows_affected(), EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        plan_repository_error_kind(append_error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Complete dependency-prefix certification terminates and rejects a repeated
/// predecessor after storage defenses are deliberately bypassed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_append_rejects_cyclic_dependency_predecessor_chain()
-> Result<(), Box<dyn Error>> {
    const SECOND_DEPENDENT_TEXT: &str = "create a cyclic dependency tip";
    const LATER_ENTRY_TEXT: &str = "must not extend a cyclic dependency chain";
    const SECOND_DEPENDENT_ORDINAL: u64 = 4;
    const LATER_DEPENDENCY_EVENT_ORDINAL: u64 = 5;
    let second_dependent = PlanEntryId::try_from_u64(SECOND_DEPENDENT_ORDINAL)
        .expect("the second dependent fixture identity is positive");
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            create_plan_arguments(SECOND_DEPENDENT_TEXT),
            depends_plan_arguments(second_dependent, prerequisite),
            create_plan_arguments(LATER_ENTRY_TEXT),
        ],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(SECOND_DEPENDENT_TEXT),
        },
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::DependsOn {
            entry: second_dependent,
            dependency: fixture.prerequisite,
        },
    )
    .await?;
    let corrupted = corrupt_dependency_projection_predecessor(
        &pool,
        fixture.session,
        LATER_DEPENDENCY_EVENT_ORDINAL,
        LATER_DEPENDENCY_EVENT_ORDINAL,
    )
    .await?;
    let append_attempt = fixture.batch.authorize_next().await?;

    let append_result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.repository.append(PlanAppendRequest::new(
            PlanEventProvenance::from_invocation(append_attempt.correlation()),
            PlanEventDraft::Create {
                text: plan_text(LATER_ENTRY_TEXT),
            },
        )),
    )
    .await
    .expect("cyclic predecessor certification terminates");
    let append_error = append_result
        .expect_err("a later append cannot extend a cyclic dependency predecessor chain");

    assert_eq!(corrupted, EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        plan_repository_error_kind(append_error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Complete dependency-prefix certification authenticates the endpoint
/// creations on every edge, including roots beneath a valid later tip.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_plan_append_rejects_malformed_non_tip_dependency_root()
-> Result<(), Box<dyn Error>> {
    const SECOND_DEPENDENT_TEXT: &str = "create a later dependency root";
    const LATER_ENTRY_TEXT: &str = "must not extend a malformed older root";
    const SECOND_DEPENDENT_ORDINAL: u64 = 4;
    const OLDER_ENTRY_ROOT_ORDINAL: u64 = 2;
    let second_dependent = PlanEntryId::try_from_u64(SECOND_DEPENDENT_ORDINAL)
        .expect("the second dependent fixture identity is positive");
    let prerequisite =
        PlanEntryId::try_from_u64(1).expect("the prerequisite fixture identity is positive");
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut fixture = dependency_plan_fixture(
        &pool,
        vec![
            create_plan_arguments(SECOND_DEPENDENT_TEXT),
            depends_plan_arguments(second_dependent, prerequisite),
            create_plan_arguments(LATER_ENTRY_TEXT),
        ],
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::Create {
            text: plan_text(SECOND_DEPENDENT_TEXT),
        },
    )
    .await?;
    append_plan_write(
        &mut fixture.batch,
        &fixture.repository,
        PlanEventDraft::DependsOn {
            entry: second_dependent,
            dependency: fixture.prerequisite,
        },
    )
    .await?;
    let corrupted =
        corrupt_plan_event_predecessor(&pool, &fixture, OLDER_ENTRY_ROOT_ORDINAL, None).await?;
    let append_attempt = fixture.batch.authorize_next().await?;

    let append_error = fixture
        .repository
        .append(PlanAppendRequest::new(
            PlanEventProvenance::from_invocation(append_attempt.correlation()),
            PlanEventDraft::Create {
                text: plan_text(LATER_ENTRY_TEXT),
            },
        ))
        .await
        .expect_err("a later append cannot hide a malformed non-tip dependency root");

    assert_eq!(corrupted, EXPECTED_PLAN_MUTATED_ROW_COUNT);
    assert_eq!(
        plan_repository_error_kind(append_error),
        PlanRepositoryErrorKind::EventSequence
    );

    pool.close().await;
    drop(container);
    Ok(())
}
