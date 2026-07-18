//! Atomic persistence and replay for the admitted `CreateSession` slice.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    CreateSessionAppliedResult, CreateSessionReconstitutionFailure,
    CreateSessionReconstitutionInput, DurableCommandId, ModelSelectionRequest,
    PreparedCreateSession, ReconstitutedSessionCreation, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    TranscriptAncestry,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::mapping::{
    PositiveOrdinalMappingError, defaults_version_from_numeric, defaults_version_to_numeric,
    direct_model_selection_from_uuid, direct_model_selection_to_uuid, durable_command_id_to_uuid,
    model_alias_from_uuid, model_alias_to_uuid, session_id_from_uuid, session_id_to_uuid,
};

const COMMAND_KIND: &str = "create_session";
const STORAGE_VERSION: i16 = 1;
const OWNER_INITIATED: &str = "owner_initiated";
const NO_ANCESTRY: &str = "none";
const APPLIED: &str = "applied";

/// The committed outcome of handling one prepared creation command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreateSessionHandlingOutcome {
    /// First handling or equal replay returns the recorded applied result.
    Applied(CreateSessionAppliedResult),
    /// The identifier is already bound to a structurally different payload.
    ConflictingReuse {
        /// The owner-global identifier whose earlier meaning is retained.
        command_id: DurableCommandId,
    },
}

/// A durable shape that cannot reconstruct the admitted domain value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CreateSessionCorruption {
    /// One required row or field is absent.
    Missing(&'static str),
    /// A closed discriminator or representation version is unsupported.
    Unsupported {
        /// The record field that could not be decoded.
        field: &'static str,
        /// The durable spelling that was observed.
        value: String,
    },
    /// A typed record relationship disagrees with another durable record.
    Inconsistent(&'static str),
    /// A stored positive ordinal cannot construct the domain value.
    InvalidOrdinal {
        /// The ordinal-bearing record field.
        field: &'static str,
        /// Why the numeric value is outside the domain.
        reason: PositiveOrdinalMappingError,
    },
    /// Complete checked values fail domain-owned correlation.
    Domain(CreateSessionReconstitutionFailure),
}

impl fmt::Display for CreateSessionCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "missing durable CreateSession {field}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "unsupported CreateSession {field}: {value}")
            }
            Self::Inconsistent(relationship) => {
                write!(formatter, "inconsistent CreateSession {relationship}")
            }
            Self::InvalidOrdinal { field, reason } => {
                write!(formatter, "invalid CreateSession {field}: {reason}")
            }
            Self::Domain(failure) => {
                write!(
                    formatter,
                    "CreateSession domain reconstitution failed: {failure:?}"
                )
            }
        }
    }
}

impl Error for CreateSessionCorruption {}

/// A database failure or a fail-closed durable-shape failure.
#[derive(Debug)]
pub enum CreateSessionRepositoryError {
    /// PostgreSQL could not complete the requested operation.
    Database(sqlx::Error),
    /// Committed or transaction-visible records cannot reconstruct the domain.
    Corruption(CreateSessionCorruption),
}

