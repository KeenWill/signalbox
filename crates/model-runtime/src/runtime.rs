//! The one-operation execution trait and cancellation signal.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::evidence::TerminalReport;
use crate::observation::ObservationSink;
use crate::operation::ModelOperation;
use crate::preparation::PreparationOutcome;

/// Prepares and executes exactly one explicitly authorized model operation.
///
/// An implementation performs at most one provider interaction per call,
/// emits observations to the sink in order, and always returns a
/// [`TerminalReport`] — failures are typed evidence, not exceptions, so the
/// caller can classify every outcome under ADR-0043. Implementations never
/// retry, fall back, or issue a second request; uncertainty is reported as
/// boundary-loss evidence, not resolved by repetition (ADR-0005).
///
/// ADR-0045 requires two distinct stages. [`prepare`](Self::prepare) performs
/// all validation, translation, serialization, credential access, and request
/// construction without provider traffic. The caller may durably authorize
/// the interaction only after that stage succeeds. [`execute`](Self::execute)
/// then consumes the opaque capability and performs no second preparation or
/// credential access.
pub trait ModelRuntime<C> {
    /// The adapter-owned, non-cloneable, nonserializable one-shot request
    /// capability produced by preparation and consumed by execution.
    type Prepared: Send;

    /// Prepares a complete request capability without provider traffic.
    ///
    /// The cancellation signal is work-first: a preparation result already
    /// available in the same poll wins over cancellation.
    fn prepare(
        &self,
        operation: ModelOperation<C>,
        cancellation: CancellationSignal,
    ) -> impl Future<Output = PreparationOutcome<C, Self::Prepared>> + Send;

    /// Consumes one prepared capability, emitting observations and returning
    /// terminal evidence.
    ///
    /// The cancellation signal is best-effort: an implementation stops local
    /// work when it fires and reports evidence about how far the request
    /// provably progressed; it never claims provider-side work stopped.
    fn execute(
        &self,
        prepared: Self::Prepared,
        sink: &mut (dyn ObservationSink<C> + Send),
        cancellation: CancellationSignal,
    ) -> impl Future<Output = TerminalReport<C>> + Send;
}

/// A caller-supplied cancellation signal: a future that resolves when the
/// caller wants the operation abandoned.
///
/// Wrapping keeps [`ModelRuntime`]'s signature free of a specific
/// cancellation library; any `Future<Output = ()> + Send` (a token's
/// `cancelled()` future, a channel closure) can back it.
pub struct CancellationSignal(Pin<Box<dyn Future<Output = ()> + Send>>);

impl CancellationSignal {
    /// A signal that never fires.
    pub fn never() -> Self {
        Self(Box::pin(std::future::pending()))
    }

    /// A signal that fires when the given future resolves.
    pub fn when(future: impl Future<Output = ()> + Send + 'static) -> Self {
        Self(Box::pin(future))
    }

    /// A signal that has already fired.
    pub fn already_cancelled() -> Self {
        Self(Box::pin(std::future::ready(())))
    }
}

impl Future for CancellationSignal {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        self.0.as_mut().poll(context)
    }
}

impl std::fmt::Debug for CancellationSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CancellationSignal")
    }
}
