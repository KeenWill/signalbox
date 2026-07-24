//! Atomic persistence and replay for session-defaults replacement.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{ReplaceSessionDefaultsOutcome, ReplaceSessionDefaultsTransaction};
use signalbox_domain::{
    DirectModelSelection, DurableCommandId, ModelAlias, ModelSelectionRequest,
    PreparedReplaceSessionDefaults, ReconstitutedReplaceSessionDefaults, ReplaceSessionDefaults,
    ReplaceSessionDefaultsAppliedResult, ReplaceSessionDefaultsReconstitutionFailure,
    ReplaceSessionDefaultsReconstitutionInput, ReplaceSessionDefaultsRejectedResult,
    ReplaceSessionDefaultsResult, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    command_registry::{
        self, CommandKind, REPLACE_SESSION_DEFAULTS_KIND, RegistryCorruption,
        RegistryInspectionError,
    },
    mapping::{
        PositiveOrdinalMappingError, defaults_version_from_numeric, defaults_version_to_numeric,
        durable_command_id_to_uuid, session_id_from_uuid, session_id_to_uuid,
    },
    session::{SessionCorruption, SessionRepositoryError, load_session_from_connection},
};

const STORAGE_VERSION: i16 = 1;
const APPLIED: &str = "applied";
const REJECTED: &str = "rejected";
const SESSION_NOT_FOUND: &str = "session_not_found";
const CURRENT_VERSION_MISMATCH: &str = "current_version_mismatch";
const VERSION_EXHAUSTED: &str = "version_exhausted";

/// The committed outcome of handling one defaults-replacement command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplaceSessionDefaultsHandlingOutcome {
    /// First handling or equal replay returns the recorded application.
    Applied(ReplaceSessionDefaultsAppliedResult),
    /// First handling or equal replay returns the recorded rejection.
    Rejected(ReplaceSessionDefaultsRejectedResult),
    /// The identifier already has a structurally different owner-global use.
    ConflictingReuse {
        /// The owner-global identifier whose existing meaning is retained.
        command_id: DurableCommandId,
    },
}

/// A durable shape that cannot reconstruct one recorded replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplaceSessionDefaultsCorruption {
    /// One required row or field is absent.
    Missing(&'static str),
    /// A closed discriminator or representation version is unsupported.
    Unsupported {
        /// The record field that could not be decoded.
        field: &'static str,
        /// The durable spelling that was observed.
        value: String,
    },
    /// Typed record relationships or variant fields disagree.
    Inconsistent(&'static str),
    /// A stored positive ordinal cannot construct a domain version.
    InvalidOrdinal {
        /// The ordinal-bearing record field.
        field: &'static str,
        /// Why the numeric value is outside the domain.
        reason: PositiveOrdinalMappingError,
    },
    /// The current session projection is incomplete or invalid.
    CurrentSession(SessionCorruption),
    /// Complete checked receipt values fail domain-owned correlation.
    Domain(ReplaceSessionDefaultsReconstitutionFailure),
}

impl fmt::Display for ReplaceSessionDefaultsCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => {
                write!(formatter, "missing durable ReplaceSessionDefaults {field}")
            }
            Self::Unsupported { field, value } => {
                write!(
                    formatter,
                    "unsupported ReplaceSessionDefaults {field}: {value}"
                )
            }
            Self::Inconsistent(relationship) => {
                write!(
                    formatter,
                    "inconsistent ReplaceSessionDefaults {relationship}"
                )
            }
            Self::InvalidOrdinal { field, reason } => {
                write!(
                    formatter,
                    "invalid ReplaceSessionDefaults {field}: {reason}"
                )
            }
            Self::CurrentSession(error) => {
                write!(
                    formatter,
                    "ReplaceSessionDefaults current Session is invalid: {error}"
                )
            }
            Self::Domain(failure) => write!(
                formatter,
                "ReplaceSessionDefaults domain reconstitution failed: {failure:?}"
            ),
        }
    }
}

impl Error for ReplaceSessionDefaultsCorruption {}

/// A database failure, wrong purpose-specific load, or integrity failure.
#[derive(Debug)]
pub enum ReplaceSessionDefaultsRepositoryError {
    /// PostgreSQL could not complete the operation.
    Database(sqlx::Error),
    /// A purpose-specific load named a valid command of another admitted kind.
    DifferentCommandKind {
        /// The owner-global identifier that names another kind.
        command_id: DurableCommandId,
    },
    /// Durable records cannot reconstruct the requested domain value.
    Corruption(ReplaceSessionDefaultsCorruption),
}

