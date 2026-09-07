use super::reader::ProcessTranscriptReader;
use super::{
    ProcessReadCorruption, ProcessReadError, SESSION_SUMMARY_PAGE_SIZE,
    decode_pending_session_summary, load_process_runner_projection,
    map_session_placement_read_error, required,
};
use crate::mapping::{model_settings_from_json, session_id_from_uuid, session_id_to_uuid};
use signalbox_domain::{
    CredentialProfileName, DirectModelSelection, ModelAlias, RunnerGeneration, RunnerId,
    RunnerSandboxProfile, RunnerSelector, RunnerWorkingDirectory, SessionId,
    SessionReadScopeRefusal, VersionedSessionPlacement, WorkspaceRepositoryKey,
};
use sqlx::postgres::PgRow;
use sqlx::types::Uuid;
use sqlx::{Postgres, Row, Transaction};
use std::collections::VecDeque;

/// One model-selection request in the process-facing session summary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessModelSelection {
    /// A stable direct-selection identity.
    Direct(DirectModelSelection),
    /// A stable alias identity.
    Alias(ModelAlias),
}

/// Closed current state in one process-facing runner projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessRunnerProjectionState {
    /// No runner has been pinned yet.
    Unpinned,
    /// The current placement is pinned.
    Pinned,
    /// The exact selected runner was lost before pinning.
    RunnerLostBeforePin,
    /// The pinned runner was lost.
    RunnerLost,
    /// The lost placement was explicitly abandoned.
    RunnerAbandoned,
}

/// Closed current connection health in one process-facing runner projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessRunnerConnectionHealth {
    /// The runner connection is currently healthy.
    Connected,
    /// The connection is inside its missed-heartbeat recovery window.
    Suspect,
    /// The connection closed through orderly shutdown.
    Shutdown,
    /// The connection reached terminal loss.
    Lost,
}

#[derive(signalbox_derive::Accessors)]
/// Complete current runner placement from one repeatable-read snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessRunnerProjection {
    /// Borrows the immutable requested selector.
    #[get]
    pub(super) selector: RunnerSelector,
    pub(super) runner: Option<RunnerId>,
    pub(super) placement_revision: RunnerGeneration,
    pub(super) sandbox: RunnerSandboxProfile,
    pub(super) credential_profile: Option<CredentialProfileName>,
    pub(super) repository: Option<WorkspaceRepositoryKey>,
    pub(super) working_directory: Option<RunnerWorkingDirectory>,
    pub(super) connection_health: Option<ProcessRunnerConnectionHealth>,
    pub(super) state: ProcessRunnerProjectionState,
}

impl ProcessRunnerProjection {
    /// Returns the current or lost exact runner when the state names one.
    pub const fn runner(&self) -> Option<RunnerId> {
        self.runner
    }

    /// Returns the positive current placement revision.
    pub const fn placement_revision(&self) -> RunnerGeneration {
        self.placement_revision
    }

    /// Returns the explicitly selected sandbox profile.
    pub const fn sandbox(&self) -> RunnerSandboxProfile {
        self.sandbox
    }

    /// Borrows the independently nullable requested credential profile.
    pub const fn credential_profile(&self) -> Option<&CredentialProfileName> {
        self.credential_profile.as_ref()
    }

    /// Borrows the independently nullable requested repository key.
    pub const fn repository(&self) -> Option<&WorkspaceRepositoryKey> {
        self.repository.as_ref()
    }

    /// Borrows the independently nullable exact requested directory.
    pub const fn working_directory(&self) -> Option<&RunnerWorkingDirectory> {
        self.working_directory.as_ref()
    }

    /// Returns current connection health exactly while the placement is pinned.
    pub const fn connection_health(&self) -> Option<ProcessRunnerConnectionHealth> {
        self.connection_health
    }

    /// Returns the exact current placement state.
    pub const fn state(&self) -> ProcessRunnerProjectionState {
        self.state
    }
}

#[derive(signalbox_derive::Accessors)]
/// One current session summary read from a shared transaction snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSessionSummary {
    session: SessionId,
    defaults_version: u64,
    model_selection: ProcessModelSelection,
    /// Borrows the current immutable placement epoch.
    #[get]
    placement: signalbox_domain::VersionedSessionPlacement,
    runner: Option<ProcessRunnerProjection>,
}

impl ProcessSessionSummary {
    /// Returns the summarized session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the current positive defaults version.
    pub const fn defaults_version(&self) -> u64 {
        self.defaults_version
    }

    /// Returns the current model-selection request.
    pub const fn model_selection(&self) -> ProcessModelSelection {
        self.model_selection
    }

