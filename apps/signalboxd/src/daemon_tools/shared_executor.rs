use signalbox_application::{
    CorrelatedToolExecutorEvidence, ToolExecutionInvocation, ToolExecutor,
};
use std::{fmt, sync::Arc};
use tokio::sync::Mutex;

pub(super) struct SharedToolExecutor<Executor> {
    inner: Arc<Mutex<Executor>>,
}

impl<Executor> SharedToolExecutor<Executor> {
    pub(super) fn new(executor: Executor) -> Self {
        Self {
            inner: Arc::new(Mutex::new(executor)),
        }
    }

    /// Whether this handle is the only one, so releasing it releases the
    /// serialization domain rather than leaving a second one beside it.
    pub(super) fn is_sole_handle(&self) -> bool {
        Arc::strong_count(&self.inner) == 1
    }
}

impl<Executor> Clone for SharedToolExecutor<Executor> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<Executor> fmt::Debug for SharedToolExecutor<Executor> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedToolExecutor")
            .finish_non_exhaustive()
    }
}

impl<Executor> ToolExecutor for SharedToolExecutor<Executor>
where
    Executor: ToolExecutor + Send,
{
    type Error = Executor::Error;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        self.inner.lock().await.execute(invocation).await
    }
}
