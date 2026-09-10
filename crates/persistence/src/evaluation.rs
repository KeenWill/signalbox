//! Atomic immutable evaluation snapshots; docs/spec/eval-system.md.

use rust_decimal::{Decimal, prelude::ToPrimitive};
use signalbox_domain::{
    JournalPosition, ProgramRegistrationId, ProgramRunId,
    evaluation::{EvaluationReceipt, EvaluationSnapshot, EvaluationTrial},
};
use sqlx::{PgConnection, PgPool, types::Json};

#[derive(Debug, signalbox_derive::OperatorError)]
pub enum EvaluationError {
    #[error("evaluation identity or snapshot conflicts")]
    Conflict,
    #[error("evaluation snapshot is incomplete or malformed")]
    InvalidSnapshot,
    #[error("evaluation storage is corrupt")]
    Corrupt,
    #[error("evaluation database operation failed: {field_0}")]
    Database(#[source] sqlx::Error),
}

impl From<sqlx::Error> for EvaluationError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

#[derive(Clone)]
pub struct EvaluationRepository {
    pool: PgPool,
}

impl EvaluationRepository {
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Inserts the complete snapshot atomically; an equal retry adopts its receipt.
    pub async fn seal(
        &self,
        snapshot: &EvaluationSnapshot,
    ) -> Result<EvaluationReceipt, EvaluationError> {
        if snapshot.trials.is_empty()
            || snapshot
                .trials
                .iter()
                .enumerate()
                .any(|(ordinal, trial)| usize::try_from(trial.ordinal).ok() != Some(ordinal))
        {
            return Err(EvaluationError::InvalidSnapshot);
        }
        let mut tx = self.pool.begin().await?;
        let identity: Option<IdentityRow> = sqlx::query_as(
            "SELECT registration_id, input FROM program_run_registration WHERE run_id = $1",
        )
        .bind(snapshot.run.into_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        if identity.is_none_or(|identity| {
            identity.registration_id != snapshot.registration.into_uuid()
                || identity.input != snapshot.input
        }) {
            return Err(EvaluationError::Conflict);
        }
        // Text-to-json bindings preserve numeric representations during retry comparison.
        let inserted = sqlx::query(
            "INSERT INTO evaluation_run (run_id, metadata, scorecard_kind, scorecard, trial_count)
             VALUES ($1, $2::json, $3, $4::json, $5) ON CONFLICT (run_id) DO NOTHING",
        )
        .bind(snapshot.run.into_uuid())
        .bind(snapshot.metadata.to_string())
        .bind(&snapshot.scorecard_kind)
        .bind(snapshot.scorecard.to_string())
        .bind(i64::try_from(snapshot.trials.len()).map_err(|_| EvaluationError::InvalidSnapshot)?)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            if load(&mut tx, snapshot.run).await?.as_ref() != Some(snapshot) {
                return Err(EvaluationError::Conflict);
            }
        } else {
            for trial in &snapshot.trials {
                let (kind, evidence) = crate::mapping::encode_evaluation_outcome(&trial.outcome);
                sqlx::query(
                    "INSERT INTO evaluation_trial
                     (run_id, trial_ordinal, case_position, repeat_ordinal, corpus_case,
                      evidence_position, outcome_kind, evidence)
                     VALUES ($1, $2, $3, $4, $5::json, $6, $7, $8::json)",
                )
                .bind(snapshot.run.into_uuid())
                .bind(i64::from(trial.ordinal))
                .bind(i64::from(trial.case_position))
                .bind(i64::from(trial.repeat))
                .bind(trial.case.to_string())
                .bind(Decimal::from(trial.evidence_position.as_u64()))
                .bind(kind)
                .bind(evidence.to_string())
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(EvaluationReceipt { run: snapshot.run })
    }

    pub async fn load(
        &self,
        run: ProgramRunId,
    ) -> Result<Option<EvaluationSnapshot>, EvaluationError> {
        let mut connection = self.pool.acquire().await?;
        load(&mut connection, run).await
    }
}

#[derive(sqlx::FromRow)]
struct IdentityRow {
    registration_id: uuid::Uuid,
    input: Vec<u8>,
}

#[derive(sqlx::FromRow)]
struct RunRow {
    registration_id: uuid::Uuid,
    input: Vec<u8>,
    metadata: Json<serde_json::Value>,
    scorecard_kind: String,
    scorecard: Json<serde_json::Value>,
}

#[derive(sqlx::FromRow)]
struct TrialRow {
    trial_ordinal: i64,
    case_position: i64,
    repeat_ordinal: i64,
    corpus_case: Json<serde_json::Value>,
    evidence_position: Decimal,
    outcome_kind: String,
    evidence: Json<serde_json::Value>,
}

async fn load(
    connection: &mut PgConnection,
    run: ProgramRunId,
) -> Result<Option<EvaluationSnapshot>, EvaluationError> {
    let Some(header): Option<RunRow> = sqlx::query_as(
        "SELECT registration_id, input, metadata, scorecard_kind, scorecard
         FROM evaluation_run JOIN program_run_registration USING (run_id) WHERE run_id = $1",
    )
    .bind(run.into_uuid())
    .fetch_optional(&mut *connection)
    .await?
    else {
        return Ok(None);
    };
    let rows: Vec<TrialRow> = sqlx::query_as(
        "SELECT trial_ordinal, case_position, repeat_ordinal, corpus_case,
                evidence_position, outcome_kind, evidence
         FROM evaluation_trial WHERE run_id = $1 ORDER BY trial_ordinal",
    )
    .bind(run.into_uuid())
    .fetch_all(connection)
    .await?;
    let trials = rows
        .into_iter()
        .map(|row| {
            Ok(EvaluationTrial {
                ordinal: u32::try_from(row.trial_ordinal).map_err(|_| EvaluationError::Corrupt)?,
                case_position: u32::try_from(row.case_position)
                    .map_err(|_| EvaluationError::Corrupt)?,
                repeat: u32::try_from(row.repeat_ordinal).map_err(|_| EvaluationError::Corrupt)?,
                case: row.corpus_case.0,
                evidence_position: row
                    .evidence_position
                    .to_u64()
                    .and_then(JournalPosition::try_from_u64)
                    .ok_or(EvaluationError::Corrupt)?,
                outcome: crate::mapping::decode_evaluation_outcome(
                    &row.outcome_kind,
                    row.evidence.0,
                )
                .ok_or(EvaluationError::Corrupt)?,
            })
        })
        .collect::<Result<_, EvaluationError>>()?;
    Ok(Some(EvaluationSnapshot {
        run,
        registration: ProgramRegistrationId::from_uuid(header.registration_id),
        input: header.input,
        metadata: header.metadata.0,
        scorecard_kind: header.scorecard_kind,
        scorecard: header.scorecard.0,
        trials,
    }))
}
