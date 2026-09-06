//! Closed inspection of the user-global durable-command registry.

use std::sync::LazyLock;

use signalbox_domain::{CommandPrincipal, DurableCommandId};
use sqlx::{PgConnection, Row};

pub(crate) use crate::mapping::DurableCommandKind as CommandKind;
use crate::mapping::{
    command_principal_module_to_str, command_principal_to_str, durable_command_id_to_uuid,
    durable_command_kind_from_str, durable_command_kind_to_str,
};

pub(crate) const CREATE_SESSION_KIND: &str =
    durable_command_kind_to_str(CommandKind::CreateSession);
pub(crate) const CREATE_SESSION_FROM_IMPORTED_FRONTIER_KIND: &str =
    durable_command_kind_to_str(CommandKind::CreateSessionFromImportedFrontier);
pub(crate) const REPLACE_SESSION_DEFAULTS_KIND: &str =
    durable_command_kind_to_str(CommandKind::ReplaceSessionDefaults);
pub(crate) const REPLACE_SESSION_METADATA_KIND: &str =
    durable_command_kind_to_str(CommandKind::ReplaceSessionMetadata);
pub(crate) const SUBMIT_INPUT_KIND: &str = durable_command_kind_to_str(CommandKind::SubmitInput);
pub(crate) const DECIDE_TOOL_REQUEST_KIND: &str =
    durable_command_kind_to_str(CommandKind::DecideToolRequest);
pub(crate) const OVERRIDE_DENIED_TOOL_REQUEST_KIND: &str =
    durable_command_kind_to_str(CommandKind::OverrideDeniedToolRequest);
pub(crate) const REVIEW_WORKFLOW_KIND: &str =
    durable_command_kind_to_str(CommandKind::ReviewWorkflow);
pub(crate) const REVIEW_ORCHESTRATION_KIND: &str =
    durable_command_kind_to_str(CommandKind::ReviewOrchestration);
pub(crate) const COMPACT_SESSION_KIND: &str =
    durable_command_kind_to_str(CommandKind::CompactSession);
pub(crate) const GOAL_KIND: &str = durable_command_kind_to_str(CommandKind::Goal);
pub(crate) const UPDATE_SESSION_PLACEMENT_KIND: &str =
    durable_command_kind_to_str(CommandKind::UpdateSessionPlacement);
pub(crate) const REGISTER_WORKSPACE_KIND: &str =
    durable_command_kind_to_str(CommandKind::RegisterWorkspace);
pub(crate) const MINT_GIT_REMOTE_KIND: &str =
    durable_command_kind_to_str(CommandKind::MintGitRemote);
pub(crate) const WITHDRAW_GIT_REMOTE_KIND: &str =
    durable_command_kind_to_str(CommandKind::WithdrawGitRemote);
pub(crate) const SESSION_LIFECYCLE_KIND: &str =
    durable_command_kind_to_str(CommandKind::SessionLifecycle);

/// Returns the envelope's `issuer_kind` and `issuer_module` spellings.
pub(crate) const fn issuer_columns(
    principal: CommandPrincipal,
) -> (&'static str, Option<&'static str>) {
    (
        command_principal_to_str(principal),
        command_principal_module_to_str(principal),
    )
}

pub(crate) const fn create_session_storage_version_is_supported(version: i16) -> bool {
    matches!(version, 1..=4 | 6..=7)
}

pub(crate) const fn imported_session_storage_version_is_supported(version: i16) -> bool {
    matches!(version, 1..=3 | 5)
}

#[derive(Clone, Copy)]
struct CommandKindDefinition {
    kind: CommandKind,
    spelling: &'static str,
    typed_table: &'static str,
    minimum_version: i16,
    maximum_version: i16,
}

