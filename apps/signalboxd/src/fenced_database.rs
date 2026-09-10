//! Single-hub acquisition, durable generation advance, and fenced pool startup.

use std::{error::Error, fmt, mem};

use signalbox_persistence::{
    hub_fence::{
        HubFenceError, HubFenceGeneration, advance_hub_fence, initialize_hub_fence,
        retire_hub_fence_generation,
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
    recovery: Option<crate::guard_recovery::GuardRecoveryObserver>,
}

impl FencedHubDatabase {
    /// Observes guard loss throughout production fence and pool construction.
    pub async fn connect_production(
        database_url: &str,
        min_connections: Option<u32>,
        observer: crate::guard_recovery::GuardRecoveryObserver,
    ) -> Result<Self, FencedHubDatabaseError> {
        let options = production_connection_options(database_url)
            .map_err(FencedHubDatabaseError::ParseOptions)?;
        Self::connect_with_observer(options, min_connections, Some(observer)).await
    }

    /// Establishes one guarded incarnation using already parsed connection
    /// options. This is also the local integration-test construction boundary.
    pub async fn connect_with(
        options: PgConnectOptions,
        min_connections: Option<u32>,
    ) -> Result<Self, FencedHubDatabaseError> {
        Self::connect_with_observer(options, min_connections, None).await
    }

    /// Observes guard loss from acquisition through fence and pool construction.
    pub async fn connect_with_observer(
        options: PgConnectOptions,
        min_connections: Option<u32>,
        recovery: Option<crate::guard_recovery::GuardRecoveryObserver>,
    ) -> Result<Self, FencedHubDatabaseError> {
        let bootstrap = PgPoolOptions::new()
            // Fence initialization and exact-session observation run concurrently.
            .max_connections(2)
            .connect_with(options.clone())
            .await
            .map_err(FencedHubDatabaseError::ConnectBootstrap)?;
        let mut guard = SingleHubGuard::acquire(&bootstrap)
            .await
            .map_err(FencedHubDatabaseError::AcquireGuard)?;
        let monitor = guard.monitor_construction(bootstrap.clone());
        let construction = async {
            initialize_hub_fence(&bootstrap)
                .await
                .map_err(FencedHubDatabaseError::InitializeFence)?;
            let mut advanced_fence = advance_hub_fence(guard.connection_mut())
                .await
                .map_err(FencedHubDatabaseError::AdvanceFence)?;
            let pool = advanced_fence
                .connect_pool(options, min_connections)
                .await
                .map_err(FencedHubDatabaseError::ConnectFencedPool)?;
            Ok((pool, advanced_fence.generation()))
        };
        let constructed = tokio::select! {
            biased;
            error = monitor => {
                if let Some(observer) = &recovery {
                    observer.guard_lost();
                }
                Err(FencedHubDatabaseError::GuardLost(error))
            },
            result = construction => result,
        };
        // A construction error can win the race with the observation query.
        let checked = guard.check().await;
        if matches!(constructed, Err(FencedHubDatabaseError::GuardLost(_))) || checked.is_err() {
            if let Some(observer) = &recovery {
                observer.guard_lost();
            }
            if let Ok((pool, _)) = constructed {
                pool.close().await;
            }
            bootstrap.close().await;
            return Err(FencedHubDatabaseError::GuardLost(
                checked
                    .err()
                    .unwrap_or(SingleHubGuardError::GuardLost(None)),
            ));
        }
        bootstrap.close().await;
        let (pool, generation) = constructed?;
        Ok(Self {
            guard: Some(guard),
            pool,
            generation,
            recovery,
        })
    }

    /// Reports guard loss before pool shutdown starts consuming the recovery bound.
    pub fn with_recovery_observer(
        mut self,
        observer: crate::guard_recovery::GuardRecoveryObserver,
    ) -> Self {
        self.recovery = Some(observer);
        self
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
        let result = match self.guard.as_mut() {
            Some(guard) => guard.check().await,
            None => Err(SingleHubGuardError::GuardLost(None)),
        };
        if result.is_err()
            && let Some(observer) = &self.recovery
        {
            observer.guard_lost();
        }
        result
    }

    /// Globally closes the fenced pool, waits for every outstanding checkout,
    /// and only then releases the singleton guard.
    pub async fn close(mut self) -> Result<(), SingleHubGuardError> {
        let pool = self.pool.clone();
        let drain = pool.close();
        tokio::pin!(drain);
        let guard_loss = {
            let monitor = async {
                loop {
                    if let Err(error) = self.check_guard().await {
                        return error;
                    }
                    tokio::time::sleep(crate::GUARD_CHECK_INTERVAL).await;
                }
            };
            tokio::pin!(monitor);
            tokio::select! {
                biased;
                error = &mut monitor => {
                    drain.await;
                    Some(error)
                }
                () = &mut drain => None,
            }
        };
        let Some(guard) = self.guard.as_mut() else {
            return Err(SingleHubGuardError::GuardLost(None));
        };
        let retirement = retire_hub_fence_generation(guard.connection_mut(), self.generation)
            .await
            .map_err(|error| SingleHubGuardError::GuardLost(Some(error)));
        let guard = self
            .guard
            .take()
            .ok_or(SingleHubGuardError::GuardLost(None))?;
        let closed = guard.close().await;
        match guard_loss {
            Some(error) => Err(error),
            None => retirement.and(closed),
        }
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

/// Result of one non-disruptive fenced-pool floor reconciliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FencedPoolFloorReconciliation {
    /// The physical-session floor was already present.
    Satisfied,
    /// Idle service capacity was preserved instead of being held by maintenance.
    DeferredForIdleCapacity,
    /// Demand had exhausted idle capacity, so one missing session was opened.
    Replenished,
}

/// Reopens one missing physical session without consuming idle service
/// capacity.
///
/// SQLx opens a session only after its idle inventory is empty. Reconciliation
/// therefore defers while any idle session remains; holding that inventory to
/// force construction would steal capacity from ordinary work for the whole
/// connection attempt. Once demand has consumed the idle inventory, one
/// acquisition opens one missing session and immediately returns it. Callers
/// own the attempt deadline and repeat bound.
pub async fn reconcile_fenced_pool_floor(
    pool: &PgPool,
    minimum: u32,
) -> Result<FencedPoolFloorReconciliation, sqlx::Error> {
    let current = pool.size();
    if current >= minimum {
        return Ok(FencedPoolFloorReconciliation::Satisfied);
    }
    if pool.num_idle() > 0 {
        return Ok(FencedPoolFloorReconciliation::DeferredForIdleCapacity);
    }
    let connection = pool.acquire().await?;
    drop(connection);
    Ok(if pool.size() > current {
        FencedPoolFloorReconciliation::Replenished
    } else {
        FencedPoolFloorReconciliation::DeferredForIdleCapacity
    })
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
    /// The acquired singleton guard was lost during database construction.
    GuardLost(SingleHubGuardError),
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
            Self::GuardLost(_) => "the hub guard was lost during database construction",
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
            Self::AcquireGuard(error) | Self::GuardLost(error) => Some(error),
            Self::InitializeFence(error) => Some(error),
            Self::AdvanceFence(error) => Some(error),
        }
    }
}
