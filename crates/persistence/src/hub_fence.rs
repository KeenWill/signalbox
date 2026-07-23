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

/// Waits out every pooled session from the prior generation, then advances the
/// durable singleton exactly once.
///
/// The exclusive prior-generation advisory lock remains held by `connection`
/// for that session's lifetime.
pub async fn advance_hub_fence(
    connection: &mut PgConnection,
) -> Result<HubFenceGeneration, HubFenceError> {
    let mut transaction = connection.begin().await?;
    let stored: Option<Decimal> = sqlx::query_scalar(lock_inventory::HUB_FENCE_GENERATION)
        .fetch_optional(&mut *transaction)
        .await?;
    let prior = stored
        .ok_or(HubFenceCorruption::MissingState)?
        .to_u64()
        .filter(|generation| *generation > 0)
        .ok_or(HubFenceCorruption::InvalidGeneration)?;
    let next = prior
        .checked_add(1)
        .ok_or(HubFenceCorruption::GenerationExhausted)?;

    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(advisory_key(prior))
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
    if advanced.to_u64() != Some(next) {
        return Err(HubFenceCorruption::InvalidGeneration.into());
    }
    transaction.commit().await?;
    Ok(HubFenceGeneration(next))
}

/// Opens a pool whose every physical session retains the shared lock for
/// `generation` before SQLx can make that session available.
pub async fn connect_fenced_pool(
    options: PgConnectOptions,
    generation: HubFenceGeneration,
) -> Result<PgPool, sqlx::Error> {
    let key = advisory_key(generation.get());
    PgPoolOptions::new()
        .after_connect(move |connection, _metadata| {
            Box::pin(async move {
                sqlx::query("SELECT pg_advisory_lock_shared($1)")
                    .bind(key)
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
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
}

impl fmt::Display for HubFenceCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MissingState => "hub fence state is missing",
            Self::InvalidGeneration => "hub fence generation is invalid",
            Self::GenerationExhausted => "hub fence generation is exhausted",
            Self::StateChanged => "hub fence state changed unexpectedly",
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
