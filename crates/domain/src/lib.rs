//! Core domain boundary for Signalbox.
//!
//! Domain identities are distinct from storage, protocol, and framework types.
//! Lifecycle and product behavior remain intentionally deferred.

use uuid::Uuid;

/// Identifies one owner-global, durably handled command submission.
///
/// This identity does not prove that the command was applied.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct DurableCommandId(Uuid);

impl DurableCommandId {
    /// Creates a durable-command identity from its UUID value.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Borrows the UUID value.
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Returns the UUID value.
    pub const fn into_uuid(self) -> Uuid {
        self.0
    }
}

/// Identifies one durable, independently browsable conversation.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct SessionId(Uuid);

impl SessionId {
    /// Creates a session identity from its UUID value.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Borrows the UUID value.
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Returns the UUID value.
    pub const fn into_uuid(self) -> Uuid {
        self.0
    }
}

/// Identifies one user submission durably accepted with a delivery treatment.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct AcceptedInputId(Uuid);

impl AcceptedInputId {
    /// Creates an accepted-input identity from its UUID value.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Borrows the UUID value.
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Returns the UUID value.
    pub const fn into_uuid(self) -> Uuid {
        self.0
    }
}

/// Identifies one logical request for a conversational outcome.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct TurnId(Uuid);

impl TurnId {
    /// Creates a turn identity from its UUID value.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Borrows the UUID value.
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Returns the UUID value.
    pub const fn into_uuid(self) -> Uuid {
        self.0
    }
}

/// Identifies one physical orchestration tenure for a turn.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct TurnAttemptId(Uuid);

impl TurnAttemptId {
    /// Creates a turn-attempt identity from its UUID value.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Borrows the UUID value.
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Returns the UUID value.
    pub const fn into_uuid(self) -> Uuid {
        self.0
    }
}

/// Identifies one hub authorization to attempt a provider interaction.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct ModelCallId(Uuid);

impl ModelCallId {
    /// Creates a model-call identity from its UUID value.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Borrows the UUID value.
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Returns the UUID value.
    pub const fn into_uuid(self) -> Uuid {
        self.0
    }
}

/// Identifies one logical request for a normalized tool operation.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct ToolRequestId(Uuid);

impl ToolRequestId {
    /// Creates a tool-request identity from its UUID value.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Borrows the UUID value.
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Returns the UUID value.
    pub const fn into_uuid(self) -> Uuid {
        self.0
    }
}

/// Identifies one physical effort to execute a tool request.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct ToolAttemptId(Uuid);

impl ToolAttemptId {
    /// Creates a tool-attempt identity from its UUID value.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Borrows the UUID value.
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Returns the UUID value.
    pub const fn into_uuid(self) -> Uuid {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;
    use std::collections::HashSet;

    use super::{
        AcceptedInputId, DurableCommandId, ModelCallId, SessionId, ToolAttemptId, ToolRequestId,
        TurnAttemptId, TurnId,
    };
    use uuid::Uuid;

    macro_rules! assert_round_trip {
        ($identity:ty, $first:expr, $second:expr) => {{
            let first_uuid = $first;
            let second_uuid = $second;
            let first_id = <$identity>::from_uuid(first_uuid);
            let equal_id = <$identity>::from_uuid(first_uuid);
            let different_id = <$identity>::from_uuid(second_uuid);

            assert!(first_id == equal_id);
            assert!(first_id != different_id);
            assert!(first_id.as_uuid() == &first_uuid);
            assert!(first_id.into_uuid() == first_uuid);
        }};
    }

    #[test]
    fn identity_kinds_are_distinct_types() {
        let kinds = HashSet::from([
            TypeId::of::<DurableCommandId>(),
            TypeId::of::<SessionId>(),
            TypeId::of::<AcceptedInputId>(),
            TypeId::of::<TurnId>(),
            TypeId::of::<TurnAttemptId>(),
            TypeId::of::<ModelCallId>(),
            TypeId::of::<ToolRequestId>(),
            TypeId::of::<ToolAttemptId>(),
        ]);

        assert_eq!(kinds.len(), 8);
    }

    #[test]
    fn identity_values_round_trip_without_changing_equality() {
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);

        assert_round_trip!(DurableCommandId, first, second);
        assert_round_trip!(SessionId, first, second);
        assert_round_trip!(AcceptedInputId, first, second);
        assert_round_trip!(TurnId, first, second);
        assert_round_trip!(TurnAttemptId, first, second);
        assert_round_trip!(ModelCallId, first, second);
        assert_round_trip!(ToolRequestId, first, second);
        assert_round_trip!(ToolAttemptId, first, second);
    }
}
