//! Closed inspection of the owner-global durable-command registry.

use signalbox_domain::DurableCommandId;
use sqlx::{PgConnection, Row};

use crate::mapping::durable_command_id_to_uuid;

pub(crate) const CREATE_SESSION_KIND: &str = "create_session";
pub(crate) const CREATE_SESSION_FROM_IMPORTED_FRONTIER_KIND: &str =
    "create_session_from_imported_frontier";
pub(crate) const REPLACE_SESSION_DEFAULTS_KIND: &str = "replace_session_defaults";
pub(crate) const REPLACE_SESSION_METADATA_KIND: &str = "replace_session_metadata";
pub(crate) const SUBMIT_INPUT_KIND: &str = "submit_input";
pub(crate) const DECIDE_TOOL_REQUEST_KIND: &str = "decide_tool_request";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommandKind {
    CreateSession,
    CreateSessionFromImportedFrontier,
    ReplaceSessionDefaults,
    ReplaceSessionMetadata,
    SubmitInput,
    DecideToolRequest,
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
            imported_create_command.command_id IS NOT NULL
                AS has_create_session_from_imported_frontier,
            defaults_command.command_id IS NOT NULL AS has_replace_session_defaults,
            metadata_command.command_id IS NOT NULL AS has_replace_session_metadata,
            input_command.command_id IS NOT NULL AS has_submit_input,
            tool_command.command_id IS NOT NULL AS has_decide_tool_request
         FROM durable_command AS command
         LEFT JOIN create_session_command AS create_command
           ON create_command.command_id = command.command_id
         LEFT JOIN create_session_from_imported_frontier_command
                AS imported_create_command
           ON imported_create_command.command_id = command.command_id
         LEFT JOIN replace_session_defaults_command AS defaults_command
           ON defaults_command.command_id = command.command_id
         LEFT JOIN replace_session_metadata_command AS metadata_command
           ON metadata_command.command_id = command.command_id
         LEFT JOIN submit_input_command AS input_command
           ON input_command.command_id = command.command_id
         LEFT JOIN decide_tool_request_command AS tool_command
           ON tool_command.command_id = command.command_id
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
        let spelling: String = row
            .try_get("command_kind")
            .map_err(RegistryInspectionError::Database)?;
        let kind = match spelling.as_str() {
            CREATE_SESSION_KIND => CommandKind::CreateSession,
            CREATE_SESSION_FROM_IMPORTED_FRONTIER_KIND => {
                CommandKind::CreateSessionFromImportedFrontier
            }
            REPLACE_SESSION_DEFAULTS_KIND => CommandKind::ReplaceSessionDefaults,
            REPLACE_SESSION_METADATA_KIND => CommandKind::ReplaceSessionMetadata,
            SUBMIT_INPUT_KIND => CommandKind::SubmitInput,
            DECIDE_TOOL_REQUEST_KIND => CommandKind::DecideToolRequest,
            _ => {
                return Err(RegistryInspectionError::Corruption(
                    RegistryCorruption::UnsupportedKind(spelling),
                ));
            }
        };
        let version_supported = match kind {
            CommandKind::CreateSession
            | CommandKind::CreateSessionFromImportedFrontier
            | CommandKind::ReplaceSessionDefaults => matches!(version, 1 | 2 | 3),
            CommandKind::ReplaceSessionMetadata
            | CommandKind::SubmitInput
            | CommandKind::DecideToolRequest => version == 1,
        };
        if !version_supported {
            return Err(RegistryInspectionError::Corruption(
                RegistryCorruption::UnsupportedVersion(version),
            ));
        }
        let has_create: bool = row
            .try_get("has_create_session")
            .map_err(RegistryInspectionError::Database)?;
        let has_imported_create: bool = row
            .try_get("has_create_session_from_imported_frontier")
            .map_err(RegistryInspectionError::Database)?;
        let has_defaults: bool = row
            .try_get("has_replace_session_defaults")
            .map_err(RegistryInspectionError::Database)?;
        let has_metadata: bool = row
            .try_get("has_replace_session_metadata")
            .map_err(RegistryInspectionError::Database)?;
        let has_input: bool = row
            .try_get("has_submit_input")
            .map_err(RegistryInspectionError::Database)?;
        let has_tool: bool = row
            .try_get("has_decide_tool_request")
            .map_err(RegistryInspectionError::Database)?;

        let present: Vec<CommandKind> = [
            (CommandKind::CreateSession, has_create),
            (
                CommandKind::CreateSessionFromImportedFrontier,
                has_imported_create,
            ),
            (CommandKind::ReplaceSessionDefaults, has_defaults),
            (CommandKind::ReplaceSessionMetadata, has_metadata),
            (CommandKind::SubmitInput, has_input),
            (CommandKind::DecideToolRequest, has_tool),
        ]
        .into_iter()
        .filter_map(|(candidate, is_present)| is_present.then_some(candidate))
        .collect();
        sole_typed_record(kind, &present).map_err(RegistryInspectionError::Corruption)
    })
    .transpose()
}

/// The registry's consistency rule: exactly one typed record exists, and it
/// is the registered kind.
fn sole_typed_record(
    kind: CommandKind,
    present: &[CommandKind],
) -> Result<CommandKind, RegistryCorruption> {
    match present {
        [only] if *only == kind => Ok(kind),
        [] => Err(RegistryCorruption::MissingTypedRecord(kind)),
        _ => Err(RegistryCorruption::ConflictingTypedRecords),
    }
}

#[cfg(test)]
mod tests {
    use super::{CommandKind, RegistryCorruption, sole_typed_record};

    #[test]
    fn a_sole_typed_record_of_the_registered_kind_is_consistent() {
        assert_eq!(
            sole_typed_record(CommandKind::SubmitInput, &[CommandKind::SubmitInput]),
            Ok(CommandKind::SubmitInput)
        );
    }

    #[test]
    fn no_typed_record_is_missing_record_corruption() {
        assert_eq!(
            sole_typed_record(CommandKind::SubmitInput, &[]),
            Err(RegistryCorruption::MissingTypedRecord(
                CommandKind::SubmitInput
            ))
        );
    }

    #[test]
    fn a_sole_typed_record_of_another_kind_is_conflicting_records_corruption() {
        assert_eq!(
            sole_typed_record(CommandKind::SubmitInput, &[CommandKind::CreateSession]),
            Err(RegistryCorruption::ConflictingTypedRecords)
        );
    }

    #[test]
    fn multiple_typed_records_are_conflicting_records_corruption() {
        assert_eq!(
            sole_typed_record(
                CommandKind::SubmitInput,
                &[CommandKind::CreateSession, CommandKind::SubmitInput]
            ),
            Err(RegistryCorruption::ConflictingTypedRecords)
        );
    }
}
