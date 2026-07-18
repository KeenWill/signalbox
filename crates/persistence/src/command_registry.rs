//! Closed inspection of the owner-global durable-command registry.

use signalbox_domain::DurableCommandId;
use sqlx::{PgConnection, Row};

use crate::mapping::durable_command_id_to_uuid;

pub(crate) const CREATE_SESSION_KIND: &str = "create_session";
pub(crate) const REPLACE_SESSION_DEFAULTS_KIND: &str = "replace_session_defaults";
pub(crate) const SUBMIT_INPUT_KIND: &str = "submit_input";
const STORAGE_VERSION: i16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommandKind {
    CreateSession,
    ReplaceSessionDefaults,
    SubmitInput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RegistryCorruption {
    UnsupportedKind(String),
    UnsupportedVersion(i16),
    MissingTypedRecord(CommandKind),
    ConflictingTypedRecords,
}

#[derive(Debug)]
pub(crate) enum RegistryInspectionError {
    Database(sqlx::Error),
    Corruption(RegistryCorruption),
}

pub(crate) async fn inspect(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<CommandKind>, RegistryInspectionError> {
    let row = sqlx::query(
        "SELECT
            command.command_kind,
            command.storage_version,
            create_command.command_id IS NOT NULL AS has_create_session,
            defaults_command.command_id IS NOT NULL AS has_replace_session_defaults,
            input_command.command_id IS NOT NULL AS has_submit_input
         FROM durable_command AS command
         LEFT JOIN create_session_command AS create_command
           ON create_command.command_id = command.command_id
         LEFT JOIN replace_session_defaults_command AS defaults_command
           ON defaults_command.command_id = command.command_id
         LEFT JOIN submit_input_command AS input_command
           ON input_command.command_id = command.command_id
         WHERE command.command_id = $1",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .fetch_optional(&mut *connection)
    .await
    .map_err(RegistryInspectionError::Database)?;

    row.map(|row| {
        let version: i16 = row
            .try_get("storage_version")
            .map_err(RegistryInspectionError::Database)?;
        if version != STORAGE_VERSION {
            return Err(RegistryInspectionError::Corruption(
                RegistryCorruption::UnsupportedVersion(version),
            ));
        }

        let spelling: String = row
            .try_get("command_kind")
            .map_err(RegistryInspectionError::Database)?;
        let kind = match spelling.as_str() {
            CREATE_SESSION_KIND => CommandKind::CreateSession,
            REPLACE_SESSION_DEFAULTS_KIND => CommandKind::ReplaceSessionDefaults,
            SUBMIT_INPUT_KIND => CommandKind::SubmitInput,
            _ => {
                return Err(RegistryInspectionError::Corruption(
                    RegistryCorruption::UnsupportedKind(spelling),
                ));
            }
        };
        let has_create: bool = row
            .try_get("has_create_session")
            .map_err(RegistryInspectionError::Database)?;
        let has_defaults: bool = row
            .try_get("has_replace_session_defaults")
            .map_err(RegistryInspectionError::Database)?;
        let has_input: bool = row
            .try_get("has_submit_input")
            .map_err(RegistryInspectionError::Database)?;

        match (kind, has_create, has_defaults, has_input) {
            (CommandKind::CreateSession, true, false, false)
            | (CommandKind::ReplaceSessionDefaults, false, true, false)
            | (CommandKind::SubmitInput, false, false, true) => Ok(kind),
            (CommandKind::CreateSession, false, false, false)
            | (CommandKind::ReplaceSessionDefaults, false, false, false)
            | (CommandKind::SubmitInput, false, false, false) => Err(
                RegistryInspectionError::Corruption(RegistryCorruption::MissingTypedRecord(kind)),
            ),
            _ => Err(RegistryInspectionError::Corruption(
                RegistryCorruption::ConflictingTypedRecords,
            )),
        }
    })
    .transpose()
}
