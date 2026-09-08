//! Immutable pool inventories and pre-call exhaustion evidence.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One reason a configured member could not serve a call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialPoolExclusion {
    /// A credential-wide quarantine; null names an action predating projections.
    ProfileQuarantine {
        /// Exact generation, or null for an action without a projection.
        record_generation: Option<u64>,
    },
    /// An exclusion from this immutable pool membership.
    MembershipExclusion {
        /// Exact generation, or null for an action without a projection.
        record_generation: Option<u64>,
    },
    /// A displacement of this session from this pool member.
    SessionDisplacement {
        /// Exact generation, or null for an action without a projection.
        record_generation: Option<u64>,
    },
    /// A predecessor excluded this credential for the current turn.
    ChainExclusion {
        /// Predecessor whose committed rotation excludes the credential.
        #[serde(with = "stored_uuid")]
        predecessor_model_call_id: Uuid,
    },
    /// A credential-wide transient exclusion through its recorded reset.
    TransientExclusion {
        /// Observation that recorded the exclusion.
        #[serde(with = "stored_uuid")]
        observation_model_call_id: Uuid,
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
    pub reset_at_unix_ms: Option<i64>,
    /// One exclusion, selected in the committed scope order.
    pub exclusion: CredentialPoolExclusion,
}

/// Complete persisted pre-call exhaustion, shared by snapshot and live reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialPoolExhaustion {
    /// Failed turn.
    pub turn_id: Uuid,
    /// Terminal physical attempt.
    pub terminal_attempt_id: Uuid,
    /// Exact terminal frontier.
    pub terminal_frontier_id: Uuid,
    /// Semantic failure marker.
    pub failure_entry_id: Uuid,
    /// Immutable policy identity.
    pub pool_policy_id: Uuid,
    /// Complete member inventory.
    pub policy_members: Vec<String>,
    /// Frozen same-ordinal evidence.
    pub members: Vec<CredentialPoolMemberEvidence>,
}

/// Storage failures while authenticating a frozen policy or exhaustion.
#[derive(Debug)]
pub enum CredentialPoolEvidenceError {
    /// PostgreSQL could not complete the read.
    Database(sqlx::Error),
    /// Retained evidence is partial, foreign, stale, or malformed.
    Corruption,
}

/// Reconstitutes the immutable header and each ordered membership record.
pub async fn load_policy(
    connection: &mut sqlx::PgConnection,
    policy_id: Uuid,
) -> Result<Option<Vec<String>>, CredentialPoolEvidenceError> {
    use sqlx::Row;
    let Some(definition) = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT definition FROM credential_pool_policy WHERE pool_policy_id = $1",
    )
    .bind(policy_id)
    .fetch_optional(&mut *connection)
    .await?
    else {
        return Ok(None);
    };
    let records = sqlx::query("SELECT ordinal, profile, priority, headroom_reserve_percent FROM credential_pool_policy_member WHERE pool_policy_id = $1 ORDER BY ordinal").bind(policy_id).fetch_all(&mut *connection).await?;
    let expected = definition
        .get("members")
        .and_then(serde_json::Value::as_array)
        .ok_or(CredentialPoolEvidenceError::Corruption)?;
    if expected.is_empty() || expected.len() > 1024 || expected.len() != records.len() {
        return Err(CredentialPoolEvidenceError::Corruption);
    }
    let mut profiles = Vec::with_capacity(records.len());
    for (ordinal, row) in records.into_iter().enumerate() {
        let profile: String = row.try_get("profile")?;
        let value = serde_json::json!({"profile": profile, "priority": row.try_get::<i64,_>("priority")?, "headroom_reserve_percent": row.try_get::<Option<i16>,_>("headroom_reserve_percent")?});
        if row.try_get::<i32, _>("ordinal")?
            != i32::try_from(ordinal).map_err(|_| CredentialPoolEvidenceError::Corruption)?
            || expected[ordinal] != value
            || profile.is_empty()
            || profile.len() > 256
            || profiles.contains(&profile)
        {
            return Err(CredentialPoolEvidenceError::Corruption);
        }
        profiles.push(profile);
    }
    Ok(Some(profiles))
}

/// Reads a policy only if the named turn references its exact identity.
pub async fn read_policy(
    connection: &mut sqlx::PgConnection,
    session: Uuid,
    turn: Uuid,
    policy: Uuid,
) -> Result<Option<Vec<String>>, CredentialPoolEvidenceError> {
    let references: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM credential_pool_terminal_exhaustion WHERE session_id = $1 AND turn_id = $2 AND pool_policy_id = $3) OR EXISTS (SELECT 1 FROM model_call AS call JOIN model_call_credential_pool_policy AS policy USING (model_call_id) WHERE call.session_id = $1 AND call.turn_id = $2 AND policy.pool_policy_id = $3)").bind(session).bind(turn).bind(policy).fetch_one(&mut *connection).await?;
    if !references {
        return Ok(None);
    }
    load_policy(connection, policy)
        .await?
        .ok_or(CredentialPoolEvidenceError::Corruption)
        .map(Some)
}

