//! Explicit mappings between domain values and PostgreSQL-compatible values.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputId, DangerousToolAutoApproval, DurableCommandId,
    SessionConfigurationDefaultsVersion, SessionId, SessionInputPosition, ToolAttemptId,
    ToolPermissionDefault, ToolRequestId, TurnId,
};
use signalbox_tools_plan::PlanStatus;
use sqlx::types::Uuid;

/// Closed durable-command kinds stored by the user-global registry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DurableCommandKind {
    /// Session creation.
    CreateSession,
    /// Session creation from an imported frontier.
    CreateSessionFromImportedFrontier,
    /// Session-default replacement.
    ReplaceSessionDefaults,
    /// Session-metadata replacement.
    ReplaceSessionMetadata,
    /// Session input submission.
    SubmitInput,
    /// Tool-request decision.
    DecideToolRequest,
    /// Review-workflow command.
    ReviewWorkflow,
    /// Review-orchestration command.
    ReviewOrchestration,
    /// Session context compaction.
    CompactSession,
}

/// Encodes a durable-command kind as its closed PostgreSQL spelling.
pub(crate) const fn durable_command_kind_to_str(value: DurableCommandKind) -> &'static str {
    match value {
        DurableCommandKind::CreateSession => "create_session",
        DurableCommandKind::CreateSessionFromImportedFrontier => {
            "create_session_from_imported_frontier"
        }
        DurableCommandKind::ReplaceSessionDefaults => "replace_session_defaults",
        DurableCommandKind::ReplaceSessionMetadata => "replace_session_metadata",
        DurableCommandKind::SubmitInput => "submit_input",
        DurableCommandKind::DecideToolRequest => "decide_tool_request",
        DurableCommandKind::ReviewWorkflow => "review_workflow",
        DurableCommandKind::ReviewOrchestration => "review_orchestration",
        DurableCommandKind::CompactSession => "compact_session",
    }
}

/// Decodes a closed durable-command kind from its PostgreSQL spelling.
pub(crate) fn durable_command_kind_from_str(value: &str) -> Option<DurableCommandKind> {
    match value {
        "create_session" => Some(DurableCommandKind::CreateSession),
        "create_session_from_imported_frontier" => {
            Some(DurableCommandKind::CreateSessionFromImportedFrontier)
        }
        "replace_session_defaults" => Some(DurableCommandKind::ReplaceSessionDefaults),
        "replace_session_metadata" => Some(DurableCommandKind::ReplaceSessionMetadata),
        "submit_input" => Some(DurableCommandKind::SubmitInput),
        "decide_tool_request" => Some(DurableCommandKind::DecideToolRequest),
        "review_workflow" => Some(DurableCommandKind::ReviewWorkflow),
        "review_orchestration" => Some(DurableCommandKind::ReviewOrchestration),
        "compact_session" => Some(DurableCommandKind::CompactSession),
        _ => None,
    }
}

/// Why a PostgreSQL `numeric(20, 0)` value is not a positive domain ordinal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PositiveOrdinalMappingError {
    /// The value is zero or negative.
    NonPositive,
    /// The value has a nonzero fractional component.
    Fractional,
    /// The positive integral value exceeds `u64::MAX`.
    OutOfRange,
}

impl fmt::Display for PositiveOrdinalMappingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NonPositive => "ordinal must be positive",
            Self::Fractional => "ordinal must not have a fractional component",
            Self::OutOfRange => "ordinal exceeds the u64 range",
        };
        formatter.write_str(message)
    }
}

impl Error for PositiveOrdinalMappingError {}

/// Encodes a defaults version as its exact PostgreSQL `numeric(20, 0)` value.
pub fn defaults_version_to_numeric(value: SessionConfigurationDefaultsVersion) -> Decimal {
    Decimal::from(value.as_u64())
}

