//! Durable hub-generation fencing and fenced PostgreSQL pool construction.

use std::{error::Error, fmt};

use rust_decimal::{Decimal, prelude::ToPrimitive};
use sqlx::{
    Connection, PgConnection, PgPool,
    migrate::MigrateError,
    postgres::{PgConnectOptions, PgPoolOptions},
};

use crate::{MIGRATOR, lock_inventory};

/// Migration that first establishes the durable hub-fence singleton.
pub const HUB_FENCE_MIGRATION_VERSION: i64 = 202607230001;

const HUB_FENCE_NAMESPACE: u64 = 1_396_852_273;

/// One positive durable hub-pool generation.
///
/// A retained value cannot call the retired free pool-construction boundary:
///
/// ```compile_fail
/// use signalbox_persistence::hub_fence::{
///     HubFenceGeneration, connect_fenced_pool,
/// };
/// use sqlx::postgres::PgConnectOptions;
///
/// async fn cannot_reopen(
///     options: PgConnectOptions,
///     retired: HubFenceGeneration,
/// ) {
///     let _ = connect_fenced_pool(options, retired).await;
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct HubFenceGeneration(u64);

impl HubFenceGeneration {
    /// Returns the exact positive generation.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Applies migrations only through the migration that establishes fencing.
///
/// A fresh database must cross this boundary before its first fenced pool can
/// exist. Later migrations run through the returned fenced pool.
pub async fn initialize_hub_fence(pool: &PgPool) -> Result<(), MigrateError> {
    MIGRATOR.run_to(HUB_FENCE_MIGRATION_VERSION, pool).await
}

/// A newly advanced fence bound to the exact live session that retained its
/// prior-generation lock.
///
/// Pool construction requires this non-cloneable capability. A copied
/// [`HubFenceGeneration`] is therefore observational only and cannot create
/// database work after the guarded session has been released.
#[derive(Debug)]
#[must_use = "the advanced fence must construct its pool while its session remains live"]
pub struct AdvancedHubFence<'guard> {
    connection: &'guard mut PgConnection,
    generation: HubFenceGeneration,
}

impl AdvancedHubFence<'_> {
    /// Returns the exact positive generation for observation and diagnostics.
    pub const fn generation(&self) -> HubFenceGeneration {
        self.generation
    }

    /// Opens a pool whose every physical session retains this generation's
    /// shared lock before SQLx can make that session available.
    pub async fn connect_pool(&mut self, options: PgConnectOptions) -> Result<PgPool, sqlx::Error> {
        self.connection.ping().await?;
        let generation = self.generation;
        let key = advisory_key(generation.get());
        PgPoolOptions::new()
            .after_connect(move |connection, _metadata| {
                Box::pin(async move {
                    sqlx::query("SELECT pg_advisory_lock_shared($1)")
                        .bind(key)
                        .execute(&mut *connection)
                        .await?;
                    let stored: Option<Decimal> = sqlx::query_scalar(
                        "SELECT generation FROM hub_fence_state WHERE singleton",
                    )
                    .fetch_optional(&mut *connection)
                    .await?;
                    let current = decode_generation(stored.ok_or_else(|| {
                        sqlx::Error::Protocol(HubFenceCorruption::MissingState.to_string())
                    })?)
                    .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
                    if current != generation {
                        return Err(sqlx::Error::Protocol(
                            HubFenceCorruption::GenerationMismatch.to_string(),
                        ));
                    }
                    Ok(())
                })
            })
            .connect_with(options)
            .await
    }
}

