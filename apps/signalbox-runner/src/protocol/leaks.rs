//! Startup report acknowledgement precedes new or resumed execution.

use super::*;
use signalbox_runner_wire::{LeakFact, LeakPage, WorkspaceLeakPage};
use std::collections::VecDeque;

#[derive(Default)]
pub(super) enum StartupReport {
    #[default]
    Disabled,
    Pending,
    Scanning(
        tokio::task::JoinHandle<Result<Vec<LeakFact>, crate::workspace::RunnerWorkspaceError>>,
    ),
    Reporting(VecDeque<LeakPage>),
    Complete,
}

impl StartupReport {
    pub(super) fn complete(&self) -> bool {
        matches!(self, Self::Disabled | Self::Complete)
    }
}

pub(super) async fn scan_finished(
    report: &mut StartupReport,
) -> Result<Vec<LeakFact>, RunnerConnectionError> {
    let StartupReport::Scanning(task) = report else {
        return std::future::pending().await;
    };
    task.await
        .map_err(|_| RunnerConnectionError::Workspace(crate::WorkspaceProvisionError::Storage))?
        .map_err(|error| RunnerConnectionError::Workspace(error.into()))
}

impl<S: AsyncRead + AsyncWrite + Unpin> RunnerConnection<S> {
    pub(super) async fn advance_local(
        &mut self,
        state: &mut RunnerStateRoot,
    ) -> Result<(), RunnerConnectionError> {
        if self.pending_offer.is_none()
            && state.reconnect_inventory().lease.is_none()
            && !matches!(
                self.startup_report,
                StartupReport::Scanning(_) | StartupReport::Reporting(_)
            )
            && let Some(release) = self.deferred_release.take()
        {
            state.record_release(release.correlation)?;
        }
        self.ensure_workspace(state)?;
        if matches!(self.startup_report, StartupReport::Pending)
            && state.reconnect_inventory().workspace_operation.is_none()
            && state.retained_leak_page().is_none()
        {
            let store = state.workspace_store()?;
            let runner = self.receipt.runner_id();
            self.startup_report =
                StartupReport::Scanning(tokio::spawn(store.scan_startup_leaks(runner)));
        }
        if let StartupReport::Reporting(pages) = &mut self.startup_report
            && state.retained_leak_page().is_none()
        {
            if let Some(page) = pages.pop_front() {
                state.record_leak_page(page)?;
            } else {
                self.startup_report = StartupReport::Complete;
            }
        }
        if !self.leak_sent && state.retained_leak_page().is_some() {
            self.send_leak_page(state).await?;
            self.leak_sent = true;
        }
        if self.startup_report.complete()
            && state.reconnect_inventory().workspace_operation.is_none()
        {
            if let Some(provision) = self.deferred_provision.take() {
                self.serve_message(state, Message::WorkspaceProvision(provision))
                    .await?;
            } else if let Some(dispatch) = self.deferred_dispatch.take() {
                self.serve_message(state, Message::Dispatch(dispatch))
                    .await?;
            } else if !self.offer_claimed
                && state.reconnect_inventory().lease.is_none()
                && let Some(offer) = &self.pending_offer
            {
                send_message(
                    &mut self.io,
                    Message::LeaseClaim(LeaseClaim {
                        correlation: offer.correlation.clone(),
                    }),
                )
                .await?;
                self.offer_claimed = true;
            }
        }
        Ok(())
    }

    pub(super) async fn send_leak_page(
        &mut self,
        state: &RunnerStateRoot,
    ) -> Result<(), RunnerConnectionError> {
        if let Some(page) = state.retained_leak_page() {
            send_message(
                &mut self.io,
                Message::WorkspaceLeakPage(WorkspaceLeakPage { page: page.clone() }),
            )
            .await?;
        }
        Ok(())
    }

    pub(super) fn record_leak_page(
        &mut self,
        state: &mut RunnerStateRoot,
        recorded: signalbox_runner_wire::WorkspaceLeakRecorded,
    ) -> Result<(), RunnerConnectionError> {
        if self.last_leak_recorded.as_ref() == Some(&recorded) {
            return Ok(());
        }
        if state.retained_leak_page().is_none_or(|page| {
            page.correlation != recorded.correlation || page.page_digest != recorded.page_digest
        }) {
            return Err(RunnerConnectionError::Violation(
                ProtocolViolation::ResumeDirectives,
            ));
        }
        state.acknowledge_leak_page(&recorded.correlation)?;
        self.leak_sent = false;
        self.last_leak_recorded = Some(recorded);
        Ok(())
    }
}
