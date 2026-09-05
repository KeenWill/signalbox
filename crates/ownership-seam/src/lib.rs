//! Closed lifecycle-event and session-command boundary for ownership modules.
//!
//! A module consumes only the eight event families projected here and emits
//! only the checked command families admitted by [`SessionCommand`]. Database
//! handles and the wider core outbox vocabulary do not cross this boundary.

use std::future::Future;

pub use signalbox_application::{
    RepoWatchEventContentIdentityV1, RepoWatchEventIdentityFrontierEntryV1,
    RepoWatchEventIdentityFrontierError, RepoWatchEventIdentityFrontierV1,
    RepoWatchEventOccurrenceV1, derive_repo_watch_events,
};
pub use signalbox_domain::{
    BranchName, CheckConclusion, CommitSha, CreateSession, DescendantTerminationScope,
    DurableCommandId, FinishCondition, GoalUserAction, GoalUserCommand, PullRequestBody,
    PullRequestNumber, PullRequestTitle, RepoWatchAuthorLogin, RepoWatchDispatchId, RepoWatchEvent,
    RepoWatchEventId, RepoWatchEventKindNameV1, RepoWatchEventKindV1, RepoWatchEventTarget,
    RepoWatchLabelMatcher, RepoWatchMatcherV1, RepoWatchMatcherV1Input, RepoWatchRule,
    RepoWatchRuleActionV1, RepoWatchRuleContentDigest, RepoWatchRuleId, RepoWatchRuleVersion,
    RepoWatchSingletonScope, RepositorySlug, SessionId, SessionLifecycleCommand,
    SessionLifecycleOperation, SessionOwnership, SessionTemplateName, StartGate, StopStickiness,
    SubmitInput, WorkflowName,
};
pub use signalbox_persistence::mapping::GoalEventDiscriminator as GoalEventKind;
pub use signalbox_persistence::outbox::{
    DispatchedCommandSettlement as CommandSettlement, DispatchedGoalChange as GoalChange,
    DispatchedInjectionOutcome as InjectionOutcome, DispatchedOwnershipChange as OwnershipChange,
    DispatchedSessionCreation as SessionCreated,
    DispatchedSessionStateChange as SessionStateChanged,
    DispatchedSessionStateKind as SessionStateKind, DispatchedSessionTerminal as SessionTerminal,
    DispatchedTurnTerminalDisposition as TurnTerminalDisposition, OutboxDispatchError,
};
use signalbox_persistence::outbox::{
    DispatchedOutboxEvent, DispatchedOutboxEventKind, OutboxConsumer, OutboxConsumerReader,
};
use sqlx::PgPool;
pub use sqlx::types::time::OffsetDateTime;

/// One of the eight lifecycle event families visible to ownership modules.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LifecycleEventKind {
    /// A session and its initial lifecycle facts committed.
    SessionCreated(SessionCreated),
    /// A session moved between non-terminal lifecycle states.
    SessionStateChanged(SessionStateChanged),
    /// A session committed its terminal outcome.
    SessionTerminal(SessionTerminal),
    /// A turn committed one terminal disposition.
    TurnTerminal {
        /// The terminal turn.
        turn: signalbox_domain::TurnId,
        /// Its exact terminal disposition.
        disposition: TurnTerminalDisposition,
    },
    /// A goal lineage appended one event.
    GoalChanged(GoalChange),
    /// A durable command settled.
    CommandSettled {
        /// The settled command.
        command: DurableCommandId,
        /// Its terminal result.
        result: CommandSettlement,
    },
    /// An accepted injection settled.
    InjectionSettled {
        /// The injecting command.
        command: DurableCommandId,
        /// Its terminal delivery outcome.
        outcome: InjectionOutcome,
    },
    /// The session's monitored ownership bit changed.
    SessionOwnershipChanged(OwnershipChange),
}

/// One replayable lifecycle event from the repository-watch cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleEvent {
    sequence: u64,
    recorded_at: OffsetDateTime,
    session: Option<SessionId>,
    kind: LifecycleEventKind,
}

impl LifecycleEvent {
    /// Returns the global outbox sequence acknowledged after this event.
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the database write time of the committed event header.
    pub const fn recorded_at(&self) -> OffsetDateTime {
        self.recorded_at
    }

    /// Returns the event's session, absent only for a rejected creation receipt.
    pub const fn session(&self) -> Option<SessionId> {
        self.session
    }

    /// Borrows the closed typed payload.
    pub const fn kind(&self) -> &LifecycleEventKind {
        &self.kind
    }
}