    /// Borrows the complete current runner projection when runner placement was requested.
    pub const fn runner(&self) -> Option<&ProcessRunnerProjection> {
        self.runner.as_ref()
    }
}

#[derive(signalbox_derive::Accessors)]
/// One complete immutable session-defaults epoch read for the process
/// boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSessionDefaults {
    pub(super) session: SessionId,
    pub(super) version: signalbox_domain::SessionConfigurationDefaultsVersion,
    /// Borrows the complete defaults value on that epoch.
    #[get]
    pub(super) defaults: signalbox_domain::SessionConfigurationDefaults,
}

impl ProcessSessionDefaults {
    /// Returns the selected session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the read immutable epoch's version.
    pub const fn version(&self) -> signalbox_domain::SessionConfigurationDefaultsVersion {
        self.version
    }
}

/// Typed outcome of one session-defaults epoch read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessSessionDefaultsRead {
    /// The selected epoch with its complete defaults value.
    Read(ProcessSessionDefaults),
    /// The selected session does not exist in the read snapshot.
    SessionNotFound,
    /// The session exists but the named epoch was never installed.
    VersionNotFound,
}

/// Typed outcome of the path-scoped native transcript-open boundary.
#[derive(Debug)]
pub enum ProcessScopedTranscriptRead {
    /// The target exists and its transcript cursor is open in the checked snapshot.
    Opened(Box<ProcessTranscriptReader>),
    /// The selected target session does not exist in the checked snapshot.
    TargetNotFound,
    /// The requesting placement's parent directory does not contain the target.
    Refused(SessionReadScopeRefusal),
}

pub(super) fn decode_session_defaults_value(
    row: &PgRow,
) -> Result<signalbox_domain::SessionConfigurationDefaults, ProcessReadError> {
    let kind: String = row
        .try_get::<Option<String>, _>("model_selection_kind")?
        .ok_or(ProcessReadCorruption::Missing("model_selection_kind"))?;
    let direct: Option<Uuid> = row.try_get("direct_model_selection_id")?;
    let alias: Option<Uuid> = row.try_get("model_alias_id")?;
    let model = match (kind.as_str(), direct, alias) {
        ("direct", Some(value), None) => {
            signalbox_domain::ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(value))
        }
        ("alias", None, Some(value)) => {
            signalbox_domain::ModelSelectionRequest::Alias(ModelAlias::from_uuid(value))
        }
        ("direct" | "alias", _, _) => {
            return Err(ProcessReadCorruption::Inconsistent("model selection").into());
        }
        _ => {
            return Err(ProcessReadCorruption::Unsupported {
                field: "model_selection_kind",
                value: kind,
            }
            .into());
        }
    };
    let tool_approval: String = row
        .try_get::<Option<String>, _>("dangerous_tool_auto_approval")?
        .ok_or(ProcessReadCorruption::Missing(
            "dangerous_tool_auto_approval",
        ))?;
    let dangerous_tool_auto_approval = crate::mapping::dangerous_tool_auto_approval_from_str(
        &tool_approval,
    )
    .ok_or(ProcessReadCorruption::Unsupported {
        field: "dangerous_tool_auto_approval",
        value: tool_approval,
    })?;
    let system_prompt = row
        .try_get::<Option<String>, _>("system_prompt")?
        .map(|value| {
            signalbox_domain::SessionSystemPrompt::try_new(value)
                .map_err(|_| ProcessReadCorruption::Inconsistent("system prompt admission"))
        })
        .transpose()?;
    let model_settings = row
        .try_get::<Option<serde_json::Value>, _>("model_settings")?
        .ok_or(ProcessReadCorruption::Missing("model_settings"))?;
    let model_settings = model_settings_from_json(model_settings)
        .map_err(|_| ProcessReadCorruption::Inconsistent("model settings"))?;
    signalbox_domain::SessionConfigurationDefaults::complete_with_model_settings(
        model,
        dangerous_tool_auto_approval,
        system_prompt,
        model_settings,
    )
    .ok_or_else(|| {
        ProcessReadCorruption::Inconsistent("model settings validation selection").into()
    })
}

/// One repeatable-read session-summary cursor with bounded read-ahead.
///
/// Call [`Self::next_summary`] until it returns `None`. That terminal call
/// commits the read-only transaction and makes [`Self::summary_count`]
/// available. Each page batches placement authentication for up to 64 sessions;
/// dropping a reader early rolls its transaction back.
#[derive(Debug)]
pub struct ProcessSessionSummaryReader {
    pub(super) transaction: Option<Transaction<'static, Postgres>>,
    pub(super) next_session_after: Option<Uuid>,
    pub(super) pending: VecDeque<PendingSessionSummary>,
    pub(super) summary_count: u64,
    pub(super) committed_summary_count: Option<u64>,
}