const COMMAND_KIND_DEFINITIONS: [CommandKindDefinition; 16] = [
    CommandKindDefinition {
        kind: CommandKind::CreateSession,
        spelling: CREATE_SESSION_KIND,
        typed_table: "create_session_command",
        minimum_version: 1,
        maximum_version: 7,
    },
    CommandKindDefinition {
        kind: CommandKind::CreateSessionFromImportedFrontier,
        spelling: CREATE_SESSION_FROM_IMPORTED_FRONTIER_KIND,
        typed_table: "create_session_from_imported_frontier_command",
        minimum_version: 1,
        maximum_version: 5,
    },
    CommandKindDefinition {
        kind: CommandKind::ReplaceSessionDefaults,
        spelling: REPLACE_SESSION_DEFAULTS_KIND,
        typed_table: "replace_session_defaults_command",
        minimum_version: 1,
        maximum_version: 4,
    },
    CommandKindDefinition {
        kind: CommandKind::ReplaceSessionMetadata,
        spelling: REPLACE_SESSION_METADATA_KIND,
        typed_table: "replace_session_metadata_command",
        minimum_version: 1,
        maximum_version: 1,
    },
    CommandKindDefinition {
        kind: CommandKind::SubmitInput,
        spelling: SUBMIT_INPUT_KIND,
        typed_table: "submit_input_command",
        minimum_version: 3,
        maximum_version: 3,
    },
    CommandKindDefinition {
        kind: CommandKind::DecideToolRequest,
        spelling: DECIDE_TOOL_REQUEST_KIND,
        typed_table: "decide_tool_request_command",
        minimum_version: 1,
        maximum_version: 1,
    },
    CommandKindDefinition {
        kind: CommandKind::OverrideDeniedToolRequest,
        spelling: OVERRIDE_DENIED_TOOL_REQUEST_KIND,
        typed_table: "override_denied_tool_request_command",
        minimum_version: 1,
        maximum_version: 1,
    },
    CommandKindDefinition {
        kind: CommandKind::ReviewWorkflow,
        spelling: REVIEW_WORKFLOW_KIND,
        typed_table: "review_workflow_command",
        minimum_version: 1,
        maximum_version: 1,
    },
    CommandKindDefinition {
        kind: CommandKind::ReviewOrchestration,
        spelling: REVIEW_ORCHESTRATION_KIND,
        typed_table: "review_orchestration_command",
        minimum_version: 1,
        maximum_version: 1,
    },
    CommandKindDefinition {
        kind: CommandKind::CompactSession,
        spelling: COMPACT_SESSION_KIND,
        typed_table: "compact_session_command",
        minimum_version: 1,
        maximum_version: 1,
    },
    CommandKindDefinition {
        kind: CommandKind::Goal,
        spelling: GOAL_KIND,
        typed_table: "goal_command",
        minimum_version: 1,
        maximum_version: 1,
    },
    CommandKindDefinition {
        kind: CommandKind::UpdateSessionPlacement,
        spelling: UPDATE_SESSION_PLACEMENT_KIND,
        typed_table: "update_session_placement_command",
        minimum_version: 1,
        maximum_version: 1,
    },
    CommandKindDefinition {
        kind: CommandKind::RegisterWorkspace,
        spelling: REGISTER_WORKSPACE_KIND,
        typed_table: "workspace",
        minimum_version: 1,
        maximum_version: 1,
    },
    CommandKindDefinition {
        kind: CommandKind::MintGitRemote,
        spelling: MINT_GIT_REMOTE_KIND,
        typed_table: "configured_git_remote_mint",
        minimum_version: 1,
        maximum_version: 1,
    },
    CommandKindDefinition {
        kind: CommandKind::WithdrawGitRemote,
        spelling: WITHDRAW_GIT_REMOTE_KIND,
        typed_table: "configured_git_remote_withdrawal",
        minimum_version: 1,
        maximum_version: 1,
    },
    CommandKindDefinition {
        kind: CommandKind::SessionLifecycle,
        spelling: SESSION_LIFECYCLE_KIND,
        typed_table: "session_lifecycle_command",
        minimum_version: 1,
        maximum_version: 1,
    },
];

