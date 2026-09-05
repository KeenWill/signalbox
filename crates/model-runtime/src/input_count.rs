//! Exact provider-native rendered-input token counting.

use std::future::Future;

use crate::{CancellationSignal, ModelOperation};

/// Provider-adapter outcome for one exact input-count request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputTokenCountOutcome<C> {
    /// The provider counted the exact translated input.
    Counted {
        /// Caller-owned operation correlation.
        correlation: C,
        /// Exact provider-reported rendered-input count.
        input_tokens: u64,
    },
    /// Caller cancellation won before a complete count was available.
    Cancelled {
        /// Caller-owned operation correlation.
        correlation: C,
    },
    /// The selected adapter has no provider-native exact count operation.
    Unavailable {
        /// Caller-owned operation correlation.
        correlation: C,
    },
    /// Translation, credential access, transport, status, or response
    /// validation failed; no estimate is substituted.
    Failed {
        /// Caller-owned operation correlation.
        correlation: C,
    },
}

/// Provider adapter capable of counting the exact native rendering of one
/// prospective operation without issuing a model-generation request.
pub trait ModelInputTokenCounter<C> {
    /// Performs at most one provider-native count interaction.
    fn count_input_tokens(
        &self,
        operation: ModelOperation<C>,
        cancellation: CancellationSignal,
    ) -> impl Future<Output = InputTokenCountOutcome<C>> + Send;
}