/// Waits out every pooled session from the prior generation, then advances the
/// durable singleton exactly once.
///
/// The exclusive prior-generation advisory lock remains held by `connection`
/// and by the returned pool-construction capability for that session's
/// lifetime.
pub async fn advance_hub_fence(
    connection: &mut PgConnection,
) -> Result<AdvancedHubFence<'_>, HubFenceError> {
    let mut transaction = connection.begin().await?;
    let stored: Option<Decimal> = sqlx::query_scalar(lock_inventory::HUB_FENCE_GENERATION)
        .fetch_optional(&mut *transaction)
        .await?;
    let prior = decode_generation(stored.ok_or(HubFenceCorruption::MissingState)?)?.get();
    let next = prior
        .checked_add(1)
        .ok_or(HubFenceCorruption::GenerationExhausted)?;
    let prior_key = advisory_key(prior);

    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(prior_key)
        .execute(&mut *transaction)
        .await?;
    let advanced: Option<Decimal> = sqlx::query_scalar(
        "UPDATE hub_fence_state
            SET generation = $1
          WHERE singleton
            AND generation = $2
        RETURNING generation",
    )
    .bind(Decimal::from(next))
    .bind(Decimal::from(prior))
    .fetch_optional(&mut *transaction)
    .await?;
    let advanced = advanced.ok_or(HubFenceCorruption::StateChanged)?;
    if advanced != Decimal::from(next) {
        return Err(HubFenceCorruption::InvalidGeneration.into());
    }

    let retained: bool = match sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(prior_key)
        .fetch_one(&mut *transaction)
        .await
    {
        Ok(retained) => retained,
        Err(error) => {
            transaction.rollback().await?;
            let _unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
                .bind(prior_key)
                .fetch_one(connection)
                .await?;
            return Err(error.into());
        }
    };
    if !retained {
        return Err(HubFenceCorruption::FenceRetentionFailed.into());
    }
    if let Err(error) = transaction.commit().await {
        let _unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
            .bind(prior_key)
            .fetch_one(connection)
            .await?;
        return Err(error.into());
    }
    Ok(AdvancedHubFence {
        connection,
        generation: HubFenceGeneration(next),
    })
}

/// Waits until no session retains `generation`'s shared fence, then holds its
/// exclusive fence on `connection`.
///
/// Clean shutdown calls this only after globally closing the generation's
/// pool. Retaining the exclusive lock until the singleton guard session closes
/// prevents a detached pool session from escaping guard release.
pub async fn retire_hub_fence_generation(
    connection: &mut PgConnection,
    generation: HubFenceGeneration,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(advisory_key(generation.get()))
        .execute(connection)
        .await?;
    Ok(())
}

fn decode_generation(stored: Decimal) -> Result<HubFenceGeneration, HubFenceCorruption> {
    let generation = stored
        .to_u64()
        .filter(|generation| *generation > 0)
        .ok_or(HubFenceCorruption::InvalidGeneration)?;
    if stored != Decimal::from(generation) {
        return Err(HubFenceCorruption::InvalidGeneration);
    }
    Ok(HubFenceGeneration(generation))
}

fn advisory_key(generation: u64) -> i64 {
    let namespaced = generation ^ (HUB_FENCE_NAMESPACE << 32 | HUB_FENCE_NAMESPACE);
    i64::from_ne_bytes(namespaced.to_ne_bytes())
}

/// A committed fence row that cannot form the required generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HubFenceCorruption {
    /// The singleton fence row was absent.
    MissingState,
    /// The stored generation was not a positive unsigned 64-bit integer.
    InvalidGeneration,
    /// Advancing the unsigned generation would wrap.
    GenerationExhausted,
    /// The locked singleton did not advance from the observed generation.
    StateChanged,
    /// A pool session acquired a lock for a generation other than the durable
    /// current generation.
    GenerationMismatch,
    /// The prior-generation lock could not be retained for the hub lifetime.
    FenceRetentionFailed,
}

impl fmt::Display for HubFenceCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MissingState => "hub fence state is missing",
            Self::InvalidGeneration => "hub fence generation is invalid",
            Self::GenerationExhausted => "hub fence generation is exhausted",
            Self::StateChanged => "hub fence state changed unexpectedly",
            Self::GenerationMismatch => "hub pool generation is no longer current",
            Self::FenceRetentionFailed => "hub fence could not retain the prior generation",
        })
    }
}

impl Error for HubFenceCorruption {}

/// PostgreSQL failure or fail-closed hub-fence corruption.
#[derive(Debug)]
pub enum HubFenceError {
    /// PostgreSQL could not establish or advance fencing.
    Database(sqlx::Error),
    /// Committed fence state could not form the required generation.
    Corruption(HubFenceCorruption),
}

impl fmt::Display for HubFenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(_) => formatter.write_str("hub fence database operation failed"),
            Self::Corruption(error) => write!(formatter, "hub fence corruption: {error}"),
        }
    }
}

impl Error for HubFenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for HubFenceError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<HubFenceCorruption> for HubFenceError {
    fn from(error: HubFenceCorruption) -> Self {
        Self::Corruption(error)
    }
}

#[cfg(test)]
mod tests {
    use super::advisory_key;

    #[test]
    fn fence_advisory_key_encoding_is_stable_across_generations() {
        assert_eq!(advisory_key(1), 5_999_434_831_275_116_080);
        assert_eq!(advisory_key(2), 5_999_434_831_275_116_083);
        assert_eq!(advisory_key(u64::MAX), -5_999_434_831_275_116_082);
    }
}
