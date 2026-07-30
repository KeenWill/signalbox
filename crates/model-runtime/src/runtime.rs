//! The one-operation execution trait and cancellation signal.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use crate::evidence::TerminalReport;
use crate::observation::ObservationSink;
use crate::operation::ModelOperation;
use crate::preparation::PreparationOutcome;

/// Prepares and executes exactly one explicitly authorized model operation.
///
/// An implementation performs at most one provider interaction per call,
/// emits observations to the sink in order, and always returns a
/// [`TerminalReport`] — failures are typed evidence, not exceptions, so the
/// caller can classify every outcome under docs/spec/model-call-execution.md.
/// Implementations never retry, fall back, or issue a second request;
/// uncertainty is reported as boundary-loss evidence, not resolved by
/// repetition (docs/spec/runtime-substrate.md).
///
/// docs/spec/runtime-substrate.md requires two distinct stages.
/// [`prepare`](Self::prepare) performs all validation, translation,
/// serialization, credential access, and request construction without
/// provider traffic. The caller may durably authorize the interaction only
/// after that stage succeeds. [`execute`](Self::execute) then consumes the
/// opaque capability and performs no second preparation or credential
/// access.
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

    /// Checks whether cancellation is already observable without blocking.
    pub fn is_cancelled(&mut self) -> bool {
        let mut context = Context::from_waker(Waker::noop());
        Pin::new(self).poll(&mut context).is_ready()
    }

    /// Runs `work` until it completes or cancellation becomes observable.
    ///
    /// Work is polled first, so already-available provider evidence wins a
    /// same-poll race instead of being discarded as ambiguous cancellation.
    pub async fn run_until_cancelled<F: Future>(&mut self, work: F) -> Option<F::Output> {
        let mut work = std::pin::pin!(work);
        std::future::poll_fn(|context| {
            if let Poll::Ready(output) = work.as_mut().poll(context) {
                return Poll::Ready(Some(output));
            }
            if Pin::new(&mut *self).poll(context).is_ready() {
                Poll::Ready(None)
            } else {
                Poll::Pending
            }
        })
        .await
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

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    use super::CancellationSignal;

    #[test]
    fn cancellation_status_is_checked_without_blocking() {
        assert!(CancellationSignal::already_cancelled().is_cancelled());
        assert!(!CancellationSignal::never().is_cancelled());
    }

    #[test]
    fn ready_work_wins_a_same_poll_cancellation_race() {
        let mut cancellation = CancellationSignal::already_cancelled();
        let mut future = std::pin::pin!(cancellation.run_until_cancelled(std::future::ready(7)));
        let mut context = Context::from_waker(Waker::noop());

        assert_eq!(future.as_mut().poll(&mut context), Poll::Ready(Some(7)));
    }

    #[test]
    fn cancellation_wins_while_work_remains_pending() {
        let mut cancellation = CancellationSignal::already_cancelled();
        let mut future =
            std::pin::pin!(cancellation.run_until_cancelled(std::future::pending::<()>()));
        let mut context = Context::from_waker(Waker::noop());

        assert_eq!(future.as_mut().poll(&mut context), Poll::Ready(None));
    }
}
