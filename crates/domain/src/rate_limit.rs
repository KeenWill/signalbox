//! Provider capacity evidence retained independently of pool policy.

use std::time::{Duration, SystemTime};

/// One observed accounting window without an inferred capacity or lifetime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderRateLimitWindow {
    /// Remaining percentage exactly as observed.
    remaining_percent: i64,
    /// Reported accounting-window duration.
    window_duration: Option<Duration>,
    /// Reported reset instant.
    resets_at: Option<SystemTime>,
}

impl ProviderRateLimitWindow {
    /// Borrows the reported remaining percentage.
    pub const fn remaining_percent(&self) -> &i64 {
        &self.remaining_percent
    }

    /// Borrows the reported accounting duration.
    pub const fn window_duration(&self) -> &Option<Duration> {
        &self.window_duration
    }

    /// Borrows the reported reset instant.
    pub const fn resets_at(&self) -> &Option<SystemTime> {
        &self.resets_at
    }

    /// Retains the provider's percentage, optional window length, and reset.
    pub fn new(
        remaining_percent: i64,
        window_duration: Option<Duration>,
        resets_at: Option<SystemTime>,
    ) -> Self {
        Self {
            remaining_percent,
            window_duration,
            resets_at,
        }
    }
}

/// The latest collection of windows observed for one credential on a call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderRateLimitSnapshot {
    /// Time the adapter observed these windows.
    observed_at: SystemTime,
    /// Windows in the adapter's reported order.
    windows: Vec<ProviderRateLimitWindow>,
}

impl ProviderRateLimitSnapshot {
    /// Borrows the observation time.
    pub const fn observed_at(&self) -> &SystemTime {
        &self.observed_at
    }

    /// Borrows the windows in reported order.
    pub fn windows(&self) -> &[ProviderRateLimitWindow] {
        &self.windows
    }

    /// Retains observation time and provider windows in their reported order.
    pub fn new(observed_at: SystemTime, windows: Vec<ProviderRateLimitWindow>) -> Self {
        Self {
            observed_at,
            windows,
        }
    }
}