/// Decodes a checked defaults version from a PostgreSQL `numeric(20, 0)` value.
pub fn defaults_version_from_numeric(
    value: Decimal,
) -> Result<SessionConfigurationDefaultsVersion, PositiveOrdinalMappingError> {
    let ordinal = positive_u64_from_numeric(value)?;
    SessionConfigurationDefaultsVersion::try_from_u64(ordinal)
        .ok_or(PositiveOrdinalMappingError::NonPositive)
}

/// Encodes an input position as its exact PostgreSQL `numeric(20, 0)` value.
pub fn input_position_to_numeric(value: SessionInputPosition) -> Decimal {
    Decimal::from(value.as_u64())
}

/// Decodes a checked input position from a PostgreSQL `numeric(20, 0)` value.
pub fn input_position_from_numeric(
    value: Decimal,
) -> Result<SessionInputPosition, PositiveOrdinalMappingError> {
    let ordinal = positive_u64_from_numeric(value)?;
    SessionInputPosition::try_from_u64(ordinal).ok_or(PositiveOrdinalMappingError::NonPositive)
}

/// Encodes the dangerous blanket posture as its closed storage spelling.
pub fn dangerous_tool_auto_approval_to_str(value: DangerousToolAutoApproval) -> &'static str {
    match value {
        DangerousToolAutoApproval::Disabled => "disabled",
        DangerousToolAutoApproval::ApproveAll => "approve_all",
    }
}

/// Decodes the closed dangerous blanket storage spelling.
pub fn dangerous_tool_auto_approval_from_str(value: &str) -> Option<DangerousToolAutoApproval> {
    match value {
        "disabled" => Some(DangerousToolAutoApproval::Disabled),
        "approve_all" => Some(DangerousToolAutoApproval::ApproveAll),
        _ => None,
    }
}

/// Encodes a tool permission default as its closed PostgreSQL spelling.
pub(crate) const fn tool_permission_default_to_str(value: ToolPermissionDefault) -> &'static str {
    match value {
        ToolPermissionDefault::Auto => "auto",
        ToolPermissionDefault::Confirm => "confirm",
        ToolPermissionDefault::AlwaysConfirm => "always_confirm",
    }
}

/// Decodes a tool permission default from its closed PostgreSQL spelling.
pub(crate) fn tool_permission_default_from_str(value: &str) -> Option<ToolPermissionDefault> {
    match value {
        "auto" => Some(ToolPermissionDefault::Auto),
        "confirm" => Some(ToolPermissionDefault::Confirm),
        "always_confirm" => Some(ToolPermissionDefault::AlwaysConfirm),
        _ => None,
    }
}

/// Closed plan-event kinds stored by PostgreSQL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlanEventStorageKind {
    /// Entry creation.
    Created,
    /// Text revision.
    TextRevised,
    /// Status change.
    StatusChanged,
}

/// Encodes a plan-event kind as its closed PostgreSQL spelling.
pub(crate) const fn plan_event_kind_to_str(value: PlanEventStorageKind) -> &'static str {
    match value {
        PlanEventStorageKind::Created => "created",
        PlanEventStorageKind::TextRevised => "text_revised",
        PlanEventStorageKind::StatusChanged => "status_changed",
    }
}

/// Decodes a closed plan-event kind from its PostgreSQL spelling.
pub(crate) fn plan_event_kind_from_str(value: &str) -> Option<PlanEventStorageKind> {
    match value {
        "created" => Some(PlanEventStorageKind::Created),
        "text_revised" => Some(PlanEventStorageKind::TextRevised),
        "status_changed" => Some(PlanEventStorageKind::StatusChanged),
        _ => None,
    }
}

/// Encodes the closed durable plan-status spelling.
pub(crate) const fn plan_status_to_str(value: PlanStatus) -> &'static str {
    match value {
        PlanStatus::Pending => "pending",
        PlanStatus::InProgress => "in_progress",
        PlanStatus::Completed => "completed",
        PlanStatus::Abandoned => "abandoned",
    }
}

