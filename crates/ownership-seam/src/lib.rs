//! Closed lifecycle-event and session-command boundary for ownership modules.
//!
//! A module consumes only the eight event families projected here and emits
//! only the checked command families admitted by [`SessionCommand`]. Database
//! handles and the wider core outbox vocabulary do not cross this boundary.

pub use signalbox_application::{
    RepoWatchEventContentIdentityV1, RepoWatchEventIdentityFrontierEntryV1,
    RepoWatchEventIdentityFrontierError, RepoWatchEventIdentityFrontierV1,
    RepoWatchEventOccurrenceV1, derive_repo_watch_events,
};
pub use signalbox_domain::{
    BranchName, CheckConclusion, CheckRunName, ChecksOutcome, CommitSha, ContextFrontierId,
    CreateSession, DeliveryRequest, DescendantTerminationScope, DirectModelSelection,
    DurableCommandId, FinishCondition, FinishConditionStatement, GitHubObjectId, GoalGuidance,
    GoalStatement, GoalUserAction, GoalUserCommand, LabelName, LifecycleActor, MergeableState,
    ModelCallId, ModelSelectionOverride, ModelSelectionRequest, ModuleDispatch,
    PerInputConfigurationChoices, PullRequestBody, PullRequestNumber, PullRequestTitle,
    ReactionChange, ReactionContent, ReactionSubject, RepoWatchAuthorLogin, RepoWatchDispatchId,
    RepoWatchEvent, RepoWatchEventId, RepoWatchEventKindNameV1, RepoWatchEventKindV1,
    RepoWatchEventTarget, RepoWatchLabelMatcher, RepoWatchMatcherV1, RepoWatchMatcherV1Input,
    RepoWatchRule, RepoWatchRuleActionV1, RepoWatchRuleContentDigest, RepoWatchRuleId,
    RepoWatchRuleIdentityField, RepoWatchRuleIdentityFieldDigest, RepoWatchRuleVersion,
    RepoWatchSingletonScope, RepositorySlug, ReviewState, ReviewThreadId,
    SemanticTranscriptEntryId, SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
    SessionCreationCause, SessionCreationProvenance, SessionFailureCause, SessionId,
    SessionLifecycleCommand, SessionLifecycleOperation, SessionLifecycleState, SessionOwnership,
    SessionOwnershipTransition, SessionTemplateName, SessionTerminalOutcome, StartGate,
    StopStickiness, SubmitInput, ToolAttemptId, TurnId, UserContent, UserContentPart, WorkflowName,
};
pub use signalbox_persistence::outbox::OutboxDispatchError;
use signalbox_persistence::outbox::{
    DispatchedCommandSettlement, DispatchedGoalChange, DispatchedInjectionOutcome,
    DispatchedOutboxEvent, DispatchedOutboxEventKind, DispatchedReconciliationOperation,
    DispatchedSessionStateKind, DispatchedTurnTerminalDisposition, OutboxConsumer,
    OutboxConsumerReader,
};
use sqlx::PgPool;
pub use sqlx::types::time::OffsetDateTime;

/// Creation facts visible across the ownership seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCreated {
    pub cause: SessionCreationCause,
    pub ownership: SessionOwnership,
}

/// Bare lifecycle state left by one transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionStateKind {
    Created,
    Dispatched,
    Active,
    Waiting,
    Recovering,
    Blocked,
    Parked,
}

/// One nonterminal lifecycle transition visible across the seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionStateChanged {
    pub prior: SessionStateKind,
    pub state: SessionLifecycleState,
    pub actor: LifecycleActor,
}

/// One terminal session transition visible across the seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTerminal {
    pub prior: SessionStateKind,
    pub outcome: SessionTerminalOutcome,
    pub standing: Option<SessionFailureCause>,
    pub actor: LifecycleActor,
}

/// Ambiguous operation that made a turn require reconciliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconciliationOperation {
    ModelCall(ModelCallId),
    ToolAttempt(ToolAttemptId),
}

