//! Plan writes and dependency projections.

use crate::*;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ConcurrentPlanAppendDisposition {
    Appended,
    DuplicateAttempt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlanRepositoryErrorKind {
    AppendProvenance,
    CurrentCreation,
    DependencyStatus,
    EventSequence,
    UntrustedProvenance,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct PlanStorageSnapshot {
    pub(crate) event_count: i64,
    pub(crate) head_ordinal: Decimal,
}

pub(crate) static NEXT_PLAN_FIXTURE_SEED: AtomicU64 = AtomicU64::new(0xd100);
pub(crate) const PLAN_FIXTURE_SEED_STRIDE: u64 = 0x200;

pub(crate) fn plan_text(value: &str) -> PlanText {
    PlanText::try_new(String::from(value)).expect("the plan text fixture is valid")
}

pub(crate) fn create_plan_arguments(text: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "kind": "create",
        "text": text,
    }))
    .expect("the plan create arguments fixture serializes")
}

pub(crate) fn revise_plan_arguments(entry: PlanEntryId, text: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "entry_id": entry.as_u64(),
        "kind": "revise",
        "text": text,
    }))
    .expect("the plan revision arguments fixture serializes")
}

pub(crate) fn status_plan_arguments(entry: PlanEntryId, status: PlanStatus) -> String {
    serde_json::to_string(&serde_json::json!({
        "entry_id": entry.as_u64(),
        "kind": "set_status",
        "status": status,
    }))
    .expect("the plan status arguments fixture serializes")
}

pub(crate) fn depends_plan_arguments(entry: PlanEntryId, dependency: PlanEntryId) -> String {
    serde_json::to_string(&serde_json::json!({
        "dependency_id": dependency.as_u64(),
        "entry_id": entry.as_u64(),
        "kind": "depends_on",
    }))
    .expect("the plan dependency arguments fixture serializes")
}

pub(crate) async fn authorize_plan_write(
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

pub(crate) struct AuthorizedPlanWriteBatch {
    pub(crate) session: SessionId,
    pub(crate) turn: TurnId,
    pub(crate) next_attempt_seed: u128,
    pub(crate) repository: PostgresToolLoopRepository,
}

impl AuthorizedPlanWriteBatch {
    pub(crate) async fn authorize_next(&mut self) -> Result<ToolDispatchAuthority, Box<dyn Error>> {
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

    pub(crate) async fn finish(
        &self,
        authorized: ToolDispatchAuthority,
    ) -> Result<(), Box<dyn Error>> {
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

pub(crate) async fn authorize_plan_writes(
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

pub(crate) async fn append_plan_write(
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

pub(crate) fn expect_appended(outcome: PlanAppendOutcome) -> PlanEvent {
    match outcome {
        PlanAppendOutcome::Appended(event) => event,
        PlanAppendOutcome::Rejected(rejection) => {
            panic!("the plan append fixture was unexpectedly rejected: {rejection:?}")
        }
    }
}

pub(crate) fn expect_dependency_cycle(outcome: PlanAppendOutcome) -> PlanDependencyCycle {
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

pub(crate) fn plan_repository_error_kind(
    error: SessionPlanRepositoryError,
) -> PlanRepositoryErrorKind {
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

pub(crate) fn concurrent_append_disposition(
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

pub(crate) const DEPENDENCY_PREREQUISITE_TEXT: &str = "finish the durable base";
pub(crate) const DEPENDENCY_DEPENDENT_TEXT: &str = "ship dependent work";
pub(crate) const EXPECTED_PLAN_MUTATED_ROW_COUNT: u64 = 1;
pub(crate) const SYNTHETIC_DEPENDENCY_ORDINAL_BASE: i64 = 100;
pub(crate) const SYNTHETIC_EVENT_ORDINAL_BASE: i64 = 200;

pub(crate) struct DependencyPlanFixture {
    pub(crate) session: SessionId,
    pub(crate) batch: AuthorizedPlanWriteBatch,
    pub(crate) repository: SessionPlanRepository,
    pub(crate) prerequisite: PlanEntryId,
    pub(crate) dependent: PlanEntryId,
}

pub(crate) async fn dependency_plan_fixture(
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

pub(crate) async fn insert_direct_dependency_event(
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

pub(crate) async fn insert_direct_dependency_event_between(
    pool: &PgPool,
    fixture: &DependencyPlanFixture,
    authorized: &ToolDispatchAuthority,
    entry: PlanEntryId,
    dependency: PlanEntryId,
) -> Result<(), sqlx::Error> {
    insert_direct_dependency_event_at(pool, fixture, authorized, 3, 4, entry, dependency).await
}

pub(crate) async fn insert_direct_dependency_event_at(
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

pub(crate) async fn corrupt_plan_event_predecessor(
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

pub(crate) async fn corrupt_dependency_event_authority(
    pool: &PgPool,
    session: SessionId,
    event_ordinal: u64,
    mismatched_arguments: &str,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "ALTER TABLE tool_request
         DISABLE TRIGGER tool_request_resolution_guard",
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
         ENABLE TRIGGER tool_request_resolution_guard",
    )
    .execute(pool)
    .await?;
    Ok(corrupted.rows_affected())
}

pub(crate) async fn corrupt_dependency_projection_predecessor(
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

pub(crate) async fn insert_synthetic_dependency_projection(
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

pub(crate) async fn install_duplicate_dependency_projection(
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

pub(crate) async fn reorder_dependency_projection_chain(
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

pub(crate) async fn insert_orphan_dependency_projection(
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

pub(crate) async fn insert_direct_malformed_status_event(
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

pub(crate) async fn insert_dependency_without_target(
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

pub(crate) fn dependency_edge(event: &PlanEvent) -> (PlanEntryId, PlanEntryId) {
    match event.kind() {
        PlanEventKind::DependsOn { entry, dependency } => (*entry, *dependency),
        PlanEventKind::Created { .. }
        | PlanEventKind::TextRevised { .. }
        | PlanEventKind::StatusChanged { .. } => {
            panic!("fixture event is not a dependency edge")
        }
    }
}