impl CommandKindDefinition {
    const fn supports_version(self, version: i16) -> bool {
        match self.kind {
            CommandKind::CreateSession => create_session_storage_version_is_supported(version),
            CommandKind::CreateSessionFromImportedFrontier => {
                imported_session_storage_version_is_supported(version)
            }
            _ => version >= self.minimum_version && version <= self.maximum_version,
        }
    }
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
    let query = inspection_query();
    let row = sqlx::query(query)
        .bind(durable_command_id_to_uuid(command_id))
        .fetch_optional(&mut *connection)
        .await
        .map_err(RegistryInspectionError::Database)?;
    if row.is_none() {
        let recovered: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM review_orchestration_command_recovery
                 WHERE command_id = $1
            )",
        )
        .bind(durable_command_id_to_uuid(command_id))
        .fetch_one(&mut *connection)
        .await
        .map_err(RegistryInspectionError::Database)?;
        if recovered {
            return Ok(Some(CommandKind::ReviewOrchestration));
        }
    }

    row.map(|row| {
        let version: i16 = row
            .try_get("storage_version")
            .map_err(RegistryInspectionError::Database)?;
        let spelling: String = row
            .try_get("command_kind")
            .map_err(RegistryInspectionError::Database)?;
        let Some(kind) = durable_command_kind_from_str(&spelling) else {
            return Err(RegistryInspectionError::Corruption(
                RegistryCorruption::UnsupportedKind(spelling),
            ));
        };
        let definition = COMMAND_KIND_DEFINITIONS
            .iter()
            .copied()
            .find(|candidate| candidate.kind == kind)
            .ok_or_else(|| {
                RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedKind(spelling))
            })?;
        if !definition.supports_version(version) {
            return Err(RegistryInspectionError::Corruption(
                RegistryCorruption::UnsupportedVersion(version),
            ));
        }

        let mut present = Vec::new();
        for candidate in COMMAND_KIND_DEFINITIONS {
            let is_present: bool = row
                .try_get(candidate.spelling)
                .map_err(RegistryInspectionError::Database)?;
            if is_present {
                present.push(candidate.kind);
            }
        }
        sole_typed_record(definition.kind, &present).map_err(RegistryInspectionError::Corruption)
    })
    .transpose()
}

fn inspection_query() -> &'static str {
    static QUERY: LazyLock<String> = LazyLock::new(|| {
        // Every interpolated identifier comes from the closed compile-time
        // table; command data remains a bind parameter and never enters the
        // SQL text.
        let mut query = String::from("SELECT command.command_kind, command.storage_version");
        for definition in COMMAND_KIND_DEFINITIONS {
            query.push_str(", (EXISTS (SELECT 1 FROM ");
            query.push_str(definition.typed_table);
            query.push_str(" AS typed WHERE typed.command_id = command.command_id)");
            if definition.kind == CommandKind::ReviewOrchestration {
                query.push_str(
                    " OR EXISTS (SELECT 1 FROM review_orchestration_command_intent AS intent
                       WHERE intent.command_id = command.command_id)",
                );
            }
            query.push_str(") AS ");
            query.push_str(definition.spelling);
        }
        query.push_str(" FROM durable_command AS command WHERE command.command_id = $1");
        query
    });
    QUERY.as_str()
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
    use std::collections::BTreeSet;

    use super::{
        COMMAND_KIND_DEFINITIONS, CommandKind, RegistryCorruption,
        create_session_storage_version_is_supported, imported_session_storage_version_is_supported,
        sole_typed_record,
    };

    fn database_admitted_command_kinds() -> BTreeSet<String> {
        // The baseline renders the constraint the way `pg_dump` deparses it:
        // inline in `CREATE TABLE` and with an `= ANY (ARRAY[...])` list rather
        // than the `IN (...)` the retired chain spelled by hand.
        const CONSTRAINT: &str = "CONSTRAINT durable_command_kind_closed";
        const KIND_LIST: &str = "command_kind = ANY (ARRAY[";
        let definition = crate::MIGRATOR
            .iter()
            .filter_map(|migration| {
                migration
                    .sql
                    .as_str()
                    .find(CONSTRAINT)
                    .map(|offset| &migration.sql.as_str()[offset..])
            })
            .next_back()
            .expect("one migration defines the durable command kind constraint");
        let list = definition
            .split_once(KIND_LIST)
            .expect("the current command-kind constraint has an admitted-kind list")
            .1
            .split_once(')')
            .expect("the current command-kind list is closed")
            .0;
        list.split('\'')
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect()
    }

    fn registry_admitted_command_kinds() -> BTreeSet<String> {
        COMMAND_KIND_DEFINITIONS
            .iter()
            .map(|definition| definition.spelling.to_owned())
            .collect()
    }

    #[test]
    fn database_and_registry_admit_the_same_command_kinds() {
        assert_eq!(
            database_admitted_command_kinds(),
            registry_admitted_command_kinds()
        );
    }

    #[test]
    fn create_session_version_five_is_rejected_until_its_payload_is_decoded() {
        assert!(create_session_storage_version_is_supported(4));
        assert!(!create_session_storage_version_is_supported(5));
        assert!(create_session_storage_version_is_supported(6));
        assert!(create_session_storage_version_is_supported(7));
        assert!(imported_session_storage_version_is_supported(3));
        assert!(!imported_session_storage_version_is_supported(4));
        assert!(imported_session_storage_version_is_supported(5));
    }

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
