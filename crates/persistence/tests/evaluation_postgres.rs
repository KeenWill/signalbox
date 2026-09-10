//! Sealed evaluation transaction and replay guarantees.

use signalbox_domain::{
    DeliveryKind, EffectRequest, InlineFramePayload, ProgramCapability, ProgramRegistrationId,
    ProgramRunId, RequestKind,
    evaluation::{EvaluationOutcome, EvaluationSnapshot, EvaluationTrial},
    program_registration::{ProgramGrants, ProgramRegistrationRequest},
};
use signalbox_persistence::{
    evaluation::{EvaluationError, EvaluationRepository},
    program_journal::ProgramJournalRepository,
    program_registration::ProgramRegistrationRepository,
    test_support::postgres::{TestDatabase, migrated_postgres},
};
use sqlx::{PgPool, types::Json};
use std::error::Error;

struct Fixture {
    _database: TestDatabase,
    pool: PgPool,
    repository: EvaluationRepository,
    snapshot: EvaluationSnapshot,
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn empty_schema_migrations_leave_only_general_evaluation_recording()
-> Result<(), Box<dyn Error>> {
    let (_database, pool, _) = migrated_postgres(4).await?;
    let remaining: Vec<String> = sqlx::query_scalar("SELECT relname FROM pg_class WHERE relname IN ('approval_judge_eval_run', 'approval_judge_eval_call')").fetch_all(&pool).await?;
    assert!(
        remaining.is_empty(),
        "temporary tables remain: {remaining:?}"
    );
    let functions: Vec<String> = sqlx::query_scalar("SELECT proname FROM pg_proc WHERE proname IN ('reject_eval_call_outside_run_recording', 'stamp_eval_run_recording_transaction')").fetch_all(&pool).await?;
    assert!(
        functions.is_empty(),
        "temporary functions remain: {functions:?}"
    );
    sqlx::query("SELECT run_id FROM evaluation_run LIMIT 0")
        .execute(&pool)
        .await?;
    sqlx::query("SELECT run_id FROM evaluation_trial LIMIT 0")
        .execute(&pool)
        .await?;
    Ok(())
}

impl Fixture {
    async fn new() -> Result<Self, Box<dyn Error>> {
        let (database, pool, _) = migrated_postgres(4).await?;
        let registrations = ProgramRegistrationRepository::new(pool.clone());
        let registration = registrations
            .register_user(
                ProgramRegistrationId::from_uuid(uuid::Uuid::now_v7()),
                ProgramRegistrationRequest {
                    name: "synthetic-evaluation".into(),
                    revision: "1".into(),
                    source: Vec::new(),
                    artifact: "export default () => null".into(),
                    grants: ProgramGrants::new([
                        ProgramCapability::Judge,
                        ProgramCapability::EvalRecord,
                    ]),
                },
            )
            .await?;
        let run = ProgramRunId::from_uuid(uuid::Uuid::now_v7());
        let input = b"synthetic immutable manifest".to_vec();
        registrations
            .start_run(run, registration.id, &input)
            .await?;
        let journal = ProgramJournalRepository::new(pool.clone());
        let request = journal
            .append_request(
                run,
                None,
                RequestKind::Effect(EffectRequest::new(
                    ProgramCapability::Judge,
                    "evaluate".into(),
                    InlineFramePayload::default(),
                )),
            )
            .await?;
        journal
            .append_delivery(
                run,
                DeliveryKind::Answer {
                    resolves: request.ordinal(),
                    payload: InlineFramePayload::new(b"synthetic verdict".as_slice()),
                },
            )
            .await?;
        let evidence_position = journal
            .load(run)
            .await?
            .ok_or("fixture journal is missing")?
            .entries()
            .last()
            .ok_or("fixture journal answer is missing")?
            .position();
        let snapshot = EvaluationSnapshot {
            run,
            registration: registration.id,
            input,
            metadata: serde_json::json!({"corpus": "synthetic"}),
            scorecard_kind: "synthetic".into(),
            scorecard: serde_json::json!({"measured": 1}),
            trials: vec![EvaluationTrial {
                ordinal: 0,
                case_position: 3,
                repeat: 0,
                case: serde_json::json!({"expected": "approve", "label_provenance": "synthetic"}),
                evidence_position,
                outcome: EvaluationOutcome::Verdict(
                    serde_json::json!({"actual": "approve", "rationale": "synthetic"}),
                ),
            }],
        };
        Ok(Self {
            _database: database,
            repository: EvaluationRepository::new(pool.clone()),
            pool,
            snapshot,
        })
    }
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn equal_concurrent_seals_adopt_one_complete_snapshot() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let (first, retry) = tokio::join!(
        fixture.repository.seal(&fixture.snapshot),
        fixture.repository.seal(&fixture.snapshot)
    );
    assert_eq!(first?, retry?);
    assert_eq!(
        fixture.repository.load(fixture.snapshot.run).await?,
        Some(fixture.snapshot)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn changed_summary_conflicts_with_the_committed_snapshot() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    fixture.repository.seal(&fixture.snapshot).await?;
    let changed = EvaluationSnapshot {
        scorecard: serde_json::json!({"measured": 2}),
        ..fixture.snapshot.clone()
    };
    assert!(matches!(
        fixture.repository.seal(&changed).await,
        Err(EvaluationError::Conflict)
    ));
    assert_eq!(
        fixture.repository.load(fixture.snapshot.run).await?,
        Some(fixture.snapshot)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn changed_evidence_conflicts_with_the_committed_snapshot() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    fixture.repository.seal(&fixture.snapshot).await?;
    let mut changed = fixture.snapshot.clone();
    changed.trials[0].outcome =
        EvaluationOutcome::Failed(serde_json::json!({"cause": "provider_error"}));
    assert!(matches!(
        fixture.repository.seal(&changed).await,
        Err(EvaluationError::Conflict)
    ));
    assert_eq!(
        fixture.repository.load(fixture.snapshot.run).await?,
        Some(fixture.snapshot)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn changed_registration_cannot_reuse_a_run_identity() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    fixture.repository.seal(&fixture.snapshot).await?;
    let changed = EvaluationSnapshot {
        registration: ProgramRegistrationId::from_uuid(uuid::Uuid::now_v7()),
        ..fixture.snapshot.clone()
    };
    assert!(matches!(
        fixture.repository.seal(&changed).await,
        Err(EvaluationError::Conflict)
    ));
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn changed_input_cannot_reuse_a_run_identity() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let changed = EvaluationSnapshot {
        input: b"changed manifest".to_vec(),
        ..fixture.snapshot.clone()
    };
    assert!(matches!(
        fixture.repository.seal(&changed).await,
        Err(EvaluationError::Conflict)
    ));
    assert_eq!(fixture.repository.load(fixture.snapshot.run).await?, None);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn late_trial_insert_is_rejected_after_seal() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    fixture.repository.seal(&fixture.snapshot).await?;
    let error = sqlx::query("INSERT INTO evaluation_trial SELECT run_id, trial_ordinal + 1, case_position, repeat_ordinal, corpus_case, evidence_position, outcome_kind, evidence FROM evaluation_trial")
        .execute(&fixture.pool).await.unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );
    assert_eq!(
        fixture.repository.load(fixture.snapshot.run).await?,
        Some(fixture.snapshot)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn partial_snapshot_cannot_commit() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let mut tx = fixture.pool.begin().await?;
    sqlx::query("INSERT INTO evaluation_run (run_id, metadata, scorecard_kind, scorecard, trial_count) VALUES ($1, $2, $3, $4, 1)")
        .bind(fixture.snapshot.run.into_uuid()).bind(Json(&fixture.snapshot.metadata))
        .bind(&fixture.snapshot.scorecard_kind).bind(Json(&fixture.snapshot.scorecard)).execute(&mut *tx).await?;
    let error = tx.commit().await.unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );
    assert_eq!(fixture.repository.load(fixture.snapshot.run).await?, None);
    fixture.repository.seal(&fixture.snapshot).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn trial_write_failure_rolls_back_the_header_and_prior_trials() -> Result<(), Box<dyn Error>>
{
    let fixture = Fixture::new().await?;
    let mut broken = fixture.snapshot.clone();
    broken.trials.push(EvaluationTrial {
        ordinal: 1,
        evidence_position: signalbox_domain::JournalPosition::try_from_u64(u64::MAX).unwrap(),
        ..broken.trials[0].clone()
    });
    assert!(matches!(
        fixture.repository.seal(&broken).await,
        Err(EvaluationError::Database(_))
    ));
    assert_eq!(fixture.repository.load(fixture.snapshot.run).await?, None);
    fixture.repository.seal(&fixture.snapshot).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn sealed_rows_reject_update_delete_and_truncate() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    fixture.repository.seal(&fixture.snapshot).await?;
    // These operations together define immutability of the same committed snapshot.
    for statement in [
        "UPDATE evaluation_run SET scorecard = '{}'",
        "DELETE FROM evaluation_run",
        "TRUNCATE evaluation_run CASCADE",
        "UPDATE evaluation_trial SET evidence = '{}'",
        "DELETE FROM evaluation_trial",
        "TRUNCATE evaluation_trial",
    ] {
        let error = sqlx::raw_sql(sqlx::AssertSqlSafe(statement))
            .execute(&fixture.pool)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("23514"),
            "{statement}"
        );
    }
    assert_eq!(
        fixture.repository.load(fixture.snapshot.run).await?,
        Some(fixture.snapshot)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn trial_evidence_cannot_resolve_through_another_run() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let foreign_run = ProgramRunId::from_uuid(uuid::Uuid::now_v7());
    ProgramRegistrationRepository::new(fixture.pool.clone())
        .start_run(
            foreign_run,
            fixture.snapshot.registration,
            &fixture.snapshot.input,
        )
        .await?;
    let journal = ProgramJournalRepository::new(fixture.pool.clone());
    let first = journal
        .append_request(
            foreign_run,
            None,
            RequestKind::Now(InlineFramePayload::default()),
        )
        .await?;
    journal
        .append_delivery(
            foreign_run,
            DeliveryKind::Answer {
                resolves: first.ordinal(),
                payload: InlineFramePayload::default(),
            },
        )
        .await?;
    let second = journal
        .append_request(
            foreign_run,
            None,
            RequestKind::Now(InlineFramePayload::default()),
        )
        .await?;
    journal
        .append_delivery(
            foreign_run,
            DeliveryKind::Answer {
                resolves: second.ordinal(),
                payload: InlineFramePayload::default(),
            },
        )
        .await?;
    let foreign_position = journal
        .load(foreign_run)
        .await?
        .unwrap()
        .entries()
        .last()
        .unwrap()
        .position();
    let mut changed = fixture.snapshot.clone();
    changed.trials[0].evidence_position = foreign_position;
    let EvaluationError::Database(error) = fixture.repository.seal(&changed).await.unwrap_err()
    else {
        panic!("same-run foreign key")
    };
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23503")
    );
    assert_eq!(fixture.repository.load(fixture.snapshot.run).await?, None);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn seal_retries_after_the_committed_receipt_is_lost() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    // The effect committed, but its caller received no delivery before restart.
    fixture.repository.seal(&fixture.snapshot).await?;
    let restarted = EvaluationRepository::new(fixture.pool.clone());
    let receipt = restarted.seal(&fixture.snapshot).await?;
    assert_eq!(receipt.run, fixture.snapshot.run);
    assert_eq!(restarted.load(receipt.run).await?, Some(fixture.snapshot));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM evaluation_run")
        .fetch_one(&fixture.pool)
        .await?;
    assert_eq!(count, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn equal_retry_preserves_json_number_representations() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    // JSON evidence admits exponent notation and integers beyond machine precision.
    fixture.snapshot.scorecard =
        serde_json::from_str(r#"{"ratio":1e-9,"count":18446744073709551616}"#)?;
    fixture.repository.seal(&fixture.snapshot).await?;
    assert_eq!(
        fixture.repository.load(fixture.snapshot.run).await?,
        Some(fixture.snapshot.clone())
    );
    let receipt = fixture.repository.seal(&fixture.snapshot).await?;
    assert_eq!(
        fixture.repository.load(receipt.run).await?,
        Some(fixture.snapshot)
    );
    Ok(())
}
