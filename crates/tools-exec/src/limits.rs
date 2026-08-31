//! Shared bounds for restricted-process execution.

/// Maximum byte length of one explicitly delivered restricted environment value.
pub(crate) const MAX_SANDBOX_ENVIRONMENT_VALUE_BYTES: usize = 64 * 1024;

/// Private namespace file used to deliver one restricted environment value.
pub const SANDBOX_ENVIRONMENT_DELIVERY_PATH: &str = "/run/signalbox/restricted-environment";
