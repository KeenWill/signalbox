//! Store load coverage.

use super::*;

/// One genuine constraint rejection for the assertion-helper tests below: the
/// enrollment guard trigger rejects an insert that does not begin active at
/// revision one with the same SQLSTATE the concurrency races produce.
pub(crate) async fn stored_check_violation(pool: &PgPool) -> RunnerProtocolStoreError {
    RunnerProtocolStoreError::Database(
        sqlx::query(
            "INSERT INTO runner_enrollment
                (enrollment_id, runner_id, authentication_reference_id,
                 allowed_class_count, revision, state_kind)
             VALUES ($1, $2, $3, 0, 2, 'revoked')",
        )
        .bind(uuid(LATER_ENROLLMENT))
        .bind(uuid(LATER_RUNNER))
        .bind(uuid(LATER_AUTHENTICATION))
        .execute(pool)
        .await
        .expect_err("an enrollment inserted as already revoked violates the guard"),
    )
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn one_conflict_assertion_accepts_either_winning_order() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let second_loses = stored_check_violation(&pool).await;
    let first_loses = stored_check_violation(&pool).await;

    assert_one_store_succeeds_and_one_conflicts(Ok(()), Err(second_loses));
    assert_one_store_succeeds_and_one_conflicts(Err(first_loses), Ok(()));
    drop(pool);
    Ok(())
}

#[test]
#[should_panic(expected = "one attempt binding must win exactly once")]
fn one_conflict_assertion_rejects_two_successes() {
    assert_one_store_succeeds_and_one_conflicts(Ok(()), Ok(()));
}

#[test]
#[should_panic(expected = "one attempt binding must win exactly once")]
fn one_conflict_assertion_rejects_two_rejections() {
    assert_one_store_succeeds_and_one_conflicts(
        Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::InvalidState,
        )),
        Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::InvalidState,
        )),
    );
}

#[test]
#[should_panic(expected = "PostgreSQL must reject the invalid durable evidence")]
fn one_conflict_assertion_rejects_a_non_constraint_rejection() {
    assert_one_store_succeeds_and_one_conflicts(
        Ok(()),
        Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::InvalidState,
        )),
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn store_rejects_oversized_repository_inventory_before_write() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let repositories = (0..=RunnerAdvertisement::MAX_REPOSITORIES).map(|index| {
        RunnerRepositoryEntry::new(
            WorkspaceRepositoryKey::try_new(format!("repository_{index}"))
                .expect("the generated repository key is valid"),
            None,
        )
    });
    let oversized = RunnerAdvertisement::new([class()], [], [], [], [], repositories);
    let error = store
        .register(&expected_enrollment, oversized)
        .await
        .expect_err("the persistence boundary rejects the oversized inventory");
    let RunnerProtocolStoreError::Domain(actual) = error else {
        panic!("the oversized inventory must fail at the domain boundary");
    };
    let durable_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM runner_registration
          WHERE enrollment_id = $1",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(actual, RunnerDomainError::TooManyAdvertisedRepositories);
    assert_eq!(durable_count, 0);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn orphan_physical_attempt_binding_cannot_commit() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, LATER_LEASE_PHYSICAL_ATTEMPT).await?;
    let orphan = sqlx::query(
        "INSERT INTO runner_physical_attempt_lease_binding
            (attempt_id, lease_id)
         VALUES ($1, $2)",
    )
    .bind(uuid(LATER_LEASE_PHYSICAL_ATTEMPT.attempt))
    .bind(uuid(LEASE + 99))
    .execute(&pool)
    .await
    .expect_err("a physical attempt binding must install its matching lease lineage");

    assert_check_violation(orphan);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn adapter_rejects_caller_reconstituted_no_execution_proof() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let correlation = pin.lease.correlation();
    let credential_authorization = pin.lease.credential_authorization().cloned();
    let reconstructed = RunnerLease::reconstitute(
        RunnerLeaseReconstitutionInput {
            lease: correlation.lease,
            dispatch: correlation.dispatch,
            runner: correlation.runner,
            tool: correlation.tool.clone(),
            effect: pin.lease.effect(),
            credential_authorization: credential_authorization.clone(),
            generation: correlation.generation,
            state: signalbox_domain::RunnerLeaseState::LostUnclaimed,
            recorded_correlation: correlation.clone(),
            recorded_session: correlation.dispatch.session(),
            recorded_effect: pin.lease.effect(),
            recorded_credential_authorization: credential_authorization,
            recorded_state: signalbox_domain::RunnerLeaseState::LostUnclaimed,
            retry_preparation: RunnerLeaseRetryPreparation::Available,
        },
        registration.registration(),
    )
    .expect("the caller-controlled loss facts are internally correlated");
    let forged = reconstructed
        .into_reconstituted_loss(Some(correlation), RunnerLeaseRetryPreparation::Available)
        .expect("the copied correlation fabricates process-local proof");
    let rejected = store
        .store_lease_loss(&forged)
        .await
        .expect_err("caller-reconstituted facts cannot originate durable no-execution proof");

    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn reconstitution_requires_trusted_catalog_declarations() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let stored = store
        .register(&expected_enrollment, advertisement())
        .await?;
    sqlx::query("ALTER TABLE runner_registration_tool DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_registration_tool
            SET model_input_schema = $4
          WHERE enrollment_id = $1
            AND registration_revision = $2
            AND tool_name = $3",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(Decimal::from(stored.revision().get()))
    .bind(tool("inspect").as_str())
    .bind(r#"{"different":0}"#)
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_registration_tool ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let error = store
        .load_registration(&expected_enrollment, stored.revision())
        .await
        .expect_err("stored declarations cannot bootstrap their own catalog authority");

    assert_store_domain_error(error, RunnerDomainError::CorruptStoredFacts);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn reconstitution_rejects_noncanonical_tool_schema() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let stored = store
        .register(&expected_enrollment, advertisement())
        .await?;
    sqlx::query("ALTER TABLE runner_registration_tool DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let rejected = sqlx::query(
        "UPDATE runner_registration_tool
            SET model_input_schema = '{ \"x\" : 0 }'
          WHERE enrollment_id = $1
            AND registration_revision = $2
            AND tool_name = $3",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(Decimal::from(stored.revision().get()))
    .bind(tool("inspect").as_str())
    .execute(&pool)
    .await
    .expect_err("noncanonical schema text is rejected at the durable boundary");
    sqlx::query("ALTER TABLE runner_registration_tool ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}
