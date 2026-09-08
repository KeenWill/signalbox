//! Retained credential-exclusion generations and durable operator clearing.
use crate::command_registry::{self, CommandKind};
use serde::{Deserialize, Serialize};
use signalbox_domain::DurableCommandId;
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialExclusionTarget {
    ProfileQuarantine {
        profile: String,
        record_generation: u64,
    },
    MembershipExclusion {
        #[serde(with = "stored_uuid")]
        pool_policy_id: Uuid,
        profile: String,
        record_generation: u64,
    },
    SessionDisplacement {
        #[serde(with = "stored_uuid")]
        session_id: Uuid,
        #[serde(with = "stored_uuid")]
        pool_policy_id: Uuid,
        profile: String,
        record_generation: u64,
    },
}
impl CredentialExclusionTarget {
    pub fn profile(&self) -> &str {
        match self {
            Self::ProfileQuarantine { profile, .. }
            | Self::MembershipExclusion { profile, .. }
            | Self::SessionDisplacement { profile, .. } => profile,
        }
    }
    pub const fn generation(&self) -> u64 {
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
    fn is_valid(&self) -> bool {
        !self.profile().is_empty()
            && self.profile().len() <= 256
            && self.profile().trim() == self.profile()
            && !self.profile().contains('\0')
            && self.generation() > 0
    }
    fn key(&self) -> (i32, Uuid, Uuid) {
        match self {
            Self::ProfileQuarantine { .. } => (0, Uuid::nil(), Uuid::nil()),
            Self::MembershipExclusion { pool_policy_id, .. } => (1, Uuid::nil(), *pool_policy_id),
            Self::SessionDisplacement {
                session_id,
                pool_policy_id,
                ..
            } => (2, *session_id, *pool_policy_id),
        }
    }
}
#[derive(Clone, Debug)]
pub struct ClearCredentialExclusion {
    pub command_id: DurableCommandId,
    pub target: CredentialExclusionTarget,
}
impl PartialEq for ClearCredentialExclusion {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target
    }
}
impl Eq for ClearCredentialExclusion {}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClearCredentialExclusionOutcome {
    Cleared,
    AlreadyCleared,
    StaleGeneration,
    UnknownCredentialExclusion,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClearCredentialExclusionResult {
    Recorded(ClearCredentialExclusionOutcome),
    ConflictingReuse,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialExclusionPage {
    pub exclusions: Vec<CredentialExclusionTarget>,
    pub next_after: Option<CredentialExclusionTarget>,
}
#[derive(Debug, signalbox_derive::OperatorError)]
pub enum CredentialExclusionError {
    #[error("credential exclusion database failure: {field_0}")]
    Database(#[source] sqlx::Error),
    #[error("credential exclusion commit is ambiguous: {field_0}")]
    CommitAmbiguous(#[source] sqlx::Error),
    #[error("credential exclusion records are inconsistent")]
    Corruption,
    #[error("invalid credential exclusion request")]
    InvalidRequest,
}
impl From<sqlx::Error> for CredentialExclusionError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

pub async fn list(
    pool: &PgPool,
    page_size: u32,
    after: Option<&CredentialExclusionTarget>,
) -> Result<CredentialExclusionPage, CredentialExclusionError> {
    if !(1..=100).contains(&page_size) || after.is_some_and(|target| !target.is_valid()) {
        return Err(CredentialExclusionError::InvalidRequest);
    }
    let (rank, session, policy) = after.map_or(
        (-1, Uuid::nil(), Uuid::nil()),
        CredentialExclusionTarget::key,
    );
    let rows = sqlx::query("SELECT * FROM credential_exclusion_state
        WHERE active AND origin <> 'oauth_refresh'
          AND (CASE kind WHEN 'profile_quarantine' THEN 0 WHEN 'membership_exclusion' THEN 1 ELSE 2 END,
               COALESCE(session_id, '00000000-0000-0000-0000-000000000000'::uuid),
               COALESCE(pool_policy_id, '00000000-0000-0000-0000-000000000000'::uuid), profile COLLATE \"C\", record_generation::numeric)
            > ($1, $2, $3, $4 COLLATE \"C\", $5::numeric)
        ORDER BY CASE kind WHEN 'profile_quarantine' THEN 0 WHEN 'membership_exclusion' THEN 1 ELSE 2 END,
                 session_id NULLS FIRST, pool_policy_id NULLS FIRST, profile COLLATE \"C\", record_generation
        LIMIT $6")
        .bind(rank).bind(session).bind(policy).bind(after.map_or("", CredentialExclusionTarget::profile))
        .bind(after.map_or(0, CredentialExclusionTarget::generation).to_string()).bind(i64::from(page_size) + 1)
        .fetch_all(pool).await?;
    let mut exclusions = rows
        .iter()
        .map(decode_target)
        .collect::<Result<Vec<_>, _>>()?;
    let next_after = if exclusions.len() > page_size as usize {
        exclusions.pop();
        exclusions.last().cloned()
    } else {
        None
    };
    Ok(CredentialExclusionPage {
        exclusions,
        next_after,
    })
}

pub async fn clear(
    pool: &PgPool,
    command: ClearCredentialExclusion,
) -> Result<ClearCredentialExclusionResult, CredentialExclusionError> {
    if !command.target.is_valid() {
        return Err(CredentialExclusionError::InvalidRequest);
    }
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query("INSERT INTO durable_command (command_id, command_kind, storage_version, claimed_at, issuer_kind)
        VALUES ($1, 'clear_credential_exclusion', 1, transaction_timestamp(), 'operator') ON CONFLICT DO NOTHING")
        .bind(command.command_id.into_uuid()).execute(&mut *tx).await?.rows_affected() == 1;
    if !inserted {
        let kind = command_registry::inspect(&mut tx, command.command_id)
            .await
            .map_err(|error| match error {
                command_registry::RegistryInspectionError::Database(error) => {
                    CredentialExclusionError::Database(error)
                }
                command_registry::RegistryInspectionError::Corruption(_) => {
                    CredentialExclusionError::Corruption
                }
            })?;
        if kind != Some(CommandKind::ClearCredentialExclusion) {
            tx.rollback().await?;
            return Ok(ClearCredentialExclusionResult::ConflictingReuse);
        }
        let row = sqlx::query("SELECT target::text, outcome FROM clear_credential_exclusion_command WHERE command_id = $1")
            .bind(command.command_id.into_uuid()).fetch_one(&mut *tx).await?;
        let target: CredentialExclusionTarget = serde_json::from_str(row.try_get("target")?)
            .map_err(|_| CredentialExclusionError::Corruption)?;
        if !target.is_valid() {
            return Err(CredentialExclusionError::Corruption);
        }
        let outcome: ClearCredentialExclusionOutcome =
            serde_json::from_value(serde_json::Value::String(row.try_get("outcome")?))
                .map_err(|_| CredentialExclusionError::Corruption)?;
        tx.rollback().await?;
        return Ok(if target == command.target {
            ClearCredentialExclusionResult::Recorded(outcome)
        } else {
            ClearCredentialExclusionResult::ConflictingReuse
        });
    }
    sqlx::query(crate::lock_inventory::HASHED_TRANSACTION_ADVISORY_LOCK)
        .bind(format!(
            "credential_pool_action_head:{}",
            command.target.profile()
        ))
        .execute(&mut *tx)
        .await?;
    let rows = sqlx::query(
        "SELECT * FROM credential_exclusion_state WHERE profile = $1 ORDER BY record_generation",
    )
    .bind(command.target.profile())
    .fetch_all(&mut *tx)
    .await?;
    let records = rows
        .iter()
        .map(|row| {
            Ok((
                decode_target(row)?,
                row.try_get::<String, _>("origin")?,
                row.try_get::<bool, _>("active")?,
                row.try_get::<bool, _>("cleared")?,
            ))
        })
        .collect::<Result<Vec<_>, CredentialExclusionError>>()?;
    let outcome = match records
        .iter()
        .find(|(target, ..)| *target == command.target)
    {
        Some((_, origin, active, cleared)) if origin != "oauth_refresh" => {
            let newer_active = records.iter().any(|(target, newer_origin, active, _)| {
                *active
                    && newer_origin == origin
                    && target.key() == command.target.key()
                    && target.generation() > command.target.generation()
            });
            if newer_active {
                ClearCredentialExclusionOutcome::StaleGeneration
            } else if *cleared {
                ClearCredentialExclusionOutcome::AlreadyCleared
            } else if *active {
                ClearCredentialExclusionOutcome::Cleared
            } else {
                ClearCredentialExclusionOutcome::UnknownCredentialExclusion
            }
        }
        _ => ClearCredentialExclusionOutcome::UnknownCredentialExclusion,
    };
    let outcome_text =
        serde_json::to_value(outcome).map_err(|_| CredentialExclusionError::Corruption)?;
    sqlx::query("INSERT INTO clear_credential_exclusion_command(command_id, target, outcome) VALUES ($1, $2::jsonb, $3)")
        .bind(command.command_id.into_uuid()).bind(serde_json::to_string(&command.target).map_err(|_| CredentialExclusionError::Corruption)?)
        .bind(outcome_text.as_str().ok_or(CredentialExclusionError::Corruption)?).execute(&mut *tx).await?;
    tx.commit().await.map_err(|error| {
        if crate::commit_failure_is_ambiguous(&error) {
            CredentialExclusionError::CommitAmbiguous(error)
        } else {
            CredentialExclusionError::Database(error)
        }
    })?;
    Ok(ClearCredentialExclusionResult::Recorded(outcome))
}

fn decode_target(
    row: &sqlx::postgres::PgRow,
) -> Result<CredentialExclusionTarget, CredentialExclusionError> {
    let profile: String = row.try_get("profile")?;
    let record_generation = u64::try_from(row.try_get::<i64, _>("record_generation")?)
        .map_err(|_| CredentialExclusionError::Corruption)?;
    let policy: Option<Uuid> = row.try_get("pool_policy_id")?;
    let session: Option<Uuid> = row.try_get("session_id")?;
    match (row.try_get::<String, _>("kind")?.as_str(), policy, session) {
        ("profile_quarantine", None, None) => Ok(CredentialExclusionTarget::ProfileQuarantine {
            profile,
            record_generation,
        }),
        ("membership_exclusion", Some(pool_policy_id), None) => {
            Ok(CredentialExclusionTarget::MembershipExclusion {
                pool_policy_id,
                profile,
                record_generation,
            })
        }
        ("session_displacement", Some(pool_policy_id), Some(session_id)) => {
            Ok(CredentialExclusionTarget::SessionDisplacement {
                session_id,
                pool_policy_id,
                profile,
                record_generation,
            })
        }
        _ => Err(CredentialExclusionError::Corruption),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clear_command_equality_excludes_its_identity() {
        let target = CredentialExclusionTarget::ProfileQuarantine {
            profile: "work".into(),
            record_generation: 1,
        };
        let first = ClearCredentialExclusion {
            command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
            target: target.clone(),
        };
        let second = ClearCredentialExclusion {
            command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
            target,
        };
        assert_eq!(first, second);
    }
}
