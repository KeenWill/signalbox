//! Durable configuration reload intent and terminal receipts for process protocol.

use signalbox_domain::DurableCommandId;
use sqlx::{PgConnection, PgPool, Row};

use crate::command_registry::{
    self, CommandKind, RELOAD_CONFIGURATION_KIND, RegistryInspectionError,
};

/// The caller's complete reload request; file contents are retained intent, not caller payload.
#[derive(Clone, Copy, Debug, Eq)]
pub struct ReloadConfiguration {
    /// User-global command identity.
    pub command_id: DurableCommandId,
}

impl PartialEq for ReloadConfiguration {
    fn eq(&self, other: &Self) -> bool {
        let Self { command_id: _ } = self;
        let Self { command_id: _ } = other;
        true
    }
}

/// Checked configuration retained before any runtime effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReloadIntent {
    /// Canonical JSON of the complete checked replacement.
    pub replacement_snapshot: String,
    /// Canonical JSON of the preceding checked snapshot.
    pub prior_snapshot: String,
    /// Digest of checked repository rule sets.
    pub rule_set_digest: [u8; 32],
}

/// Operation that refused a configuration reload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReloadPhase {
    /// Reading a configured file.
    Read,
    /// Checking the replacement.
    Validate,
    /// Activating module rules.
    Activate,
    /// Reconciling configured convergence targets.
    Reconcile,
    /// Installing runtime configuration.
    Install,
}

impl ReloadPhase {
    fn spelling(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Validate => "validate",
            Self::Activate => "activate",
            Self::Reconcile => "reconcile",
            Self::Install => "install",
        }
    }

    fn decode(value: &str) -> Result<Self, ReloadRepositoryError> {
        match value {
            "read" => Ok(Self::Read),
            "validate" => Ok(Self::Validate),
            "activate" => Ok(Self::Activate),
            "reconcile" => Ok(Self::Reconcile),
            "install" => Ok(Self::Install),
            _ => Err(ReloadRepositoryError::Corruption("unknown reload phase")),
        }
    }
}

/// Stored terminal result. Success installs all three reloadable sections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReloadResult {
    /// The complete retained replacement is installed.
    Reloaded,
    /// The running configuration was not replaced.
    Failed {
        /// Operation that refused the reload.
        phase: ReloadPhase,
        /// Bounded sanitized startup-style diagnostic.
        reason: String,
    },
}

/// Registry lookup or competing claim outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReloadLookup {
    /// No command holds this identity.
    Unclaimed,
    /// The reload intent has no terminal result yet.
    Pending,
    /// The exact stored result of this reload.
    Recorded(ReloadResult),
    /// Another command family owns this identity.
    ConflictingReuse,
}

/// Whether this transaction owns installation of a newly retained intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReloadClaim {
    /// This caller committed the intent and may install it.
    Retained,
    /// A standing claim or an atomic pre-effect rejection determines the response.
    Settled(ReloadLookup),
}

