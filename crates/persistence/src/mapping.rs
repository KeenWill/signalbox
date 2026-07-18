//! Explicit mappings between domain values and PostgreSQL-compatible values.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputId, DurableCommandId, SessionConfigurationDefaultsVersion, SessionId,
    SessionInputPosition, TurnId,
};
use sqlx::types::Uuid;

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

fn positive_u64_from_numeric(value: Decimal) -> Result<u64, PositiveOrdinalMappingError> {
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

/// Why a PostgreSQL `uuid` value is not a valid durable-command identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableCommandIdMappingError {
    /// The value is the nil or max sentinel UUID, which ADR-0033 rejects as an
    /// invalid command identity before canonical command construction.
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
/// Per ADR-0033 the nil and max UUIDs are invalid sentinel-like command
/// identities and are rejected before a `DurableCommandId` is constructed.
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
        SessionInputPosition, TurnId,
    };
    use sqlx::types::Uuid;

    use super::{
        DurableCommandIdMappingError, PositiveOrdinalMappingError, accepted_input_id_from_uuid,
        accepted_input_id_to_uuid, defaults_version_from_numeric, defaults_version_to_numeric,
        durable_command_id_from_uuid, durable_command_id_to_uuid, input_position_from_numeric,
        input_position_to_numeric, session_id_from_uuid, session_id_to_uuid, turn_id_from_uuid,
        turn_id_to_uuid,
    };

    const OUT_OF_U64_RANGE: &str = "18446744073709551616";

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

    /// INV-002 / ADR-0033: the durable-command boundary rejects the nil and max
    /// sentinel UUIDs rather than admitting them as command identities.
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
