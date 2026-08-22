//! Shared bounds for restricted-process execution.

/// Maximum byte length of one explicitly delivered restricted environment value.
pub(crate) const MAX_SANDBOX_ENVIRONMENT_VALUE_BYTES: usize = 64 * 1024;
