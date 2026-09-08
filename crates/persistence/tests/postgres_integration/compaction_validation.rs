//! Behavioral checks for malformed compaction evidence and cyclic ancestry.

use crate::*;

/// Records one or two valid compactions over a completed fixture turn.
async fn completed_compactions(
    count: u64,
) -> Result<(ContainerAsync<Postgres>, PgPool, ContextCompactionId), Box<dyn Error>> {
    const FIXTURE_SEED: u128 = 0x134900;
    let (container, pool, _) = migrated_postgres().await?;
    let (fixture, repository, authorized) =
        authorize_checkpointed_model_call(&pool, FIXTURE_SEED).await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Completed {
            assistant_text: vec![
                AssistantText::try_new(String::from("compaction fixture response"))
                    .expect("nonempty response"),
            ],
        });
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::now_v7())],
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            )),
            |_| panic!("the fixture has no pending steering"),
        )
        .await?;
    let repository = ContextCompactionRepository::new(pool.clone());
    let mut last = None;
    for through in 1..=count {
        let PrepareContextCompactionOutcome::Prepared(prepared) = repository
            .prepare(PrepareContextCompactionRequest {
                command: DurableCommandId::from_uuid(Uuid::now_v7()),
                session: fixture.session,
                requested_through_position: Some(through),
                automatic_for_turn: None,
                defaults_version: SessionConfigurationDefaultsVersion::first(),
                selection: DirectModelSelection::from_uuid(Uuid::from_u128(FIXTURE_SEED + 5)),
                target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                    Uuid::from_u128(FIXTURE_SEED + 6),
                )),
                input_includes_cache_tokens: false,
                credential_reference: String::from("compaction-fixture"),
                call: ModelCallId::from_uuid(Uuid::now_v7()),
                compaction: ContextCompactionId::from_uuid(Uuid::now_v7()),
                summary_entry: SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                result_frontier: ContextFrontierId::from_uuid(Uuid::now_v7()),
            })
            .await?
        else {
            panic!("the completed fixture has a compactable frontier");
        };
        repository.authorize(&prepared).await?;
        repository
            .complete(
                &prepared,
                "compacted fixture summary",
                ContextCompactionTokenUsage::unreported(),
            )
            .await?;
        last = Some(prepared.compaction());
    }
    Ok((
        container,
        pool,
        last.expect("the fixture requests at least one compaction"),
    ))
}

/// Runs the production evidence trigger independently of the already-committed
/// record's immutability trigger, preserving its deferred commit boundary.
async fn create_compaction_probe(connection: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        "CREATE TEMP TABLE compaction_probe (LIKE context_compaction) ON COMMIT DROP;
         CREATE CONSTRAINT TRIGGER compaction_probe_requires_evidence
         AFTER INSERT ON compaction_probe DEFERRABLE INITIALLY DEFERRED
         FOR EACH ROW EXECUTE FUNCTION require_context_compaction_exact_evidence();
         SET LOCAL statement_timeout = '10s';",
    )
    .execute(connection)
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn compaction_result_missing_its_summary_cannot_commit() -> Result<(), Box<dyn Error>> {
    let (_container, pool, compaction) = completed_compactions(1).await?;
    const OTHER_SOURCE_SEED: u128 = 0x134a00;
    let (other, _, _) = authorize_checkpointed_model_call(&pool, OTHER_SOURCE_SEED).await?;
    let alternate = Uuid::now_v7();
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO context_frontier (owning_session_id, context_frontier_id, member_count, prefix_context_frontier_id)
         SELECT compaction.session_id, $2, source.member_count + 1, source.context_frontier_id
           FROM context_compaction AS compaction JOIN context_frontier AS source
             ON source.owning_session_id = compaction.session_id AND source.context_frontier_id = compaction.source_frontier_id
          WHERE compaction.context_compaction_id = $1",
    ).bind(compaction.into_uuid()).bind(alternate).execute(&mut *transaction).await?;
    sqlx::query(
        "INSERT INTO context_frontier_delta (owning_session_id, context_frontier_id, member_position, source_session_id, semantic_entry_id)
         SELECT owning_session_id, context_frontier_id, member_count, $2, $3
           FROM context_frontier WHERE context_frontier_id = $1",
    ).bind(alternate).bind(other.session.into_uuid()).bind(Uuid::from_u128(OTHER_SOURCE_SEED + 10))
        .execute(&mut *transaction).await?;
    create_compaction_probe(&mut transaction).await?;
    sqlx::query(
        "INSERT INTO compaction_probe
         SELECT context_compaction_id, session_id, predecessor_compaction_id,
                source_frontier_id, $2, producing_call_id, first_source_session_id,
                first_entry_id, through_source_session_id, through_entry_id, summary_entry_id, applied_at
           FROM context_compaction WHERE context_compaction_id = $1",
    )
    .bind(compaction.into_uuid())
    .bind(alternate)
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a different appended member cannot stand in for the summary");
    let database = error
        .as_database_error()
        .expect("compaction evidence error");
    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(
        database.message(),
        "compaction result must be the source plus its summary"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn compaction_member_lookup_terminates_on_a_missing_member_in_cyclic_prefixes()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _) = migrated_postgres().await?;
    let session = insert_outbox_session_fixture(&pool, 0x134b00).await?;
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    let absent = Uuid::now_v7();
    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '10s'")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO context_frontier (owning_session_id, context_frontier_id, member_count, prefix_context_frontier_id)
         VALUES ($1, $2, 0, $3), ($1, $3, 0, $2)",
    ).bind(session).bind(first).bind(second).execute(&mut *transaction).await?;
    let position: Option<Decimal> =
        sqlx::query_scalar("SELECT context_frontier_member_position($1, $2, $1, $3)")
            .bind(session)
            .bind(first)
            .bind(absent)
            .fetch_one(&mut *transaction)
            .await?;
    assert_eq!(position, None);
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn compaction_predecessor_cycle_is_rejected_at_commit() -> Result<(), Box<dyn Error>> {
    let (_container, pool, compaction) = completed_compactions(2).await?;
    let mut transaction = pool.begin().await?;
    // Supply malformed stored ancestry while leaving the new successor's
    // dedicated call, summary, source and result evidence intact.
    sqlx::query("ALTER TABLE context_compaction DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "UPDATE context_compaction SET predecessor_compaction_id = $1
          WHERE context_compaction_id = (SELECT predecessor_compaction_id FROM context_compaction WHERE context_compaction_id = $1)",
    ).bind(compaction.into_uuid()).execute(&mut *transaction).await?;
    sqlx::query("ALTER TABLE context_compaction ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    create_compaction_probe(&mut transaction).await?;
    sqlx::query("INSERT INTO compaction_probe SELECT * FROM context_compaction WHERE context_compaction_id = $1")
        .bind(compaction.into_uuid()).execute(&mut *transaction).await?;
    let error = transaction
        .commit()
        .await
        .expect_err("cyclic predecessor evidence cannot authorize a compaction");
    let database = error
        .as_database_error()
        .expect("compaction evidence error");
    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(
        database.message(),
        "compaction predecessor result and visible start must match"
    );
    Ok(())
}
