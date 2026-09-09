//! Durable immutable registration and run-to-registration binding.

use signalbox_domain::{
    ProgramRegistrationId, ProgramRunId,
    program_registration::{
        ProgramContentDigest, ProgramExecutable, ProgramGrants, ProgramRegistration,
        ProgramRegistrationContent, ProgramRegistrationRequest,
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
    #[error("program registration identity or revision is already bound differently")]
    RegistrationConflict { registration: ProgramRegistrationId },
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
        registration: ProgramRegistrationId,
        request: ProgramRegistrationRequest,
    ) -> Result<ProgramRegistration, ProgramRegistrationError> {
        self.register_executable_user(registration, request.into_content())
            .await
    }

    /// Resolves the registrant's grants only through its durable registration.
    pub async fn register_child(
        &self,
        registrant: ProgramRunId,
        registration: ProgramRegistrationId,
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
        self.register_executable_user(registration, content).await
    }

    /// Stores an executable identity admitted at the user boundary.
    pub async fn register_executable_user(
        &self,
        id: ProgramRegistrationId,
        content: ProgramRegistrationContent,
    ) -> Result<ProgramRegistration, ProgramRegistrationError> {
        let registration = ProgramRegistration { id, content };
        let executable = ExecutableColumns::from(&registration.content.executable);
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO program_registration
            (registration_id, name, revision, source_digest, artifact_digest, artifact, grants,
             executable_kind, native_entry, native_revision, binary_digest)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) ON CONFLICT DO NOTHING",
        )
        .bind(registration.id.into_uuid())
        .bind(&registration.content.name)
        .bind(&registration.content.revision)
        .bind(
            executable
                .source_digest
                .map(|digest| digest.as_bytes().to_vec()),
        )
        .bind(
            executable
                .artifact_digest
                .map(|digest| digest.as_bytes().to_vec()),
        )
        .bind(executable.artifact)
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
        .bind(executable.kind)
        .bind(executable.entry)
        .bind(executable.native_revision)
        .bind(
            executable
                .binary_digest
                .map(|digest| digest.as_bytes().to_vec()),
        )
        .execute(&mut *transaction)
        .await?;
        if inserted.rows_affected() == 0 {
            let stored =
                sqlx::query("SELECT * FROM program_registration WHERE registration_id = $1")
                    .bind(id.into_uuid())
                    .fetch_optional(&mut *transaction)
                    .await?
                    .map(decode)
                    .transpose()?;
            transaction.rollback().await?;
            return if stored.as_ref() == Some(&registration) {
                Ok(registration)
            } else {
                Err(ProgramRegistrationError::RegistrationConflict { registration: id })
            };
        }
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
        input: &[u8],
    ) -> Result<ProgramRunId, ProgramRegistrationError> {
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query("INSERT INTO program_run_journal_stream (run_id, frame_contract_version) VALUES ($1, $2) ON CONFLICT (run_id) DO NOTHING")
            .bind(run.into_uuid()).bind(FRAME_CONTRACT_VERSION).execute(&mut *transaction).await?;
        if inserted.rows_affected() == 0 {
            let bound: Option<(Uuid, Vec<u8>)> = sqlx::query_as(
                "SELECT registration_id, input FROM program_run_registration WHERE run_id = $1",
            )
            .bind(run.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
            transaction.rollback().await?;
            return if bound == Some((registration.into_uuid(), input.to_vec())) {
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
            "INSERT INTO program_run_registration (run_id, registration_id, input) VALUES ($1, $2, $3)",
        )
        .bind(run.into_uuid())
        .bind(registration.into_uuid())
        .bind(input)
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

    /// Loads the immutable exact run input without loading executable code.
    pub async fn input_for_run(
        &self,
        run: ProgramRunId,
    ) -> Result<Option<signalbox_domain::InlineFramePayload>, ProgramRegistrationError> {
        let input: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT input FROM program_run_registration WHERE run_id = $1")
                .bind(run.into_uuid())
                .fetch_optional(&self.pool)
                .await?;
        Ok(input.map(signalbox_domain::InlineFramePayload::new))
    }

    /// Adopts an immutable registration only when its complete requested content matches.
    pub async fn find(
        &self,
        content: &ProgramRegistrationContent,
    ) -> Result<Option<ProgramRegistration>, ProgramRegistrationError> {
        let registered =
            sqlx::query("SELECT * FROM program_registration WHERE name = $1 AND revision = $2")
                .bind(&content.name)
                .bind(&content.revision)
                .fetch_optional(&self.pool)
                .await?
                .map(decode)
                .transpose()?;
        Ok(registered.filter(|registration| &registration.content == content))
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
    let kind: String = row.try_get("executable_kind")?;
    let executable = match kind.as_str() {
        "javascript" => {
            let artifact: String = row.try_get("artifact")?;
            if digest(&row, "artifact_digest")? != ProgramContentDigest::of(artifact.as_bytes()) {
                return Err(ProgramRegistrationError::Corruption(
                    "artifact digest mismatch",
                ));
            }
            ProgramExecutable::JavaScript {
                source_digest: digest(&row, "source_digest")?,
                artifact,
            }
        }
        "native" => ProgramExecutable::Native {
            entry: row.try_get("native_entry")?,
            revision: row.try_get("native_revision")?,
            binary_digest: digest(&row, "binary_digest")?,
        },
        _ => return Err(ProgramRegistrationError::Corruption("executable kind")),
    };
    Ok(ProgramRegistration {
        id: ProgramRegistrationId::from_uuid(row.try_get("registration_id")?),
        content: ProgramRegistrationContent {
            name: row.try_get("name")?,
            revision: row.try_get("revision")?,
            executable,
            grants: ProgramGrants::new(grants),
        },
    })
}

fn digest(
    row: &PgRow,
    column: &'static str,
) -> Result<ProgramContentDigest, ProgramRegistrationError> {
    let bytes: Vec<u8> = row.try_get(column)?;
    Ok(ProgramContentDigest::from_bytes(bytes.try_into().map_err(
        |_| ProgramRegistrationError::Corruption(column),
    )?))
}

struct ExecutableColumns<'a> {
    kind: &'static str,
    source_digest: Option<ProgramContentDigest>,
    artifact_digest: Option<ProgramContentDigest>,
    artifact: Option<&'a str>,
    entry: Option<&'a str>,
    native_revision: Option<&'a str>,
    binary_digest: Option<ProgramContentDigest>,
}

impl<'a> From<&'a ProgramExecutable> for ExecutableColumns<'a> {
    fn from(executable: &'a ProgramExecutable) -> Self {
        match executable {
            ProgramExecutable::JavaScript {
                source_digest,
                artifact,
            } => Self {
                kind: "javascript",
                source_digest: Some(*source_digest),
                artifact_digest: Some(ProgramContentDigest::of(artifact.as_bytes())),
                artifact: Some(artifact),
                entry: None,
                native_revision: None,
                binary_digest: None,
            },
            ProgramExecutable::Native {
                entry,
                revision,
                binary_digest,
            } => Self {
                kind: "native",
                source_digest: None,
                artifact_digest: None,
                artifact: None,
                entry: Some(entry),
                native_revision: Some(revision),
                binary_digest: Some(*binary_digest),
            },
        }
    }
}