impl fmt::Display for ReplaceSessionDefaultsRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => {
                write!(
                    formatter,
                    "ReplaceSessionDefaults database failure: {error}"
                )
            }
            Self::DifferentCommandKind { command_id } => write!(
                formatter,
                "durable command {command_id:?} does not name ReplaceSessionDefaults"
            ),
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for ReplaceSessionDefaultsRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::DifferentCommandKind { .. } => None,
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for ReplaceSessionDefaultsRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<ReplaceSessionDefaultsCorruption> for ReplaceSessionDefaultsRepositoryError {
    fn from(error: ReplaceSessionDefaultsCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// PostgreSQL implementation of atomic defaults replacement.
#[derive(Clone, Debug)]
pub struct ReplaceSessionDefaultsRepository {
    pool: PgPool,
}

impl ReplaceSessionDefaultsRepository {
    /// Uses the supplied pool for atomic handling and complete receipt loads.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Claims and handles an unseen command, or resolves its recorded meaning.
    ///
    /// Owner-global command lookup is the first durable read. Mutable session
    /// state is consulted only for an unseen identifier.
    pub async fn handle(
        &self,
        command: ReplaceSessionDefaults,
    ) -> Result<ReplaceSessionDefaultsHandlingOutcome, ReplaceSessionDefaultsRepositoryError> {
        let command_id = command.command_id();
        let mut transaction = self.pool.begin().await?;

        match inspect_registry(&mut transaction, command_id).await? {
            Some(CommandKind::ReplaceSessionDefaults) => {
                let recorded = load_from_connection(&mut transaction, command_id)
                    .await?
                    .ok_or(ReplaceSessionDefaultsCorruption::Inconsistent(
                        "registry entry disappeared",
                    ))?;
                let outcome = existing_outcome(&command, &recorded);
                transaction.rollback().await?;
                return Ok(outcome);
            }
            Some(CommandKind::CreateSession | CommandKind::CreateSessionFromImportedFrontier) => {
                transaction.rollback().await?;
                return Ok(ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse { command_id });
            }
            Some(CommandKind::SubmitInput) => {
                transaction.rollback().await?;
                return Ok(ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse { command_id });
            }
            None => {}
        }

        let claimed = sqlx::query(
            "INSERT INTO durable_command
                (command_id, command_kind, storage_version, claimed_at)
             VALUES ($1, $2, $3, transaction_timestamp())
             ON CONFLICT DO NOTHING",
        )
        .bind(durable_command_id_to_uuid(command_id))
        .bind(REPLACE_SESSION_DEFAULTS_KIND)
        .bind(STORAGE_VERSION)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;

        if !claimed {
            let outcome = match inspect_registry(&mut transaction, command_id).await? {
                Some(CommandKind::ReplaceSessionDefaults) => {
                    let recorded = load_from_connection(&mut transaction, command_id)
                        .await?
                        .ok_or(ReplaceSessionDefaultsCorruption::Inconsistent(
                            "winner claim disappeared",
                        ))?;
                    existing_outcome(&command, &recorded)
                }
                Some(
                    CommandKind::CreateSession | CommandKind::CreateSessionFromImportedFrontier,
                ) => ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse { command_id },
                Some(CommandKind::SubmitInput) => {
                    ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse { command_id }
                }
                None => {
                    return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
                        "winner claim disappeared",
                    )
                    .into());
                }
            };
            transaction.rollback().await?;
            return Ok(outcome);
        }

        let prepared = prepare_against_current(&mut transaction, command).await?;
        let prepared = match prepared.result() {
            ReplaceSessionDefaultsResult::Applied(applied) => {
                let updated = sqlx::query(
                    "UPDATE session_current_defaults
                     SET current_version = $3
                     WHERE session_id = $1
                       AND current_version = $2",
                )
                .bind(session_id_to_uuid(command.session()))
                .bind(defaults_version_to_numeric(
                    command.expected_current_version(),
                ))
                .bind(defaults_version_to_numeric(applied.installed().version()))
                .execute(&mut *transaction)
                .await?
                .rows_affected();

                if updated == 1 {
                    insert_defaults_version(&mut transaction, applied).await?;
                    prepared
                } else if updated == 0 {
                    let rederived = prepare_against_current(&mut transaction, command).await?;
                    if matches!(rederived.result(), ReplaceSessionDefaultsResult::Applied(_)) {
                        transaction.rollback().await?;
                        return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
                            "pointer compare-and-set lost without a version change",
                        )
                        .into());
                    }
                    rederived
                } else {
                    transaction.rollback().await?;
                    return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
                        "pointer compare-and-set affected multiple rows",
                    )
                    .into());
                }
            }
            ReplaceSessionDefaultsResult::Rejected(_) => prepared,
        };

        insert_typed_record(&mut transaction, prepared).await?;
        let outcome = result_outcome(prepared.result());
        transaction.commit().await?;
        Ok(outcome)
    }

    /// Loads one complete replacement receipt, or `None` only for an unseen ID.
    pub async fn load(
        &self,
        command_id: DurableCommandId,
    ) -> Result<Option<ReconstitutedReplaceSessionDefaults>, ReplaceSessionDefaultsRepositoryError>
    {
        let mut connection = self.pool.acquire().await?;
        match inspect_registry(&mut connection, command_id).await? {
            None => Ok(None),
            Some(CommandKind::ReplaceSessionDefaults) => {
                load_from_connection(&mut connection, command_id).await
            }
            Some(CommandKind::CreateSession | CommandKind::CreateSessionFromImportedFrontier) => {
                Err(ReplaceSessionDefaultsRepositoryError::DifferentCommandKind { command_id })
            }
            Some(CommandKind::SubmitInput) => {
                Err(ReplaceSessionDefaultsRepositoryError::DifferentCommandKind { command_id })
            }
        }
    }
}