#[derive(Debug, signalbox_derive::OperatorError)]
/// Database and fail-closed reload record errors.
pub enum ReloadRepositoryError {
    #[error("reload commit outcome is ambiguous: {field_0}")]
    /// PostgreSQL could not confirm whether a command transaction committed.
    CommitAmbiguous(#[source] sqlx::Error),
    #[error("reload database operation failed: {field_0}")]
    /// The database operation failed.
    Database(#[source] sqlx::Error),
    #[error("reload storage is corrupt: {field_0}")]
    /// Retained records cannot be reconstituted.
    Corruption(&'static str),
    #[error("reload command identity is reserved")]
    /// A sentinel identity cannot be claimed.
    InvalidCommandId,
}

impl From<sqlx::Error> for ReloadRepositoryError {
    fn from(value: sqlx::Error) -> Self {
        Self::Database(value)
    }
}

/// Append-only reload command repository.
#[derive(Clone, Debug)]
pub struct ReloadConfigurationRepository {
    pool: PgPool,
}

impl ReloadConfigurationRepository {
    /// Uses the core-owned pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Inspects the user-global registry before any file or current-state validation.
    pub async fn lookup(
        &self,
        request: ReloadConfiguration,
    ) -> Result<ReloadLookup, ReloadRepositoryError> {
        lookup(&mut *self.pool.acquire().await?, request).await
    }

    /// Commits checked intent, or a pre-effect rejection and its receipt atomically.
    /// Existing and competing claims return their current lookup outcome.
    pub async fn claim(
        &self,
        request: ReloadConfiguration,
        intent: Result<&ReloadIntent, &ReloadResult>,
    ) -> Result<ReloadClaim, ReloadRepositoryError> {
        if request.command_id.as_uuid().is_nil() || request.command_id.as_uuid().is_max() {
            return Err(ReloadRepositoryError::InvalidCommandId);
        }
        let mut tx = self.pool.begin().await?;
        let found = lookup(&mut tx, request).await?;
        if found != ReloadLookup::Unclaimed {
            return Ok(ReloadClaim::Settled(found));
        }
        let issuer = command_registry::issuer_columns(signalbox_domain::CommandPrincipal::Operator);
        let claimed = sqlx::query(
            "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind, issuer_module)
            VALUES ($1, $2, 1, transaction_timestamp(), $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(request.command_id.as_uuid())
        .bind(RELOAD_CONFIGURATION_KIND)
        .bind(issuer.0)
        .bind(issuer.1)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            return lookup(&mut tx, request).await.map(ReloadClaim::Settled);
        }
        let retained = intent.as_ref().ok();
        sqlx::query(
            "INSERT INTO reload_configuration_command
            (command_id, replacement_snapshot, prior_snapshot, rule_set_digest)
            VALUES ($1, $2::text::jsonb, $3::text::jsonb, $4)",
        )
        .bind(request.command_id.as_uuid())
        .bind(retained.map(|v| v.replacement_snapshot.as_str()))
        .bind(retained.map(|v| v.prior_snapshot.as_str()))
        .bind(retained.map(|v| v.rule_set_digest.as_slice()))
        .execute(&mut *tx)
        .await?;
        let outcome = match intent {
            Ok(_) => ReloadClaim::Retained,
            Err(result) => {
                record_result(&mut tx, request, result).await?;
                ReloadClaim::Settled(ReloadLookup::Recorded(result.clone()))
            }
        };
        tx.commit()
            .await
            .map_err(ReloadRepositoryError::CommitAmbiguous)?;
        Ok(outcome)
    }

    /// Appends one terminal receipt; replay must agree with the standing receipt.
    pub async fn finish(
        &self,
        request: ReloadConfiguration,
        result: &ReloadResult,
    ) -> Result<(), ReloadRepositoryError> {
        let mut tx = self.pool.begin().await?;
        record_result(&mut tx, request, result).await?;
        if lookup(&mut tx, request).await? != ReloadLookup::Recorded(result.clone()) {
            return Err(ReloadRepositoryError::Corruption(
                "conflicting terminal reload result",
            ));
        }
        tx.commit()
            .await
            .map_err(ReloadRepositoryError::CommitAmbiguous)?;
        Ok(())
    }

    /// Returns retained undelivered intents for serial startup recovery.
    pub async fn pending(
        &self,
    ) -> Result<Vec<(ReloadConfiguration, ReloadIntent)>, ReloadRepositoryError> {
        let rows = sqlx::query(
            "SELECT c.command_id, c.replacement_snapshot::text AS replacement,
            c.prior_snapshot::text AS prior, c.rule_set_digest FROM reload_configuration_command c
            JOIN durable_command d USING (command_id)
            LEFT JOIN reload_configuration_result r USING (command_id)
            WHERE r.command_id IS NULL ORDER BY d.claimed_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let id: uuid::Uuid = row.try_get("command_id")?;
                let digest: Vec<u8> = row.try_get("rule_set_digest")?;
                let command_id = crate::mapping::durable_command_id_from_uuid(id)
                    .map_err(|_| ReloadRepositoryError::Corruption("invalid reload identity"))?;
                Ok((
                    ReloadConfiguration { command_id },
                    ReloadIntent {
                        replacement_snapshot: row.try_get("replacement")?,
                        prior_snapshot: row.try_get("prior")?,
                        rule_set_digest: digest.try_into().map_err(|_| {
                            ReloadRepositoryError::Corruption("invalid rule digest")
                        })?,
                    },
                ))
            })
            .collect()
    }
}

async fn lookup(
    connection: &mut PgConnection,
    request: ReloadConfiguration,
) -> Result<ReloadLookup, ReloadRepositoryError> {
    match command_registry::inspect(connection, request.command_id)
        .await
        .map_err(|error| match error {
            RegistryInspectionError::Database(error) => ReloadRepositoryError::Database(error),
            RegistryInspectionError::Corruption(_) => {
                ReloadRepositoryError::Corruption("invalid registry record")
            }
        })? {
        None => Ok(ReloadLookup::Unclaimed),
        Some(CommandKind::ReloadConfiguration) => {
            let row = sqlx::query("SELECT outcome, phase, reason FROM reload_configuration_result WHERE command_id = $1")
                .bind(request.command_id.as_uuid()).fetch_optional(connection).await?;
            let Some(row) = row else {
                return Ok(ReloadLookup::Pending);
            };
            let outcome: String = row.try_get("outcome")?;
            Ok(ReloadLookup::Recorded(match outcome.as_str() {
                "reloaded" => ReloadResult::Reloaded,
                "failed" => ReloadResult::Failed {
                    phase: ReloadPhase::decode(&row.try_get::<String, _>("phase")?)?,
                    reason: row.try_get("reason")?,
                },
                _ => return Err(ReloadRepositoryError::Corruption("unknown reload outcome")),
            }))
        }
        Some(_) => Ok(ReloadLookup::ConflictingReuse),
    }
}

async fn record_result(
    connection: &mut PgConnection,
    request: ReloadConfiguration,
    result: &ReloadResult,
) -> Result<(), ReloadRepositoryError> {
    let (outcome, phase, reason) = match result {
        ReloadResult::Reloaded => ("reloaded", None, None),
        ReloadResult::Failed { phase, reason } => {
            ("failed", Some(phase.spelling()), Some(reason.as_str()))
        }
    };
    sqlx::query(
        "INSERT INTO reload_configuration_result (command_id, outcome, phase, reason)
        VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
    )
    .bind(request.command_id.as_uuid())
    .bind(outcome)
    .bind(phase)
    .bind(reason)
    .execute(connection)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_payload_equality_excludes_the_command_identity() {
        let first = ReloadConfiguration {
            command_id: DurableCommandId::from_uuid(uuid::Uuid::from_u128(1)),
        };
        let second = ReloadConfiguration {
            command_id: DurableCommandId::from_uuid(uuid::Uuid::from_u128(2)),
        };
        assert_ne!(first.command_id, second.command_id);
        assert_eq!(first, second);
    }
}
