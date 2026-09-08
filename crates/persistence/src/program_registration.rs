//! Durable immutable registration and run-to-registration binding.

use signalbox_domain::{
    ProgramRegistrationId, ProgramRunId,
    program_registration::{
        ProgramContentDigest, ProgramGrants, ProgramRegistration, ProgramRegistrationContent,
        ProgramRegistrationRequest,
    },
};
use sqlx::{PgPool, Row, postgres::PgRow};
use uuid::Uuid;

use crate::{
    commit_failure_is_ambiguous,
    mapping::{program_capability_from_str, program_capability_to_str},
    program_journal::FRAME_CONTRACT_VERSION,
};

#[derive(Debug, signalbox_derive::OperatorError)]
pub enum ProgramRegistrationError {
    #[error("program registration database: {source}")]
    Database {
        #[source]
        source: sqlx::Error,
        commit_ambiguous: bool,
    },
    #[error("program run is already bound differently")]
    RunConflict { run: ProgramRunId },
    #[error("program registration corruption: {field_0}")]
    Corruption(&'static str),
    #[error("program registration grants exceed the registrant's grants")]
    GrantsDenied,
    #[error("program run has no registration")]
    RunMissing,
}

impl From<sqlx::Error> for ProgramRegistrationError {
    fn from(source: sqlx::Error) -> Self {
        Self::Database {
            source,
            commit_ambiguous: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProgramRegistrationRepository {
    pool: PgPool,
}

impl ProgramRegistrationRepository {
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Registers exact source bytes and a stripped artifact at the user boundary.
    pub async fn register_user(
        &self,
        request: ProgramRegistrationRequest,
    ) -> Result<ProgramRegistration, ProgramRegistrationError> {
        self.insert(request.into_content()).await
    }

    /// Resolves the registrant's grants only through its durable registration.
    pub async fn register_child(
        &self,
        registrant: ProgramRunId,
        request: ProgramRegistrationRequest,
    ) -> Result<ProgramRegistration, ProgramRegistrationError> {
        let content = request.into_content();
        let parent = self
            .for_run(registrant)
            .await?
            .ok_or(ProgramRegistrationError::RunMissing)?;
        if !parent.content.grants.permits_child(&content.grants) {
            return Err(ProgramRegistrationError::GrantsDenied);
        }
        self.insert(content).await
    }

    async fn insert(
        &self,
        content: ProgramRegistrationContent,
    ) -> Result<ProgramRegistration, ProgramRegistrationError> {
        let registration = ProgramRegistration {
            id: ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            artifact_digest: ProgramContentDigest::of(content.artifact.as_bytes()),
            content,
        };
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO program_registration
            (registration_id, name, revision, source_digest, artifact_digest, artifact, grants)
            VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(registration.id.into_uuid())
        .bind(&registration.content.name)
        .bind(&registration.content.revision)
        .bind(registration.content.source_digest.as_bytes().as_slice())
        .bind(registration.artifact_digest.as_bytes().as_slice())
        .bind(&registration.content.artifact)
        .bind(
            registration
                .content
                .grants
                .capabilities()
                .iter()
                .copied()
                .map(program_capability_to_str)
                .collect::<Vec<_>>(),
        )
        .execute(&mut *transaction)
        .await?;
        transaction
            .commit()
            .await
            .map_err(|source| ProgramRegistrationError::Database {
                commit_ambiguous: commit_failure_is_ambiguous(&source),
                source,
            })?;
        Ok(registration)
    }

    /// Creates one journal and its immutable binding, or returns an equal retry.
    pub async fn start_run(
        &self,
        run: ProgramRunId,
        registration: ProgramRegistrationId,
    ) -> Result<ProgramRunId, ProgramRegistrationError> {
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query("INSERT INTO program_run_journal_stream (run_id, frame_contract_version) VALUES ($1, $2) ON CONFLICT (run_id) DO NOTHING")
            .bind(run.into_uuid()).bind(FRAME_CONTRACT_VERSION).execute(&mut *transaction).await?;
        if inserted.rows_affected() == 0 {
            let bound: Option<Uuid> = sqlx::query_scalar(
                "SELECT registration_id FROM program_run_registration WHERE run_id = $1",
            )
            .bind(run.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
            transaction.rollback().await?;
            return if bound == Some(registration.into_uuid()) {
                Ok(run)
            } else {
                Err(ProgramRegistrationError::RunConflict { run })
            };
        }
        sqlx::query("INSERT INTO program_run_journal_sequence_state (run_id) VALUES ($1)")
            .bind(run.into_uuid())
            .execute(&mut *transaction)
            .await?;
        sqlx::query(
            "INSERT INTO program_run_registration (run_id, registration_id) VALUES ($1, $2)",
        )
        .bind(run.into_uuid())
        .bind(registration.into_uuid())
        .execute(&mut *transaction)
        .await?;
        transaction
            .commit()
            .await
            .map_err(|source| ProgramRegistrationError::Database {
                commit_ambiguous: commit_failure_is_ambiguous(&source),
                source,
            })?;
        Ok(run)
    }

    pub async fn for_run(
        &self,
        run: ProgramRunId,
    ) -> Result<Option<ProgramRegistration>, ProgramRegistrationError> {
        sqlx::query(
            "SELECT registration.* FROM program_registration AS registration
            JOIN program_run_registration AS run USING (registration_id) WHERE run.run_id = $1",
        )
        .bind(run.into_uuid())
        .fetch_optional(&self.pool)
        .await?
        .map(decode)
        .transpose()
    }
}

fn decode(row: PgRow) -> Result<ProgramRegistration, ProgramRegistrationError> {
    let stored_grants: Vec<String> = row.try_get("grants")?;
    let grants = stored_grants
        .iter()
        .map(|grant| {
            program_capability_from_str(grant).ok_or(ProgramRegistrationError::Corruption("grant"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let source_digest: Vec<u8> = row.try_get("source_digest")?;
    let artifact_digest: Vec<u8> = row.try_get("artifact_digest")?;
    let artifact: String = row.try_get("artifact")?;
    let artifact_digest = ProgramContentDigest::from_bytes(
        artifact_digest
            .try_into()
            .map_err(|_| ProgramRegistrationError::Corruption("artifact digest"))?,
    );
    if artifact_digest != ProgramContentDigest::of(artifact.as_bytes()) {
        return Err(ProgramRegistrationError::Corruption(
            "artifact digest mismatch",
        ));
    }
    Ok(ProgramRegistration {
        id: ProgramRegistrationId::from_uuid(row.try_get("registration_id")?),
        artifact_digest,
        content: ProgramRegistrationContent {
            name: row.try_get("name")?,
            revision: row.try_get("revision")?,
            artifact,
            source_digest: ProgramContentDigest::from_bytes(
                source_digest
                    .try_into()
                    .map_err(|_| ProgramRegistrationError::Corruption("source digest"))?,
            ),
            grants: ProgramGrants::new(grants),
        },
    })
}