impl ReplaceSessionDefaultsTransaction for ReplaceSessionDefaultsRepository {
    type Error = ReplaceSessionDefaultsRepositoryError;

    async fn handle(
        &mut self,
        command: ReplaceSessionDefaults,
    ) -> Result<ReplaceSessionDefaultsOutcome, Self::Error> {
        let outcome = ReplaceSessionDefaultsRepository::handle(self, command).await?;

        Ok(match outcome {
            ReplaceSessionDefaultsHandlingOutcome::Applied(result) => {
                ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Applied(
                    result,
                ))
            }
            ReplaceSessionDefaultsHandlingOutcome::Rejected(result) => {
                ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Rejected(
                    result,
                ))
            }
            ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse { command_id } => {
                ReplaceSessionDefaultsOutcome::ConflictingReuse { command_id }
            }
        })
    }
}

async fn prepare_against_current(
    connection: &mut PgConnection,
    command: ReplaceSessionDefaults,
) -> Result<PreparedReplaceSessionDefaults, ReplaceSessionDefaultsRepositoryError> {
    match load_session_from_connection(connection, command.session()).await {
        Ok(Some(session)) => command.prepare_against(&session).map_err(|_| {
            ReplaceSessionDefaultsCorruption::Inconsistent("current session ownership").into()
        }),
        Ok(None) => Ok(command.prepare_session_not_found()),
        Err(SessionRepositoryError::Database(error)) => Err(error.into()),
        Err(SessionRepositoryError::Corruption(error)) => {
            Err(ReplaceSessionDefaultsCorruption::CurrentSession(error).into())
        }
    }
}

fn existing_outcome(
    command: &ReplaceSessionDefaults,
    recorded: &ReconstitutedReplaceSessionDefaults,
) -> ReplaceSessionDefaultsHandlingOutcome {
    if command == recorded.command() {
        result_outcome(recorded.result())
    } else {
        ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse {
            command_id: command.command_id(),
        }
    }
}

fn result_outcome(result: ReplaceSessionDefaultsResult) -> ReplaceSessionDefaultsHandlingOutcome {
    match result {
        ReplaceSessionDefaultsResult::Applied(result) => {
            ReplaceSessionDefaultsHandlingOutcome::Applied(result)
        }
        ReplaceSessionDefaultsResult::Rejected(result) => {
            ReplaceSessionDefaultsHandlingOutcome::Rejected(result)
        }
    }
}