#[derive(Debug)]
pub(super) struct PendingSessionSummary {
    pub(super) session: SessionId,
    pub(super) defaults_version: u64,
    pub(super) model_selection: ProcessModelSelection,
    pub(super) placement: VersionedSessionPlacement,
}

impl PendingSessionSummary {
    fn with_runner(self, runner: Option<ProcessRunnerProjection>) -> ProcessSessionSummary {
        ProcessSessionSummary {
            session: self.session,
            defaults_version: self.defaults_version,
            model_selection: self.model_selection,
            placement: self.placement,
            runner,
        }
    }
}

impl ProcessSessionSummaryReader {
    /// Returns the committed count only after [`Self::next_summary`] returned
    /// `None`.
    pub const fn summary_count(&self) -> Option<u64> {
        self.committed_summary_count
    }

    /// Yields one summary in session-identity order without retaining prior
    /// decoded rows.
    pub async fn next_summary(
        &mut self,
    ) -> Result<Option<ProcessSessionSummary>, ProcessReadError> {
        if self.committed_summary_count.is_some() {
            return Ok(None);
        }

        if self.pending.is_empty() {
            let next_session_after = self.next_session_after;
            let (pending, next_session_after) =
                load_session_summary_page(self.transaction_mut()?, next_session_after).await?;
            self.pending = pending;
            self.next_session_after = next_session_after;
        }

        if let Some(pending) = self.pending.front() {
            let session = pending.session;
            let runner = load_process_runner_projection(self.transaction_mut()?, session).await?;
            let summary = self
                .pending
                .pop_front()
                .ok_or(ProcessReadCorruption::Missing("pending session summary"))?
                .with_runner(runner);
            self.summary_count =
                self.summary_count
                    .checked_add(1)
                    .ok_or(ProcessReadCorruption::InvalidOrdinal(
                        "session summary count",
                    ))?;
            return Ok(Some(summary));
        }

        let transaction = self
            .transaction
            .take()
            .ok_or(ProcessReadCorruption::Missing("process read transaction"))?;
        transaction.commit().await?;
        self.committed_summary_count = Some(self.summary_count);
        Ok(None)
    }

    fn transaction_mut(&mut self) -> Result<&mut Transaction<'static, Postgres>, ProcessReadError> {
        self.transaction
            .as_mut()
            .ok_or_else(|| ProcessReadCorruption::Missing("process read transaction").into())
    }
}

async fn load_session_summary_page(
    transaction: &mut Transaction<'static, Postgres>,
    next_session_after: Option<Uuid>,
) -> Result<(VecDeque<PendingSessionSummary>, Option<Uuid>), ProcessReadError> {
    let rows = sqlx::query(
        "SELECT
            session_row.session_id,
            current_defaults.current_version AS defaults_version,
            selected_defaults.model_selection_kind,
            selected_defaults.direct_model_selection_id,
            selected_defaults.model_alias_id
           FROM session AS session_row
           LEFT JOIN session_current_defaults AS current_defaults
             ON current_defaults.session_id = session_row.session_id
           LEFT JOIN session_defaults_version AS selected_defaults
             ON selected_defaults.session_id = current_defaults.session_id
            AND selected_defaults.version = current_defaults.current_version
          WHERE ($1::uuid IS NULL OR session_row.session_id > $1)
          ORDER BY session_row.session_id
          LIMIT $2",
    )
    .bind(next_session_after)
    .bind(SESSION_SUMMARY_PAGE_SIZE)
    .fetch_all(&mut **transaction)
    .await?;
    if rows.is_empty() {
        return Ok((VecDeque::new(), next_session_after));
    }

    let sessions = rows
        .iter()
        .map(|row| required::<Uuid>(row, "session_id").map(session_id_from_uuid))
        .collect::<Result<Vec<_>, _>>()?;
    let mut placements = crate::session_placement::load_current_batch(transaction, &sessions)
        .await
        .map_err(map_session_placement_read_error)?;
    let mut pending = VecDeque::with_capacity(rows.len());
    for row in rows {
        let session_uuid = required(&row, "session_id")?;
        let placement = placements
            .remove(&session_uuid)
            .ok_or(ProcessReadCorruption::Missing("session placement"))?;
        pending.push_back(decode_pending_session_summary(&row, placement)?);
    }
    if !placements.is_empty() {
        return Err(ProcessReadCorruption::Inconsistent("session placement batch").into());
    }
    let next_session_after = pending
        .back()
        .map(|summary| session_id_to_uuid(summary.session));
    Ok((pending, next_session_after))
}
