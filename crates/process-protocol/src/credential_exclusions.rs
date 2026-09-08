//! Clearable credential-exclusion targets and outcomes.

use crate::{CanonicalU64, CanonicalUuid, FrameValidationError};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// Exact retained exclusion generation; chain exclusions are turn-local.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialExclusionTarget {
    /// Profile-wide quarantine from a clearable origin.
    ProfileQuarantine {
        profile: String,
        record_generation: CanonicalU64,
    },
    /// Exclusion from one immutable pool policy.
    MembershipExclusion {
        pool_policy_id: CanonicalUuid,
        profile: String,
        record_generation: CanonicalU64,
    },
    /// Displacement within one session and immutable pool policy.
    SessionDisplacement {
        session_id: CanonicalUuid,
        pool_policy_id: CanonicalUuid,
        profile: String,
        record_generation: CanonicalU64,
    },
}
impl CredentialExclusionTarget {
    /// Returns the configured profile reference.
    pub fn profile(&self) -> &str {
        match self {
            Self::ProfileQuarantine { profile, .. }
            | Self::MembershipExclusion { profile, .. }
            | Self::SessionDisplacement { profile, .. } => profile,
        }
    }
    /// Returns the exact retained generation.
    pub const fn generation(&self) -> CanonicalU64 {
        match self {
            Self::ProfileQuarantine {
                record_generation, ..
            }
            | Self::MembershipExclusion {
                record_generation, ..
            }
            | Self::SessionDisplacement {
                record_generation, ..
            } => *record_generation,
        }
    }
    pub(crate) fn validate(&self) -> Result<(), FrameValidationError> {
        let profile = self.profile();
        if profile.is_empty()
            || profile.len() > 256
            || profile.trim() != profile
            || profile.contains('\0')
            || self.generation().value() == 0
        {
            return Err(FrameValidationError::CredentialExclusionShape);
        }
        Ok(())
    }
    fn scope_key(&self) -> (u8, uuid::Uuid, uuid::Uuid) {
        match self {
            Self::ProfileQuarantine { .. } => (0, uuid::Uuid::nil(), uuid::Uuid::nil()),
            Self::MembershipExclusion { pool_policy_id, .. } => {
                (1, uuid::Uuid::nil(), pool_policy_id.into_uuid())
            }
            Self::SessionDisplacement {
                session_id,
                pool_policy_id,
                ..
            } => (2, session_id.into_uuid(), pool_policy_id.into_uuid()),
        }
    }
}
impl Ord for CredentialExclusionTarget {
    fn cmp(&self, other: &Self) -> Ordering {
        self.scope_key()
            .cmp(&other.scope_key())
            .then_with(|| self.profile().cmp(other.profile()))
            .then_with(|| self.generation().cmp(&other.generation()))
    }
}
impl PartialOrd for CredentialExclusionTarget {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Successful durable clear receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialExclusionClearOutcome {
    /// This command cleared the named generation.
    Cleared,
    /// A prior command already cleared the named generation.
    AlreadyCleared,
}