/// Turn terminal disposition visible across the seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnTerminalDisposition {
    Completed {
        call: ModelCallId,
        completion_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    },
    Refused {
        call: ModelCallId,
        terminal_frontier: ContextFrontierId,
    },
    Failed {
        failure_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    },
    Cancelled {
        cancellation_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    },
    ReconciliationRequired {
        operation: ReconciliationOperation,
        terminal_frontier: ContextFrontierId,
    },
    Retired,
}

/// Closed goal event kind visible across the seam.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalEventKind {
    Commissioned,
    Blocked,
    Resumed,
    Achieved,
    UserStopped,
    Superseded,
    SessionClosed,
}

/// One goal lineage event visible across the seam.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GoalChange {
    pub event_ordinal: u64,
    pub generation: u64,
    pub kind: GoalEventKind,
}

/// One ownership transition visible across the seam.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OwnershipChange {
    pub event_ordinal: u64,
    pub transition: SessionOwnershipTransition,
    pub actor: LifecycleActor,
}

/// Durable command settlement visible across the seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandSettlement {
    Applied,
    Rejected { kind: String },
}

/// Accepted input settlement visible across the seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InjectionOutcome {
    Delivered { turn: Option<TurnId> },
    NotDelivered,
    Rejected { kind: String },
}

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

    /// Returns the event's session, absent for a rejected creation or a
    /// lifecycle command that names an unknown session.
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
            LifecycleEventKind::SessionCreated(SessionCreated {
                cause: value.cause,
                ownership: value.ownership,
            })
        }
        DispatchedOutboxEventKind::SessionStateChanged(value) => {
            LifecycleEventKind::SessionStateChanged(SessionStateChanged {
                prior: project_session_state(value.prior),
                state: value.state,
                actor: value.actor,
            })
        }
        DispatchedOutboxEventKind::SessionTerminal(value) => {
            LifecycleEventKind::SessionTerminal(SessionTerminal {
                prior: project_session_state(value.prior),
                outcome: value.outcome,
                standing: value.standing,
                actor: value.actor,
            })
        }
        DispatchedOutboxEventKind::TurnTerminal { turn, disposition } => {
            LifecycleEventKind::TurnTerminal {
                turn,
                disposition: project_turn_terminal(disposition),
            }
        }
        DispatchedOutboxEventKind::GoalChanged(value) => {
            LifecycleEventKind::GoalChanged(project_goal_change(value))
        }
        DispatchedOutboxEventKind::CommandSettled { command, result } => {
            LifecycleEventKind::CommandSettled {
                command,
                result: match result {
                    DispatchedCommandSettlement::Applied => CommandSettlement::Applied,
                    DispatchedCommandSettlement::Rejected { kind } => {
                        CommandSettlement::Rejected { kind }
                    }
                },
            }
        }
        DispatchedOutboxEventKind::InjectionSettled { command, outcome } => {
            LifecycleEventKind::InjectionSettled {
                command,
                outcome: match outcome {
                    DispatchedInjectionOutcome::Delivered { turn } => {
                        InjectionOutcome::Delivered { turn }
                    }
                    DispatchedInjectionOutcome::NotDelivered => InjectionOutcome::NotDelivered,
                    DispatchedInjectionOutcome::Rejected { kind } => {
                        InjectionOutcome::Rejected { kind }
                    }
                },
            }
        }
        DispatchedOutboxEventKind::SessionOwnershipChanged(value) => {
            LifecycleEventKind::SessionOwnershipChanged(OwnershipChange {
                event_ordinal: value.event_ordinal,
                transition: value.transition,
                actor: value.actor,
            })
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

const fn project_session_state(value: DispatchedSessionStateKind) -> SessionStateKind {
    match value {
        DispatchedSessionStateKind::Created => SessionStateKind::Created,
        DispatchedSessionStateKind::Dispatched => SessionStateKind::Dispatched,
        DispatchedSessionStateKind::Active => SessionStateKind::Active,
        DispatchedSessionStateKind::Waiting => SessionStateKind::Waiting,
        DispatchedSessionStateKind::Recovering => SessionStateKind::Recovering,
        DispatchedSessionStateKind::Blocked => SessionStateKind::Blocked,
        DispatchedSessionStateKind::Parked => SessionStateKind::Parked,
    }
}

fn project_turn_terminal(value: DispatchedTurnTerminalDisposition) -> TurnTerminalDisposition {
    match value {
        DispatchedTurnTerminalDisposition::Completed {
            call,
            completion_entry,
            terminal_frontier,
        } => TurnTerminalDisposition::Completed {
            call,
            completion_entry,
            terminal_frontier,
        },
        DispatchedTurnTerminalDisposition::Refused {
            call,
            terminal_frontier,
        } => TurnTerminalDisposition::Refused {
            call,
            terminal_frontier,
        },
        DispatchedTurnTerminalDisposition::Failed {
            failure_entry,
            terminal_frontier,
        } => TurnTerminalDisposition::Failed {
            failure_entry,
            terminal_frontier,
        },
        DispatchedTurnTerminalDisposition::Cancelled {
            cancellation_entry,
            terminal_frontier,
        } => TurnTerminalDisposition::Cancelled {
            cancellation_entry,
            terminal_frontier,
        },
        DispatchedTurnTerminalDisposition::ReconciliationRequired {
            operation,
            terminal_frontier,
        } => TurnTerminalDisposition::ReconciliationRequired {
            operation: match operation {
                DispatchedReconciliationOperation::ModelCall(call) => {
                    ReconciliationOperation::ModelCall(call)
                }
                DispatchedReconciliationOperation::ToolAttempt(attempt) => {
                    ReconciliationOperation::ToolAttempt(attempt)
                }
            },
            terminal_frontier,
        },
        DispatchedTurnTerminalDisposition::Retired => TurnTerminalDisposition::Retired,
    }
}

const fn project_goal_change(value: DispatchedGoalChange) -> GoalChange {
    use signalbox_persistence::mapping::GoalEventDiscriminator;

    GoalChange {
        event_ordinal: value.event_ordinal,
        generation: value.generation,
        kind: match value.kind {
            GoalEventDiscriminator::Commissioned => GoalEventKind::Commissioned,
            GoalEventDiscriminator::Blocked => GoalEventKind::Blocked,
            GoalEventDiscriminator::Resumed => GoalEventKind::Resumed,
            GoalEventDiscriminator::Achieved => GoalEventKind::Achieved,
            GoalEventDiscriminator::UserStopped => GoalEventKind::UserStopped,
            GoalEventDiscriminator::Superseded => GoalEventKind::Superseded,
            GoalEventDiscriminator::SessionClosed => GoalEventKind::SessionClosed,
        },
    }
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

impl SessionCommand {
    /// Admits a repository-watch-dispatched create-session command.
    pub fn create_session(command: CreateSession) -> Result<Self, CommandOutsideSeam> {
        if !matches!(
            command.provenance().cause(),
            signalbox_domain::SessionCreationCause::ModuleDispatched {
                dispatch: signalbox_domain::ModuleDispatch::RepositoryWatch { .. }
            }
        ) {
            return Err(CommandOutsideSeam);
        }
        Ok(Self {
            payload: SessionCommandPayload::CreateSession(command),
        })
    }

    /// Admits the ordinary typed input-submission command.
    pub fn submit_input(command: SubmitInput) -> Result<Self, CommandOutsideSeam> {
        if command.actor() != signalbox_domain::Actor::User {
            return Err(CommandOutsideSeam);
        }
        Ok(Self {
            payload: SessionCommandPayload::SubmitInput(command),
        })
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
            | SessionLifecycleOperation::Stop {
                sticky: signalbox_domain::StopStickiness::Sticky,
                ..
            }
            | SessionLifecycleOperation::Adopt { .. }
            | SessionLifecycleOperation::Release => Ok(Self {
                payload: SessionCommandPayload::Lifecycle(command),
            }),
            SessionLifecycleOperation::Supersede { .. }
            | SessionLifecycleOperation::Stop {
                sticky: signalbox_domain::StopStickiness::Redispatchable,
                ..
            }
            | SessionLifecycleOperation::Abandon
            | SessionLifecycleOperation::CloseFailed { .. }
            | SessionLifecycleOperation::Resume => Err(CommandOutsideSeam),
        }
    }

    /// Consumes the wrapper and returns the existing typed core command.
    pub fn into_payload(self) -> SessionCommandPayload {
        self.payload
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

#[cfg(test)]
mod tests {
    use super::{
        CreateSession, FinishCondition, FinishConditionStatement, SessionCommand,
        SessionLifecycleCommand, SessionLifecycleOperation, SubmitInput,
    };
    use signalbox_domain::{
        DeliveryRequest, DescendantTerminationScope, DirectModelSelection, DurableCommandId,
        ModelSelectionOverride, ModelSelectionRequest, ModuleDispatch,
        PerInputConfigurationChoices, RepoWatchDispatchId, SessionConfigurationDefaults,
        SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
        SessionId, StopStickiness, TranscriptAncestry, TurnId, UserContent,
    };
    use uuid::Uuid;

    fn lifecycle(operation: SessionLifecycleOperation) -> SessionLifecycleCommand {
        SessionLifecycleCommand::new(
            DurableCommandId::from_uuid(Uuid::from_u128(1)),
            SessionId::from_uuid(Uuid::from_u128(2)),
            operation,
        )
    }

    fn defaults() -> SessionConfigurationDefaults {
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(Uuid::from_u128(4)),
        ))
    }

    fn ordinary_input() -> SubmitInput {
        SubmitInput::new(
            DurableCommandId::from_uuid(Uuid::from_u128(5)),
            SessionId::from_uuid(Uuid::from_u128(6)),
            UserContent::try_text(String::from("module input")).expect("fixture text is admitted"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )
    }

    #[test]
    fn closed_lifecycle_commands_admit_sticky_stop_but_not_supersede() {
        let statement = FinishConditionStatement::try_new(String::from("merge when green"))
            .expect("fixture finish condition is admitted");
        assert!(
            SessionCommand::lifecycle(lifecycle(SessionLifecycleOperation::Adopt {
                finish_condition: Some(FinishCondition::Declared(statement)),
            }))
            .is_ok()
        );
        assert!(
            SessionCommand::lifecycle(lifecycle(SessionLifecycleOperation::Stop {
                sticky: StopStickiness::Sticky,
                descendant_scope: DescendantTerminationScope::ParentAlone,
            }))
            .is_ok()
        );
        assert!(
            SessionCommand::lifecycle(lifecycle(SessionLifecycleOperation::Stop {
                sticky: StopStickiness::Redispatchable,
                descendant_scope: DescendantTerminationScope::ParentAlone,
            }))
            .is_err()
        );
        assert!(
            SessionCommand::lifecycle(lifecycle(SessionLifecycleOperation::Supersede {
                successor: SessionId::from_uuid(Uuid::from_u128(3)),
            }))
            .is_err()
        );
    }

    #[test]
    fn create_session_admits_only_repository_watch_dispatch_provenance() {
        let repository_watch = CreateSession::new(
            DurableCommandId::from_uuid(Uuid::from_u128(7)),
            SessionCreationProvenance::module_dispatched(ModuleDispatch::RepositoryWatch {
                dispatch: RepoWatchDispatchId::from_uuid(Uuid::from_u128(8)),
            }),
            defaults(),
        );
        assert!(SessionCommand::create_session(repository_watch).is_ok());

        let interactive = CreateSession::new(
            DurableCommandId::from_uuid(Uuid::from_u128(9)),
            SessionCreationProvenance::new(
                SessionCreationCause::Interactive,
                TranscriptAncestry::None,
            ),
            defaults(),
        );
        assert!(SessionCommand::create_session(interactive).is_err());
    }

    #[test]
    fn submit_input_admits_user_shape_and_rejects_core_interrupt() {
        assert!(SessionCommand::submit_input(ordinary_input()).is_ok());

        let core_interrupt = SubmitInput::new_core_interrupt(
            DurableCommandId::from_uuid(Uuid::from_u128(10)),
            SessionId::from_uuid(Uuid::from_u128(11)),
            UserContent::try_text(String::from("core interrupt"))
                .expect("fixture text is admitted"),
            TurnId::from_uuid(Uuid::from_u128(12)),
            DescendantTerminationScope::ParentAlone,
            PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        );
        assert!(SessionCommand::submit_input(core_interrupt).is_err());
    }
}