/// Decodes the closed durable plan-status spelling.
pub(crate) fn plan_status_from_str(value: &str) -> Option<PlanStatus> {
    match value {
        "pending" => Some(PlanStatus::Pending),
        "in_progress" => Some(PlanStatus::InProgress),
        "completed" => Some(PlanStatus::Completed),
        "abandoned" => Some(PlanStatus::Abandoned),
        _ => None,
    }
}

pub(crate) fn positive_u64_from_numeric(
    value: Decimal,
) -> Result<u64, PositiveOrdinalMappingError> {
    if !value.fract().is_zero() {
        return Err(PositiveOrdinalMappingError::Fractional);
    }
    if value <= Decimal::ZERO {
        return Err(PositiveOrdinalMappingError::NonPositive);
    }
    u64::try_from(value).map_err(|_| PositiveOrdinalMappingError::OutOfRange)
}

/// Encodes a session identity for a PostgreSQL `uuid` column.
pub fn session_id_to_uuid(value: SessionId) -> Uuid {
    value.into_uuid()
}

/// Decodes a session identity from a PostgreSQL `uuid` column.
pub fn session_id_from_uuid(value: Uuid) -> SessionId {
    SessionId::from_uuid(value)
}

/// Encodes an accepted-input identity for a PostgreSQL `uuid` column.
pub fn accepted_input_id_to_uuid(value: AcceptedInputId) -> Uuid {
    value.into_uuid()
}

/// Decodes an accepted-input identity from a PostgreSQL `uuid` column.
pub fn accepted_input_id_from_uuid(value: Uuid) -> AcceptedInputId {
    AcceptedInputId::from_uuid(value)
}

/// Encodes a turn identity for a PostgreSQL `uuid` column.
pub fn turn_id_to_uuid(value: TurnId) -> Uuid {
    value.into_uuid()
}

/// Decodes a turn identity from a PostgreSQL `uuid` column.
pub fn turn_id_from_uuid(value: Uuid) -> TurnId {
    TurnId::from_uuid(value)
}

/// Encodes a logical tool-request identity for a PostgreSQL `uuid` column.
pub fn tool_request_id_to_uuid(value: ToolRequestId) -> Uuid {
    value.into_uuid()
}

/// Decodes a logical tool-request identity from a PostgreSQL `uuid` column.
pub fn tool_request_id_from_uuid(value: Uuid) -> ToolRequestId {
    ToolRequestId::from_uuid(value)
}

/// Encodes a physical tool-attempt identity for a PostgreSQL `uuid` column.
pub fn tool_attempt_id_to_uuid(value: ToolAttemptId) -> Uuid {
    value.into_uuid()
}

/// Decodes a physical tool-attempt identity from a PostgreSQL `uuid` column.
pub fn tool_attempt_id_from_uuid(value: Uuid) -> ToolAttemptId {
    ToolAttemptId::from_uuid(value)
}

/// Why a PostgreSQL `uuid` value is not a valid durable-command identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableCommandIdMappingError {
    /// The value is the nil or max sentinel UUID, rejected as an invalid
    /// command identity before canonical command construction
    /// (docs/spec/identity-and-commands.md).
    SentinelUuid,
}

impl fmt::Display for DurableCommandIdMappingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::SentinelUuid => "durable-command identity must not be the nil or max UUID",
        };
        formatter.write_str(message)
    }
}

impl Error for DurableCommandIdMappingError {}

/// Encodes a durable-command identity for a PostgreSQL `uuid` column.
pub fn durable_command_id_to_uuid(value: DurableCommandId) -> Uuid {
    value.into_uuid()
}