/// Repository-watch's typed view of the core outbox.
#[derive(Clone, Debug)]
pub struct LifecycleEventSource {
    reader: OutboxConsumerReader,
}

impl LifecycleEventSource {
    /// Binds the source to repository-watch's durable consumer cursor.
    pub const fn new(core_pool: PgPool) -> Self {
        Self {
            reader: OutboxConsumerReader::new(core_pool, OutboxConsumer::RepoWatch),
        }
    }

    /// Reads the next module-visible event without advancing its cursor.
    ///
    /// Core-only event families are acknowledged internally and skipped. A
    /// returned event remains replayable until [`Self::acknowledge`] succeeds.
    pub async fn next(&self) -> Result<Option<LifecycleEvent>, OutboxDispatchError> {
        loop {
            let Some(event) = self.reader.read_next().await? else {
                return Ok(None);
            };
            let sequence = event.sequence();
            if let Some(event) = project_lifecycle_event(event) {
                return Ok(Some(event));
            }
            self.reader.acknowledge(sequence).await?;
        }
    }

    /// Advances through the exact next event after its module effects commit.
    pub async fn acknowledge(&self, event: &LifecycleEvent) -> Result<(), OutboxDispatchError> {
        self.reader.acknowledge(event.sequence).await
    }
}

fn project_lifecycle_event(event: DispatchedOutboxEvent) -> Option<LifecycleEvent> {
    let kind = match event.kind().clone() {
        DispatchedOutboxEventKind::SessionCreated(value) => {
            LifecycleEventKind::SessionCreated(value)
        }
        DispatchedOutboxEventKind::SessionStateChanged(value) => {
            LifecycleEventKind::SessionStateChanged(value)
        }
        DispatchedOutboxEventKind::SessionTerminal(value) => {
            LifecycleEventKind::SessionTerminal(value)
        }
        DispatchedOutboxEventKind::TurnTerminal { turn, disposition } => {
            LifecycleEventKind::TurnTerminal { turn, disposition }
        }
        DispatchedOutboxEventKind::GoalChanged(value) => LifecycleEventKind::GoalChanged(value),
        DispatchedOutboxEventKind::CommandSettled { command, result } => {
            LifecycleEventKind::CommandSettled { command, result }
        }
        DispatchedOutboxEventKind::InjectionSettled { command, outcome } => {
            LifecycleEventKind::InjectionSettled { command, outcome }
        }
        DispatchedOutboxEventKind::SessionOwnershipChanged(value) => {
            LifecycleEventKind::SessionOwnershipChanged(value)
        }
        DispatchedOutboxEventKind::SessionModelSettingsChanged(_)
        | DispatchedOutboxEventKind::TurnModelSettingsResolved(_)
        | DispatchedOutboxEventKind::InputAccepted { .. }
        | DispatchedOutboxEventKind::TurnActivated { .. }
        | DispatchedOutboxEventKind::ModelCallTransition { .. }
        | DispatchedOutboxEventKind::ToolBatchTransition { .. }
        | DispatchedOutboxEventKind::ToolApprovalDecided { .. }
        | DispatchedOutboxEventKind::ContextCompacted { .. }
        | DispatchedOutboxEventKind::RunnerStateTransition { .. }
        | DispatchedOutboxEventKind::DelegationUpdate(_)
        | DispatchedOutboxEventKind::DelegationWake(_) => return None,
    };
    Some(LifecycleEvent {
        sequence: event.sequence(),
        recorded_at: event.recorded_at(),
        session: event.session(),
        kind,
    })
}

/// The checked existing core command carried by one seam submission.
#[derive(Clone, Debug)]
pub enum SessionCommandPayload {
    /// Create a session from an already resolved module dispatch.
    CreateSession(CreateSession),
    /// Submit content using the ordinary input command.
    SubmitInput(SubmitInput),
    /// Attach a goal or resume it with guidance.
    Goal(GoalUserCommand),
    /// Release a start, stop a session, adopt it, or release its ownership.
    Lifecycle(SessionLifecycleCommand),
}

/// A closed session command admitted from an ownership module.
#[derive(Clone, Debug)]
pub struct SessionCommand {
    payload: SessionCommandPayload,
}

/// Stable discriminator stored in a module's dispatch ledger.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionCommandKind {
    /// Create one session.
    CreateSession,
    /// Submit one input.
    SubmitInput,
    /// Mutate one goal lineage.
    Goal,
    /// Mutate session lifecycle or ownership.
    Lifecycle,
}

