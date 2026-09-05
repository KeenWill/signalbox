//! Provider-native rendered-input token estimation.

use std::future::Future;

use crate::{CancellationSignal, ModelOperation};

/// Provider-adapter outcome for one input-count estimate request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputTokenCountOutcome<C> {
    /// The provider estimated the translated input.
    Counted {
        /// Caller-owned operation correlation.
        correlation: C,
        /// Provider-reported rendered-input estimate.
        input_tokens: u64,
    },
    /// Caller cancellation won before a complete count was available.
    Cancelled {
        /// Caller-owned operation correlation.
        correlation: C,
    },
    /// The selected adapter has no provider-native estimate operation.
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

/// Provider adapter capable of estimating the native rendering of one
/// prospective operation without issuing a model-generation request.
pub trait ModelInputTokenCounter<C> {
    /// Performs at most one provider-native count interaction.
    fn count_input_tokens(
        &self,
        operation: ModelOperation<C>,
        cancellation: CancellationSignal,
    ) -> impl Future<Output = InputTokenCountOutcome<C>> + Send;
}