/// Decodes a checked durable-command identity from a PostgreSQL `uuid` column.
///
/// Per docs/spec/identity-and-commands.md, the nil and max UUIDs are invalid
/// sentinel-like command identities and are rejected before a
/// `DurableCommandId` is constructed.
pub fn durable_command_id_from_uuid(
    value: Uuid,
) -> Result<DurableCommandId, DurableCommandIdMappingError> {
    if value == Uuid::nil() || value == Uuid::max() {
        return Err(DurableCommandIdMappingError::SentinelUuid);
    }
    Ok(DurableCommandId::from_uuid(value))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rust_decimal::Decimal;
    use signalbox_domain::{
        AcceptedInputId, DurableCommandId, SessionConfigurationDefaultsVersion, SessionId,
        SessionInputPosition, ToolPermissionDefault, TurnId,
    };
    use sqlx::types::Uuid;

    use super::{
        DurableCommandIdMappingError, DurableCommandKind, PlanEventStorageKind,
        PositiveOrdinalMappingError, accepted_input_id_from_uuid, accepted_input_id_to_uuid,
        defaults_version_from_numeric, defaults_version_to_numeric, durable_command_id_from_uuid,
        durable_command_id_to_uuid, durable_command_kind_from_str, durable_command_kind_to_str,
        input_position_from_numeric, input_position_to_numeric, plan_event_kind_from_str,
        plan_event_kind_to_str, session_id_from_uuid, session_id_to_uuid,
        tool_permission_default_from_str, tool_permission_default_to_str, turn_id_from_uuid,
        turn_id_to_uuid,
    };

    const OUT_OF_U64_RANGE: &str = "18446744073709551616";

    #[test]
    fn plan_event_kind_mapping_is_closed() {
        assert_eq!(
            plan_event_kind_from_str(plan_event_kind_to_str(PlanEventStorageKind::Created)),
            Some(PlanEventStorageKind::Created)
        );
        assert_eq!(
            plan_event_kind_from_str(plan_event_kind_to_str(PlanEventStorageKind::TextRevised)),
            Some(PlanEventStorageKind::TextRevised)
        );
        assert_eq!(
            plan_event_kind_from_str(plan_event_kind_to_str(PlanEventStorageKind::StatusChanged)),
            Some(PlanEventStorageKind::StatusChanged)
        );
        assert_eq!(plan_event_kind_from_str("unknown"), None);
    }

    #[test]
    fn compact_session_command_kind_mapping_is_closed() {
        assert_eq!(
            durable_command_kind_to_str(DurableCommandKind::CompactSession),
            "compact_session"
        );
        assert_eq!(
            durable_command_kind_from_str("compact_session"),
            Some(DurableCommandKind::CompactSession)
        );
        assert_eq!(durable_command_kind_from_str("unknown"), None);
    }

    #[test]
    fn always_confirm_permission_mapping_is_closed() {
        let encoded = tool_permission_default_to_str(ToolPermissionDefault::AlwaysConfirm);
        let decoded = tool_permission_default_from_str(encoded)
            .expect("the additive permission encoding is canonical");

        assert_eq!(encoded, "always_confirm");
        assert_eq!(decoded, ToolPermissionDefault::AlwaysConfirm);
        assert_eq!(tool_permission_default_from_str("unknown"), None);
    }

    /// INV-002: PostgreSQL numeric values are decoded and checked before a
    /// domain defaults version exists.
    #[test]
    fn inv002_defaults_version_numeric_boundary() {
        assert_eq!(
            defaults_version_from_numeric(Decimal::ZERO),
            Err(PositiveOrdinalMappingError::NonPositive)
        );
        assert_eq!(
            defaults_version_from_numeric(Decimal::NEGATIVE_ONE),
            Err(PositiveOrdinalMappingError::NonPositive)
        );
        assert_eq!(
            defaults_version_from_numeric(Decimal::new(15, 1)),
            Err(PositiveOrdinalMappingError::Fractional)
        );
        assert_eq!(
            defaults_version_from_numeric(Decimal::ONE),
            Ok(SessionConfigurationDefaultsVersion::first())
        );

        let maximum = Decimal::from(u64::MAX);
        let mapped = defaults_version_from_numeric(maximum).expect("maximum must round-trip");
        assert_eq!(mapped.as_u64(), u64::MAX);
        assert_eq!(defaults_version_to_numeric(mapped), maximum);

        let out_of_range = Decimal::from_str(OUT_OF_U64_RANGE).expect("representable decimal");
        assert_eq!(
            defaults_version_from_numeric(out_of_range),
            Err(PositiveOrdinalMappingError::OutOfRange)
        );
    }

    /// INV-002: PostgreSQL numeric values are decoded and checked before a
    /// domain input position exists.
    #[test]
    fn inv002_input_position_numeric_boundary() {
        assert_eq!(
            input_position_from_numeric(Decimal::ZERO),
            Err(PositiveOrdinalMappingError::NonPositive)
        );
        assert_eq!(
            input_position_from_numeric(Decimal::NEGATIVE_ONE),
            Err(PositiveOrdinalMappingError::NonPositive)
        );
        assert_eq!(
            input_position_from_numeric(Decimal::new(15, 1)),
            Err(PositiveOrdinalMappingError::Fractional)
        );
        assert_eq!(
            input_position_from_numeric(Decimal::ONE),
            Ok(SessionInputPosition::first())
        );

        let maximum = Decimal::from(u64::MAX);
        let mapped = input_position_from_numeric(maximum).expect("maximum must round-trip");
        assert_eq!(mapped.as_u64(), u64::MAX);
        assert_eq!(input_position_to_numeric(mapped), maximum);

        let out_of_range = Decimal::from_str(OUT_OF_U64_RANGE).expect("representable decimal");
        assert_eq!(
            input_position_from_numeric(out_of_range),
            Err(PositiveOrdinalMappingError::OutOfRange)
        );
    }

    /// INV-002: each CreateSession identity kind crosses the persistence
    /// boundary through its own typed conversion.
    #[test]
    fn inv002_create_session_identity_mappings_remain_kind_specific() {
        let session_uuid = Uuid::from_u128(1);
        let command_uuid = Uuid::from_u128(2);

        let session = session_id_from_uuid(session_uuid);
        let command = durable_command_id_from_uuid(command_uuid).expect("non-sentinel command");

        assert_eq!(session, SessionId::from_uuid(session_uuid));
        assert_eq!(command, DurableCommandId::from_uuid(command_uuid));
        assert_eq!(session_id_to_uuid(session), session_uuid);
        assert_eq!(durable_command_id_to_uuid(command), command_uuid);
    }

    /// INV-002: accepted-input and future-turn identities cross the SQL
    /// boundary through distinct mappings even though both use native UUIDs.
    #[test]
    fn inv002_submit_input_identity_mappings_remain_kind_specific() {
        let accepted_uuid = Uuid::from_u128(3);
        let turn_uuid = Uuid::from_u128(4);

        let accepted = accepted_input_id_from_uuid(accepted_uuid);
        let turn = turn_id_from_uuid(turn_uuid);

        assert_eq!(accepted, AcceptedInputId::from_uuid(accepted_uuid));
        assert_eq!(turn, TurnId::from_uuid(turn_uuid));
        assert_eq!(accepted_input_id_to_uuid(accepted), accepted_uuid);
        assert_eq!(turn_id_to_uuid(turn), turn_uuid);
    }

    /// INV-002: the durable-command boundary rejects the nil and max sentinel
    /// UUIDs rather than admitting them as command identities.
    #[test]
    fn inv002_durable_command_mapping_rejects_sentinel_uuids() {
        assert_eq!(
            durable_command_id_from_uuid(Uuid::nil()),
            Err(DurableCommandIdMappingError::SentinelUuid)
        );
        assert_eq!(
            durable_command_id_from_uuid(Uuid::max()),
            Err(DurableCommandIdMappingError::SentinelUuid)
        );

        let valid = Uuid::from_u128(7);
        assert_eq!(
            durable_command_id_from_uuid(valid),
            Ok(DurableCommandId::from_uuid(valid))
        );
    }
}