impl SessionCommand {
    /// Admits an existing typed create-session command.
    pub const fn create_session(command: CreateSession) -> Self {
        Self {
            payload: SessionCommandPayload::CreateSession(command),
        }
    }

    /// Admits the ordinary typed input-submission command.
    pub const fn submit_input(command: SubmitInput) -> Self {
        Self {
            payload: SessionCommandPayload::SubmitInput(command),
        }
    }

    /// Admits only the goal operations named by the seam.
    pub fn goal(command: GoalUserCommand) -> Result<Self, CommandOutsideSeam> {
        match command.action() {
            GoalUserAction::Attach(_) | GoalUserAction::Resume(_) => Ok(Self {
                payload: SessionCommandPayload::Goal(command),
            }),
            GoalUserAction::Stop { .. } | GoalUserAction::Supersede(_) => Err(CommandOutsideSeam),
        }
    }

    /// Admits only the lifecycle operations named by the seam.
    pub fn lifecycle(command: SessionLifecycleCommand) -> Result<Self, CommandOutsideSeam> {
        match command.operation() {
            SessionLifecycleOperation::ReleaseStart
            | SessionLifecycleOperation::Stop { .. }
            | SessionLifecycleOperation::Adopt { .. }
            | SessionLifecycleOperation::Release => Ok(Self {
                payload: SessionCommandPayload::Lifecycle(command),
            }),
            SessionLifecycleOperation::Supersede { .. }
            | SessionLifecycleOperation::Abandon
            | SessionLifecycleOperation::CloseFailed { .. }
            | SessionLifecycleOperation::Resume => Err(CommandOutsideSeam),
        }
    }

    /// Consumes the wrapper and returns the existing typed core command.
    pub fn into_payload(self) -> SessionCommandPayload {
        self.payload
    }

    /// Returns the durable command identity claimed by the checked payload.
    pub const fn command_id(&self) -> DurableCommandId {
        match &self.payload {
            SessionCommandPayload::CreateSession(command) => command.command_id(),
            SessionCommandPayload::SubmitInput(command) => command.command_id(),
            SessionCommandPayload::Goal(command) => command.command_id(),
            SessionCommandPayload::Lifecycle(command) => command.command_id(),
        }
    }

    /// Returns the ledger discriminator for the checked payload family.
    pub const fn kind(&self) -> SessionCommandKind {
        match self.payload {
            SessionCommandPayload::CreateSession(_) => SessionCommandKind::CreateSession,
            SessionCommandPayload::SubmitInput(_) => SessionCommandKind::SubmitInput,
            SessionCommandPayload::Goal(_) => SessionCommandKind::Goal,
            SessionCommandPayload::Lifecycle(_) => SessionCommandKind::Lifecycle,
        }
    }
}

/// A typed core command that the ownership seam does not expose.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandOutsideSeam;

impl std::fmt::Display for CommandOutsideSeam {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("session command is outside the ownership seam")
    }
}

impl std::error::Error for CommandOutsideSeam {}

/// Existing command handling supplied by daemon core to a compiled-in module.
pub trait SessionCommandSink {
    /// Infrastructure failure returned by core command admission.
    type Error;

    /// Submits one checked command under the module's authenticated principal.
    fn submit(
        &self,
        command: SessionCommand,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

#[cfg(test)]
mod tests {
    use super::{SessionCommand, SessionLifecycleCommand, SessionLifecycleOperation};
    use signalbox_domain::{
        DescendantTerminationScope, DurableCommandId, SessionId, StopStickiness,
    };
    use uuid::Uuid;

    fn lifecycle(operation: SessionLifecycleOperation) -> SessionLifecycleCommand {
        SessionLifecycleCommand::new(
            DurableCommandId::from_uuid(Uuid::from_u128(1)),
            SessionId::from_uuid(Uuid::from_u128(2)),
            operation,
        )
    }

    #[test]
    fn closed_lifecycle_commands_admit_sticky_stop_but_not_supersede() {
        assert!(
            SessionCommand::lifecycle(lifecycle(SessionLifecycleOperation::Stop {
                sticky: StopStickiness::Sticky,
                descendant_scope: DescendantTerminationScope::ParentAlone,
            }))
            .is_ok()
        );
        assert!(
            SessionCommand::lifecycle(lifecycle(SessionLifecycleOperation::Supersede {
                successor: SessionId::from_uuid(Uuid::from_u128(3)),
            }))
            .is_err()
        );
    }
}
