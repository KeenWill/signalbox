//! Immutable pool inventories and pre-call exhaustion evidence.
use crate::scalars::deserialize_required_nullable;
use crate::{CanonicalU64, CanonicalUuid};
use serde::{Deserialize, Serialize};

/// Maximum admitted headroom reserve, leaving at least one percent usable.
pub const MAX_HEADROOM_RESERVE_PERCENT: i64 = 99;

/// One reason a configured member could not serve a call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialPoolExclusion {
    /// A credential-wide quarantine; null names an action predating projections.
    ProfileQuarantine {
        /// Exact generation, or null for an action without a projection.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        record_generation: Option<CanonicalU64>,
    },
    /// An exclusion from this immutable pool membership.
    MembershipExclusion {
        /// Exact generation, or null for an action without a projection.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        record_generation: Option<CanonicalU64>,
    },
    /// A displacement of this session from this pool member.
    SessionDisplacement {
        /// Exact generation, or null for an action without a projection.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        record_generation: Option<CanonicalU64>,
    },
    /// A predecessor excluded this credential for the current turn.
    ChainExclusion {
        /// Predecessor whose committed rotation excludes the credential.
        predecessor_model_call_id: CanonicalUuid,
    },
    /// A credential-wide transient exclusion through its recorded reset.
    TransientExclusion {
        /// Observation that recorded the exclusion.
        observation_model_call_id: CanonicalUuid,
    },
    /// The observed capacity was at or below the configured reserve.
    HeadroomReserve {
        /// Observed binding headroom percentage.
        observed_headroom_percent: i64,
        /// Effective configured reserve percentage.
        reserve_percent: u8,
    },
}

/// Frozen evidence for one policy member at the failure commit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialPoolMemberEvidence {
    /// Non-secret configured profile reference.
    pub profile: String,
    /// Latest reset only when every active exclusion has a reset.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub reset_at_unix_ms: Option<i64>,
    /// One exclusion, selected in the committed scope order.
    pub exclusion: CredentialPoolExclusion,
}

/// Validates a complete bounded immutable policy inventory.
pub fn valid_credential_pool_members(policy_members: &[String]) -> bool {
    !policy_members.is_empty()
        && policy_members.len() <= 1024
        && policy_members
            .iter()
            .all(|profile| !profile.is_empty() && profile.len() <= 256)
        && policy_members
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            == policy_members.len()
}

/// Validates complete evidence against the same-ordinal immutable inventory.
pub fn valid_credential_pool_evidence(
    policy_members: &[String],
    members: &[CredentialPoolMemberEvidence],
) -> bool {
    valid_credential_pool_members(policy_members)
        && policy_members.len() == members.len()
        && policy_members.iter().zip(members).all(|(profile, member)| {
            profile == &member.profile
                && match &member.exclusion {
                    CredentialPoolExclusion::ProfileQuarantine { record_generation }
                    | CredentialPoolExclusion::MembershipExclusion { record_generation }
                    | CredentialPoolExclusion::SessionDisplacement { record_generation } => {
                        member.reset_at_unix_ms.is_none()
                            && record_generation.is_none_or(|generation| generation.value() > 0)
                    }
                    CredentialPoolExclusion::HeadroomReserve {
                        observed_headroom_percent,
                        reserve_percent,
                    } => {
                        member.reset_at_unix_ms.is_some()
                            && i64::from(*reserve_percent) <= MAX_HEADROOM_RESERVE_PERCENT
                            && *observed_headroom_percent <= i64::from(*reserve_percent)
                    }
                    CredentialPoolExclusion::ChainExclusion { .. } => {
                        member.reset_at_unix_ms.is_none()
                    }
                    CredentialPoolExclusion::TransientExclusion { .. } => {
                        member.reset_at_unix_ms.is_some()
                    }
                }
        })
}
