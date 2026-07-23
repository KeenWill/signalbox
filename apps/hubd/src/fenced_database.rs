//! Single-hub acquisition, durable generation advance, and fenced pool startup.

use std::{error::Error, fmt, mem};

use signalbox_persistence::{
    hub_fence::{
        HubFenceError, HubFenceGeneration, advance_hub_fence, connect_fenced_pool,
        initialize_hub_fence,
    },
    production_connection_options,
};
use sqlx::{
    PgPool,
    migrate::MigrateError,
    postgres::{PgConnectOptions, PgPoolOptions},
};

use crate::{SingleHubGuard, SingleHubGuardError};

/// One guarded hub database incarnation and its fenced application pool.
#[derive(Debug)]
#[must_use = "the fenced pool must be closed before releasing the singleton guard"]
pub struct FencedHubDatabase {
    guard: Option<SingleHubGuard>,
    pool: PgPool,
    generation: HubFenceGeneration,
}

impl FencedHubDatabase {
    /// Opens a production database, establishes the singleton guard, fences the
    /// prior generation, and returns only the new fenced pool.
    pub async fn connect_production(database_url: &str) -> Result<Self, FencedHubDatabaseError> {
        let options = production_connection_options(database_url)
            .map_err(FencedHubDatabaseError::ParseOptions)?;
        Self::connect_with(options).await
    }

    /// Establishes one guarded incarnation using already parsed connection
    /// options. This is also the local integration-test construction boundary.
    pub async fn connect_with(options: PgConnectOptions) -> Result<Self, FencedHubDatabaseError> {
        let bootstrap = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .map_err(FencedHubDatabaseError::ConnectBootstrap)?;
        let mut guard = SingleHubGuard::acquire(&bootstrap)
            .await
            .map_err(FencedHubDatabaseError::AcquireGuard)?;
        initialize_hub_fence(&bootstrap)
            .await
            .map_err(FencedHubDatabaseError::InitializeFence)?;
        let generation = advance_hub_fence(guard.connection_mut())
            .await
            .map_err(FencedHubDatabaseError::AdvanceFence)?;
        bootstrap.close().await;
        let pool = connect_fenced_pool(options, generation)
            .await
            .map_err(FencedHubDatabaseError::ConnectFencedPool)?;
        Ok(Self {
            guard: Some(guard),
            pool,
            generation,
        })
    }

    /// Borrows the fenced application pool.
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Returns this incarnation's exact durable generation.
    pub const fn generation(&self) -> HubFenceGeneration {
        self.generation
    }

    /// Proves that this incarnation's exact guarded session remains usable.
    pub async fn check_guard(&mut self) -> Result<(), SingleHubGuardError> {
        let Some(guard) = self.guard.as_mut() else {
            return Err(SingleHubGuardError::GuardLost(None));
        };
        guard.check().await
    }

    /// Globally closes the fenced pool, waits for every outstanding checkout,
    /// and only then releases the singleton guard.
    pub async fn close(mut self) -> Result<(), SingleHubGuardError> {
        self.pool.close().await;
        let Some(guard) = self.guard.take() else {
            return Err(SingleHubGuardError::GuardLost(None));
        };
        guard.close().await
    }
}

impl Drop for FencedHubDatabase {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            // Async pool drain cannot run from Drop. Retaining the guard until
            // process exit fails closed if explicit shutdown is omitted or
            // cancelled while raw PgPool clones or checkouts may still exist.
            mem::forget(guard);
        }
    }
}

/// Sanitized guarded-database startup failure.
#[derive(Debug)]
pub enum FencedHubDatabaseError {
    /// The production URL could not form SQLx connection options.
    ParseOptions(sqlx::Error),
    /// The temporary pre-fence pool could not connect.
    ConnectBootstrap(sqlx::Error),
    /// The database-scoped singleton guard could not be established.
    AcquireGuard(SingleHubGuardError),
    /// Migrations through the fence-establishing boundary failed.
    InitializeFence(MigrateError),
    /// The prior generation could not be fenced and advanced.
    AdvanceFence(HubFenceError),
    /// The new generation's shared-lock pool could not connect.
    ConnectFencedPool(sqlx::Error),
}

impl fmt::Display for FencedHubDatabaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ParseOptions(_) => "database connection options are invalid",
            Self::ConnectBootstrap(_) => "the hub bootstrap database connection failed",
            Self::AcquireGuard(_) => "the database-scoped hub guard failed",
            Self::InitializeFence(_) => "the hub fence migration boundary failed",
            Self::AdvanceFence(_) => "the prior hub generation could not be fenced",
            Self::ConnectFencedPool(_) => "the fenced hub database pool could not connect",
        })
    }
}

impl Error for FencedHubDatabaseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ParseOptions(error)
            | Self::ConnectBootstrap(error)
            | Self::ConnectFencedPool(error) => Some(error),
            Self::AcquireGuard(error) => Some(error),
            Self::InitializeFence(error) => Some(error),
            Self::AdvanceFence(error) => Some(error),
        }
    }
}