/// Authenticates a pre-call failure and reconstitutes its frozen evidence.
pub async fn load(
    connection: &mut sqlx::PgConnection,
    session: Uuid,
    turn: Uuid,
) -> Result<Option<CredentialPoolExhaustion>, CredentialPoolEvidenceError> {
    use sqlx::Row;
    let Some(row) = sqlx::query("SELECT exhausted.terminal_attempt_id, exhausted.pool_policy_id, lifecycle.terminal_frontier_id, event.failure_entry_id FROM credential_pool_terminal_exhaustion AS exhausted LEFT JOIN turn_lifecycle AS lifecycle ON lifecycle.session_id = exhausted.session_id AND lifecycle.turn_id = exhausted.turn_id AND lifecycle.terminal_attempt_id = exhausted.terminal_attempt_id AND lifecycle.terminal_model_call_id IS NULL AND lifecycle.state_kind = 'terminal' AND lifecycle.terminal_disposition_kind = 'failed' LEFT JOIN turn_terminal_outbox_event AS event ON event.session_id = lifecycle.session_id AND event.turn_id = lifecycle.turn_id AND event.terminal_frontier_id = lifecycle.terminal_frontier_id AND event.disposition_kind = 'failed' WHERE exhausted.session_id = $1 AND exhausted.turn_id = $2 AND exhausted.terminal_model_call_id IS NULL AND exhausted.terminal_attempt_id = (SELECT terminal_attempt_id FROM turn_lifecycle WHERE session_id = $1 AND turn_id = $2)").bind(session).bind(turn).fetch_optional(&mut *connection).await? else { return Ok(None); };
    let terminal_frontier_id = row
        .try_get::<Option<Uuid>, _>("terminal_frontier_id")?
        .ok_or(CredentialPoolEvidenceError::Corruption)?;
    let failure_entry_id = row
        .try_get::<Option<Uuid>, _>("failure_entry_id")?
        .ok_or(CredentialPoolEvidenceError::Corruption)?;
    let attempt: Uuid = row.try_get("terminal_attempt_id")?;
    let policy: Option<Uuid> = row.try_get("pool_policy_id")?;
    let policy = policy.ok_or(CredentialPoolEvidenceError::Corruption)?;
    let policy_members = load_policy(connection, policy)
        .await?
        .ok_or(CredentialPoolEvidenceError::Corruption)?;
    let valid: bool = sqlx::query_scalar("SELECT credential_pool_exhaustion_evidence_valid($1)")
        .bind(attempt)
        .fetch_one(&mut *connection)
        .await?;
    if !valid {
        return Err(CredentialPoolEvidenceError::Corruption);
    }
    let rows = sqlx::query("SELECT ordinal, profile, evidence FROM credential_pool_exhaustion_member WHERE terminal_attempt_id = $1 AND pool_policy_id = $2 ORDER BY ordinal").bind(attempt).bind(policy).fetch_all(&mut *connection).await?;
    if rows.len() != policy_members.len() {
        return Err(CredentialPoolEvidenceError::Corruption);
    }
    let mut members = Vec::with_capacity(rows.len());
    for (ordinal, row) in rows.into_iter().enumerate() {
        let member: CredentialPoolMemberEvidence = serde_json::from_value(row.try_get("evidence")?)
            .map_err(|_| CredentialPoolEvidenceError::Corruption)?;
        if row.try_get::<i32, _>("ordinal")?
            != i32::try_from(ordinal).map_err(|_| CredentialPoolEvidenceError::Corruption)?
            || member.profile != policy_members[ordinal]
            || member.profile != row.try_get::<String, _>("profile")?
        {
            return Err(CredentialPoolEvidenceError::Corruption);
        }
        members.push(member);
    }
    Ok(Some(CredentialPoolExhaustion {
        turn_id: turn,
        terminal_attempt_id: attempt,
        terminal_frontier_id,
        failure_entry_id,
        pool_policy_id: policy,
        policy_members,
        members,
    }))
}

mod stored_uuid {
    use serde::{Deserialize, Deserializer, Serializer};
    use uuid::Uuid;
    pub fn serialize<S: Serializer>(value: &Uuid, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Uuid, D::Error> {
        let text = String::deserialize(deserializer)?;
        let value = Uuid::parse_str(&text).map_err(serde::de::Error::custom)?;
        if text != value.to_string() {
            return Err(serde::de::Error::custom("noncanonical stored UUID"));
        }
        Ok(value)
    }
}

impl From<sqlx::Error> for CredentialPoolEvidenceError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}
impl std::fmt::Display for CredentialPoolEvidenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Database(_) => "credential pool evidence database failure",
            Self::Corruption => "invalid credential pool evidence",
        })
    }
}
impl std::error::Error for CredentialPoolEvidenceError {}