impl fmt::Display for CreateSessionRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "CreateSession database failure: {error}"),
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for CreateSessionRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for CreateSessionRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<CreateSessionCorruption> for CreateSessionRepositoryError {
    fn from(error: CreateSessionCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// PostgreSQL implementation of the initial session-creation boundary.
#[derive(Clone, Debug)]
pub struct CreateSessionRepository {
    pool: PgPool,
}

impl CreateSessionRepository {
    /// Uses the supplied pool for atomic handling and complete loads.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Claims and applies a new command, or resolves replay from the winner.
    ///
    /// Lookup by owner-global command identity is the first durable operation.
    /// All first-handling records commit together; no returned applied result
    /// precedes commit.
    pub async fn handle(
        &self,
        prepared: PreparedCreateSession,
    ) -> Result<CreateSessionHandlingOutcome, CreateSessionRepositoryError> {
        let command_id = prepared.command().command_id();
        let mut transaction = self.pool.begin().await?;

        if let Some(recorded) = load_from_connection(&mut transaction, command_id).await? {
            let outcome = existing_outcome(&prepared, &recorded);
            transaction.rollback().await?;
            return Ok(outcome);
        }

        let claimed = sqlx::query(
            "INSERT INTO durable_command
                (command_id, command_kind, storage_version, claimed_at)
             VALUES ($1, $2, $3, transaction_timestamp())
             ON CONFLICT DO NOTHING",
        )
        .bind(durable_command_id_to_uuid(command_id))
        .bind(COMMAND_KIND)
        .bind(STORAGE_VERSION)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;

        if !claimed {
            let recorded = load_from_connection(&mut transaction, command_id)
                .await?
                .ok_or(CreateSessionCorruption::Inconsistent(
                    "winner claim disappeared",
                ))?;
            let outcome = existing_outcome(&prepared, &recorded);
            transaction.rollback().await?;
            return Ok(outcome);
        }

        if let Err(error) = insert_prepared(&mut transaction, prepared).await {
            transaction.rollback().await?;
            return Err(error);
        }

        let result = prepared.applied_result();
        transaction.commit().await?;
        Ok(CreateSessionHandlingOutcome::Applied(result))
    }

    /// Loads one complete claimed creation, or `None` only for an unseen ID.
    pub async fn load(
        &self,
        command_id: DurableCommandId,
    ) -> Result<Option<ReconstitutedSessionCreation>, CreateSessionRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        load_from_connection(&mut connection, command_id).await
    }
}

fn existing_outcome(
    prepared: &PreparedCreateSession,
    recorded: &ReconstitutedSessionCreation,
) -> CreateSessionHandlingOutcome {
    if prepared.command() == recorded.command() {
        CreateSessionHandlingOutcome::Applied(recorded.applied_result())
    } else {
        CreateSessionHandlingOutcome::ConflictingReuse {
            command_id: prepared.command().command_id(),
        }
    }
}

async fn insert_prepared(
    connection: &mut PgConnection,
    prepared: PreparedCreateSession,
) -> Result<(), CreateSessionRepositoryError> {
    let command = prepared.command();
    let session = prepared.session();
    let defaults = session.configuration_defaults();
    let command_selection = encode_selection(command.initial_configuration_defaults().model());
    let stored_selection = encode_selection(defaults.defaults().model());

    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES ($1, $2, $3)",
    )
    .bind(session_id_to_uuid(session.id()))
    .bind(OWNER_INITIATED)
    .bind(NO_ANCESTRY)
    .execute(&mut *connection)
    .await?;

    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(session_id_to_uuid(session.id()))
    .bind(defaults_version_to_numeric(defaults.version()))
    .bind(stored_selection.kind)
    .bind(stored_selection.direct)
    .bind(stored_selection.alias)
    .execute(&mut *connection)
    .await?;

    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ($1, $2)",
    )
    .bind(session_id_to_uuid(session.id()))
    .bind(defaults_version_to_numeric(defaults.version()))
    .execute(&mut *connection)
    .await?;

    sqlx::query(
        "INSERT INTO create_session_command
            (command_id, command_kind, storage_version,
             creation_cause, ancestry_kind, initial_defaults_version,
             model_selection_kind, direct_model_selection_id, model_alias_id,
             result_kind, created_session_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(COMMAND_KIND)
    .bind(STORAGE_VERSION)
    .bind(OWNER_INITIATED)
    .bind(NO_ANCESTRY)
    .bind(defaults_version_to_numeric(defaults.version()))
    .bind(command_selection.kind)
    .bind(command_selection.direct)
    .bind(command_selection.alias)
    .bind(APPLIED)
    .bind(session_id_to_uuid(prepared.applied_result().session()))
    .execute(&mut *connection)
    .await?;

    Ok(())
}

struct EncodedSelection {
    kind: &'static str,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
}

fn encode_selection(selection: ModelSelectionRequest) -> EncodedSelection {
    match selection {
        ModelSelectionRequest::Direct(value) => EncodedSelection {
            kind: "direct",
            direct: Some(direct_model_selection_to_uuid(value)),
            alias: None,
        },
        ModelSelectionRequest::Alias(value) => EncodedSelection {
            kind: "alias",
            direct: None,
            alias: Some(model_alias_to_uuid(value)),
        },
    }
}

async fn load_from_connection(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<ReconstitutedSessionCreation>, CreateSessionRepositoryError> {
    let row = sqlx::query(
        "SELECT
            d.command_kind AS registry_kind,
            d.storage_version AS registry_version,
            c.command_id AS typed_command_id,
            c.command_kind AS typed_kind,
            c.storage_version AS typed_version,
            c.creation_cause AS command_cause,
            c.ancestry_kind AS command_ancestry,
            c.initial_defaults_version,
            c.model_selection_kind AS command_model_kind,
            c.direct_model_selection_id AS command_direct_id,
            c.model_alias_id AS command_alias_id,
            c.result_kind,
            c.created_session_id AS result_session_id,
            s.session_id AS stored_session_id,
            s.creation_cause AS stored_cause,
            s.ancestry_kind AS stored_ancestry,
            v.session_id AS defaults_session_id,
            v.version AS stored_defaults_version,
            v.model_selection_kind AS stored_model_kind,
            v.direct_model_selection_id AS stored_direct_id,
            v.model_alias_id AS stored_alias_id,
            p.session_id AS pointer_session_id,
            p.current_version
         FROM durable_command AS d
         LEFT JOIN create_session_command AS c
           ON c.command_id = d.command_id
         LEFT JOIN session AS s
           ON s.session_id = c.created_session_id
         LEFT JOIN session_defaults_version AS v
           ON v.session_id = c.created_session_id
          AND v.version = c.initial_defaults_version
         LEFT JOIN session_current_defaults AS p
           ON p.session_id = c.created_session_id
         WHERE d.command_id = $1",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .fetch_optional(&mut *connection)
    .await?;

    row.map(|row| decode_complete(row, command_id)).transpose()
}

fn decode_complete(
    row: PgRow,
    command_id: DurableCommandId,
) -> Result<ReconstitutedSessionCreation, CreateSessionRepositoryError> {
    require_spelling(&row, "registry_kind", COMMAND_KIND)?;
    require_version(&row, "registry_version", STORAGE_VERSION)?;
    let _: Uuid = required(&row, "typed_command_id")?;
    require_spelling(&row, "typed_kind", COMMAND_KIND)?;
    require_version(&row, "typed_version", STORAGE_VERSION)?;
    let command_provenance = decode_provenance(
        required(&row, "command_cause")?,
        required(&row, "command_ancestry")?,
    )?;
    let initial_version = decode_ordinal(&row, "initial_defaults_version")?;
    if initial_version != SessionConfigurationDefaultsVersion::first() {
        return Err(
            CreateSessionCorruption::Inconsistent("command initial defaults version").into(),
        );
    }
    let command_defaults = decode_selection(
        required(&row, "command_model_kind")?,
        row.try_get("command_direct_id")?,
        row.try_get("command_alias_id")?,
        "command model selection",
    )?;
    require_spelling(&row, "result_kind", APPLIED)?;
    let result_session = session_id_from_uuid(required(&row, "result_session_id")?);

    let stored_session_uuid: Uuid = required(&row, "stored_session_id")?;
    let stored_session = session_id_from_uuid(stored_session_uuid);
    let stored_provenance = decode_provenance(
        required(&row, "stored_cause")?,
        required(&row, "stored_ancestry")?,
    )?;
    let defaults_session: Uuid = required(&row, "defaults_session_id")?;
    let pointer_session: Uuid = required(&row, "pointer_session_id")?;
    if defaults_session != stored_session_uuid || pointer_session != stored_session_uuid {
        return Err(CreateSessionCorruption::Inconsistent("session/defaults ownership").into());
    }
    let stored_version = decode_ordinal(&row, "stored_defaults_version")?;
    let current_version = decode_ordinal(&row, "current_version")?;
    if current_version != stored_version {
        return Err(
            CreateSessionCorruption::Inconsistent("current defaults pointer version").into(),
        );
    }
    let stored_defaults = decode_selection(
        required(&row, "stored_model_kind")?,
        row.try_get("stored_direct_id")?,
        row.try_get("stored_alias_id")?,
        "stored model selection",
    )?;

    CreateSessionReconstitutionInput::new(
        signalbox_domain::CreateSession::new(command_id, command_provenance, command_defaults),
        result_session,
        stored_session,
        stored_provenance,
        stored_version,
        stored_defaults,
    )
    .reconstitute()
    .map_err(|error| CreateSessionCorruption::Domain(error.failure()).into())
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, CreateSessionRepositoryError>
where
    for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or_else(|| CreateSessionCorruption::Missing(field).into())
}

fn require_spelling(
    row: &PgRow,
    field: &'static str,
    expected: &str,
) -> Result<(), CreateSessionRepositoryError> {
    let actual: String = required(row, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(CreateSessionCorruption::Unsupported {
            field,
            value: actual,
        }
        .into())
    }
}

fn require_version(
    row: &PgRow,
    field: &'static str,
    expected: i16,
) -> Result<(), CreateSessionRepositoryError> {
    let actual: i16 = required(row, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(CreateSessionCorruption::Unsupported {
            field,
            value: actual.to_string(),
        }
        .into())
    }
}

fn decode_ordinal(
    row: &PgRow,
    field: &'static str,
) -> Result<SessionConfigurationDefaultsVersion, CreateSessionRepositoryError> {
    let value: Decimal = required(row, field)?;
    defaults_version_from_numeric(value)
        .map_err(|reason| CreateSessionCorruption::InvalidOrdinal { field, reason }.into())
}

fn decode_provenance(
    cause: String,
    ancestry: String,
) -> Result<SessionCreationProvenance, CreateSessionRepositoryError> {
    if cause != OWNER_INITIATED {
        return Err(CreateSessionCorruption::Unsupported {
            field: "creation cause",
            value: cause,
        }
        .into());
    }
    if ancestry != NO_ANCESTRY {
        return Err(CreateSessionCorruption::Unsupported {
            field: "ancestry kind",
            value: ancestry,
        }
        .into());
    }
    Ok(SessionCreationProvenance::new(
        SessionCreationCause::OwnerInitiated,
        TranscriptAncestry::None,
    ))
}

fn decode_selection(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    field: &'static str,
) -> Result<SessionConfigurationDefaults, CreateSessionRepositoryError> {
    let model = match (kind.as_str(), direct, alias) {
        ("direct", Some(value), None) => {
            ModelSelectionRequest::Direct(direct_model_selection_from_uuid(value))
        }
        ("alias", None, Some(value)) => ModelSelectionRequest::Alias(model_alias_from_uuid(value)),
        ("direct" | "alias", _, _) => {
            return Err(CreateSessionCorruption::Inconsistent(field).into());
        }
        _ => {
            return Err(CreateSessionCorruption::Unsupported { field, value: kind }.into());
        }
    };
    Ok(SessionConfigurationDefaults::new(model))
}