async fn insert_defaults_version(
    connection: &mut PgConnection,
    applied: ReplaceSessionDefaultsAppliedResult,
) -> Result<(), ReplaceSessionDefaultsRepositoryError> {
    let installed = applied.installed();
    let selection = encode_selection(installed.defaults().model());
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(session_id_to_uuid(applied.session()))
    .bind(defaults_version_to_numeric(installed.version()))
    .bind(selection.kind)
    .bind(selection.direct)
    .bind(selection.alias)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn insert_typed_record(
    connection: &mut PgConnection,
    prepared: PreparedReplaceSessionDefaults,
) -> Result<(), ReplaceSessionDefaultsRepositoryError> {
    let command = prepared.command();
    let selection = encode_selection(command.replacement().model());
    let encoded_result = encode_result(prepared.result());

    sqlx::query(
        "INSERT INTO replace_session_defaults_command
            (command_id, command_kind, storage_version, session_id,
             expected_current_version, model_selection_kind,
             direct_model_selection_id, model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_installed_version, result_expected_version,
             result_current_version)
         VALUES
            ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(REPLACE_SESSION_DEFAULTS_KIND)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(command.session()))
    .bind(defaults_version_to_numeric(
        command.expected_current_version(),
    ))
    .bind(selection.kind)
    .bind(selection.direct)
    .bind(selection.alias)
    .bind(encoded_result.result_kind)
    .bind(encoded_result.rejection_kind)
    .bind(session_id_to_uuid(encoded_result.session))
    .bind(encoded_result.installed_version)
    .bind(encoded_result.expected_version)
    .bind(encoded_result.current_version)
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
            direct: Some(value.into_uuid()),
            alias: None,
        },
        ModelSelectionRequest::Alias(value) => EncodedSelection {
            kind: "alias",
            direct: None,
            alias: Some(value.into_uuid()),
        },
    }
}

struct EncodedResult {
    result_kind: &'static str,
    rejection_kind: Option<&'static str>,
    session: signalbox_domain::SessionId,
    installed_version: Option<Decimal>,
    expected_version: Option<Decimal>,
    current_version: Option<Decimal>,
}

fn encode_result(result: ReplaceSessionDefaultsResult) -> EncodedResult {
    match result {
        ReplaceSessionDefaultsResult::Applied(result) => EncodedResult {
            result_kind: APPLIED,
            rejection_kind: None,
            session: result.session(),
            installed_version: Some(defaults_version_to_numeric(result.installed().version())),
            expected_version: None,
            current_version: None,
        },
        ReplaceSessionDefaultsResult::Rejected(
            ReplaceSessionDefaultsRejectedResult::SessionNotFound(result),
        ) => EncodedResult {
            result_kind: REJECTED,
            rejection_kind: Some(SESSION_NOT_FOUND),
            session: result.session(),
            installed_version: None,
            expected_version: None,
            current_version: None,
        },
        ReplaceSessionDefaultsResult::Rejected(
            ReplaceSessionDefaultsRejectedResult::CurrentVersionMismatch(result),
        ) => EncodedResult {
            result_kind: REJECTED,
            rejection_kind: Some(CURRENT_VERSION_MISMATCH),
            session: result.session(),
            installed_version: None,
            expected_version: Some(defaults_version_to_numeric(result.expected())),
            current_version: Some(defaults_version_to_numeric(result.current())),
        },
        ReplaceSessionDefaultsResult::Rejected(
            ReplaceSessionDefaultsRejectedResult::VersionExhausted(result),
        ) => EncodedResult {
            result_kind: REJECTED,
            rejection_kind: Some(VERSION_EXHAUSTED),
            session: result.session(),
            installed_version: None,
            expected_version: None,
            current_version: Some(defaults_version_to_numeric(result.current())),
        },
    }
}

async fn load_from_connection(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<ReconstitutedReplaceSessionDefaults>, ReplaceSessionDefaultsRepositoryError> {
    let row = sqlx::query(
        "SELECT
            command.command_kind AS registry_kind,
            command.storage_version AS registry_version,
            typed.command_id AS typed_command_id,
            typed.command_kind AS typed_kind,
            typed.storage_version AS typed_version,
            typed.session_id,
            typed.expected_current_version,
            typed.model_selection_kind AS command_model_kind,
            typed.direct_model_selection_id AS command_direct_id,
            typed.model_alias_id AS command_alias_id,
            typed.result_kind,
            typed.rejection_kind,
            typed.result_session_id,
            typed.result_installed_version,
            typed.result_expected_version,
            typed.result_current_version,
            installed.session_id AS installed_session_id,
            installed.version AS installed_version,
            installed.model_selection_kind AS installed_model_kind,
            installed.direct_model_selection_id AS installed_direct_id,
            installed.model_alias_id AS installed_alias_id
         FROM durable_command AS command
         LEFT JOIN replace_session_defaults_command AS typed
           ON typed.command_id = command.command_id
         LEFT JOIN session_defaults_version AS installed
           ON installed.session_id = typed.result_session_id
          AND installed.version = typed.result_installed_version
         WHERE command.command_id = $1",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .fetch_optional(&mut *connection)
    .await?;

    row.map(|row| decode_complete(row, command_id)).transpose()
}

fn decode_complete(
    row: PgRow,
    command_id: DurableCommandId,
) -> Result<ReconstitutedReplaceSessionDefaults, ReplaceSessionDefaultsRepositoryError> {
    require_spelling(&row, "registry_kind", REPLACE_SESSION_DEFAULTS_KIND)?;
    require_version(&row, "registry_version", STORAGE_VERSION)?;
    let _: Uuid = required(&row, "typed_command_id")?;
    require_spelling(&row, "typed_kind", REPLACE_SESSION_DEFAULTS_KIND)?;
    require_version(&row, "typed_version", STORAGE_VERSION)?;

    let command = ReplaceSessionDefaults::new(
        command_id,
        session_id_from_uuid(required(&row, "session_id")?),
        decode_ordinal(&row, "expected_current_version")?,
        decode_selection(
            required(&row, "command_model_kind")?,
            row.try_get("command_direct_id")?,
            row.try_get("command_alias_id")?,
            "command model selection",
        )?,
    );
    let result_kind: String = required(&row, "result_kind")?;
    let rejection_kind: Option<String> = row.try_get("rejection_kind")?;
    let result_session = session_id_from_uuid(required(&row, "result_session_id")?);
    let installed: Option<Decimal> = row.try_get("result_installed_version")?;
    let expected: Option<Decimal> = row.try_get("result_expected_version")?;
    let current: Option<Decimal> = row.try_get("result_current_version")?;

    let input = match (result_kind.as_str(), rejection_kind.as_deref()) {
        (APPLIED, None) => {
            if expected.is_some() || current.is_some() {
                return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
                    "applied result fields",
                )
                .into());
            }
            let result_version = decode_optional_ordinal(installed, "result_installed_version")?
                .ok_or(ReplaceSessionDefaultsCorruption::Missing(
                    "result_installed_version",
                ))?;
            let installed_session = session_id_from_uuid(required(&row, "installed_session_id")?);
            let installed_version = decode_ordinal(&row, "installed_version")?;
            let installed_defaults = decode_selection(
                required(&row, "installed_model_kind")?,
                row.try_get("installed_direct_id")?,
                row.try_get("installed_alias_id")?,
                "installed model selection",
            )?;
            ReplaceSessionDefaultsReconstitutionInput::applied(
                command,
                result_session,
                result_version,
                installed_session,
                installed_version,
                installed_defaults,
            )
        }
        (REJECTED, Some(SESSION_NOT_FOUND)) => {
            require_absent_result_versions(installed, expected, current)?;
            ReplaceSessionDefaultsReconstitutionInput::rejected_session_not_found(
                command,
                result_session,
            )
        }
        (REJECTED, Some(CURRENT_VERSION_MISMATCH)) => {
            if installed.is_some() {
                return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
                    "mismatch installed version",
                )
                .into());
            }
            let result_expected = decode_optional_ordinal(expected, "result_expected_version")?
                .ok_or(ReplaceSessionDefaultsCorruption::Missing(
                    "result_expected_version",
                ))?;
            let result_current = decode_optional_ordinal(current, "result_current_version")?
                .ok_or(ReplaceSessionDefaultsCorruption::Missing(
                    "result_current_version",
                ))?;
            ReplaceSessionDefaultsReconstitutionInput::rejected_current_version_mismatch(
                command,
                result_session,
                result_expected,
                result_current,
            )
        }
        (REJECTED, Some(VERSION_EXHAUSTED)) => {
            if installed.is_some() || expected.is_some() {
                return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
                    "exhaustion result fields",
                )
                .into());
            }
            let result_current = decode_optional_ordinal(current, "result_current_version")?
                .ok_or(ReplaceSessionDefaultsCorruption::Missing(
                    "result_current_version",
                ))?;
            ReplaceSessionDefaultsReconstitutionInput::rejected_version_exhausted(
                command,
                result_session,
                result_current,
            )
        }
        (APPLIED, Some(_)) | (REJECTED, None) => {
            return Err(
                ReplaceSessionDefaultsCorruption::Inconsistent("terminal result shape").into(),
            );
        }
        (REJECTED, Some(value)) => {
            return Err(ReplaceSessionDefaultsCorruption::Unsupported {
                field: "rejection_kind",
                value: value.to_owned(),
            }
            .into());
        }
        (value, _) => {
            return Err(ReplaceSessionDefaultsCorruption::Unsupported {
                field: "result_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };

    input
        .reconstitute()
        .map_err(|error| ReplaceSessionDefaultsCorruption::Domain(error.failure()).into())
}

fn require_absent_result_versions(
    installed: Option<Decimal>,
    expected: Option<Decimal>,
    current: Option<Decimal>,
) -> Result<(), ReplaceSessionDefaultsRepositoryError> {
    if installed.is_none() && expected.is_none() && current.is_none() {
        Ok(())
    } else {
        Err(
            ReplaceSessionDefaultsCorruption::Inconsistent("session-not-found result fields")
                .into(),
        )
    }
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, ReplaceSessionDefaultsRepositoryError>
where
    for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or_else(|| ReplaceSessionDefaultsCorruption::Missing(field).into())
}

fn require_spelling(
    row: &PgRow,
    field: &'static str,
    expected: &str,
) -> Result<(), ReplaceSessionDefaultsRepositoryError> {
    let actual: String = required(row, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(ReplaceSessionDefaultsCorruption::Unsupported {
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
) -> Result<(), ReplaceSessionDefaultsRepositoryError> {
    let actual: i16 = required(row, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(ReplaceSessionDefaultsCorruption::Unsupported {
            field,
            value: actual.to_string(),
        }
        .into())
    }
}

fn decode_ordinal(
    row: &PgRow,
    field: &'static str,
) -> Result<SessionConfigurationDefaultsVersion, ReplaceSessionDefaultsRepositoryError> {
    let value: Decimal = required(row, field)?;
    defaults_version_from_numeric(value)
        .map_err(|reason| ReplaceSessionDefaultsCorruption::InvalidOrdinal { field, reason }.into())
}

fn decode_optional_ordinal(
    value: Option<Decimal>,
    field: &'static str,
) -> Result<Option<SessionConfigurationDefaultsVersion>, ReplaceSessionDefaultsRepositoryError> {
    value
        .map(|value| {
            defaults_version_from_numeric(value).map_err(|reason| {
                ReplaceSessionDefaultsCorruption::InvalidOrdinal { field, reason }.into()
            })
        })
        .transpose()
}

fn decode_selection(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    field: &'static str,
) -> Result<SessionConfigurationDefaults, ReplaceSessionDefaultsRepositoryError> {
    let model = match (kind.as_str(), direct, alias) {
        ("direct", Some(value), None) => {
            ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(value))
        }
        ("alias", None, Some(value)) => ModelSelectionRequest::Alias(ModelAlias::from_uuid(value)),
        ("direct" | "alias", _, _) => {
            return Err(ReplaceSessionDefaultsCorruption::Inconsistent(field).into());
        }
        _ => {
            return Err(
                ReplaceSessionDefaultsCorruption::Unsupported { field, value: kind }.into(),
            );
        }
    };
    Ok(SessionConfigurationDefaults::new(model))
}

async fn inspect_registry(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<CommandKind>, ReplaceSessionDefaultsRepositoryError> {
    command_registry::inspect(connection, command_id)
        .await
        .map_err(map_registry_error)
}

fn map_registry_error(error: RegistryInspectionError) -> ReplaceSessionDefaultsRepositoryError {
    match error {
        RegistryInspectionError::Database(error) => error.into(),
        RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedKind(value)) => {
            ReplaceSessionDefaultsCorruption::Unsupported {
                field: "registry_kind",
                value,
            }
            .into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedVersion(value)) => {
            ReplaceSessionDefaultsCorruption::Unsupported {
                field: "registry_version",
                value: value.to_string(),
            }
            .into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::MissingTypedRecord(_)) => {
            ReplaceSessionDefaultsCorruption::Missing("typed_command_id").into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::ConflictingTypedRecords) => {
            ReplaceSessionDefaultsCorruption::Inconsistent("typed command family").into()
        }
    }
}
