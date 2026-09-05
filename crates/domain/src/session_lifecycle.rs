//! Durable session lifecycle (docs/spec/session-lifecycle.md): eight states,
//! their typed detail, the closed terminal-outcome vocabulary, ownership, and
//! the actor classification every transition records.
//!
//! The turn machine persists unchanged beneath this one. The session state is
//! authoritative and moves in the same transaction as every turn or goal
//! transition that changes the mapping, so the two machines never disagree.

use crate::{
    Actor, CommissionedDispatchId, GoalBlockedReasonKind, RepoWatchDispatchId, SessionId,
    ToolRequestId, TurnId,
};

/// A module that dispatches sessions.
///
/// The set is closed and holds only modules with a producing dispatch path, so
/// no spelling here is a placeholder. The `module{name}` actor reads this value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DispatchingModule {
    /// Repository watch dispatched the session from a matched rule.
    RepositoryWatch,
    /// An operator commissioned the session under a recorded authority fence.
    CommissionedDispatch,
}

/// The exact dispatch one module-dispatched session came from.
///
/// The variant names the dispatching module and carries that module's own
/// durable dispatch identity, so the reference is a typed identity rather than
/// a string a reader has to interpret.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ModuleDispatch {
    /// One repository-watch rule dispatch.
    RepositoryWatch {
        /// The dispatch record this session was created for.
        dispatch: RepoWatchDispatchId,
    },
    /// One operator-commissioned dispatch.
    Commissioned {
        /// The commission this session was created for.
        dispatch: CommissionedDispatchId,
    },
}

impl ModuleDispatch {
    /// Returns which module dispatched the session.
    pub const fn module(&self) -> DispatchingModule {
        match self {
            Self::RepositoryWatch { .. } => DispatchingModule::RepositoryWatch,
            Self::Commissioned { .. } => DispatchingModule::CommissionedDispatch,
        }
    }
}

/// Agency behind a `core` lifecycle transition.
///
/// Model- and tool-initiated agency classifies as `core` because neither is an
/// operator nor a module, and the exact acting identity is kept rather than
/// erased by the classification.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CoreAgency {
    /// Daemon machinery acting with no model or tool agency behind it.
    Daemon,
    /// Model output from one exact turn.
    Model {
        /// The turn whose model output acted.
        turn: TurnId,
    },
    /// Execution of one exact tool request.
    Tool {
        /// The tool request whose execution acted.
        request: ToolRequestId,
    },
}

/// The actor classification recorded with every lifecycle transition.
///
/// This classifies the domain [`Actor`] rather than replacing it: the domain
/// algebra, its wire projection, and its replay-equality contract are
/// untouched, and every command keeps its exact domain actor.
///
/// `Module` has no domain-actor spelling. A module principal is authenticated
/// at the command boundary and supplied alongside the domain actor; the
/// dispatching module of a module-dispatched creation is the one such
/// principal core can name on its own today.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LifecycleActor {
    /// Daemon core, carrying the exact model or tool identity behind it.
    Core {
        /// The agency the classification came from.
        agency: CoreAgency,
    },
    /// The single user's authority, however connected.
    Operator,
    /// One exact module.
    Module {
        /// The module that acted.
        module: DispatchingModule,
    },
    /// The recovery scan or liveness watchdog.
    Watchdog,
}

impl LifecycleActor {
    /// Classifies one domain actor.
    ///
    /// `User` reads as `operator`, the recovery scan as `watchdog`, and model-
    /// and tool-initiated agency as `core` with the acting identity retained.
    pub const fn classify(actor: Actor) -> Self {
        match actor {
            Actor::User => Self::Operator,
            Actor::Core => Self::Core {
                agency: CoreAgency::Daemon,
            },
            Actor::Recovery => Self::Watchdog,
            Actor::Model { turn } => Self::Core {
                agency: CoreAgency::Model { turn },
            },
            Actor::Tool { request } => Self::Core {
                agency: CoreAgency::Tool { request },
            },
        }
    }
}

/// Whether the daemon holds a liveness obligation for one session.
///
/// Owned means one state, one armed deadline, and a driven path to a declared
/// terminal outcome. Unmonitored means a conversation: no deadlines, no
/// auto-resume, no slot held, and no place in occupancy accounting. Turn
/// liveness is not on that list — a dead turn is recovered whoever owns the
/// session, because leaving one active would block the next input every
/// non-terminal session is promised.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionOwnership {
    /// The daemon drives this session to a declared terminal outcome.
    Owned,
    /// A conversation the daemon does not drive.
    Unmonitored,
}

impl SessionOwnership {
    /// Whether the daemon holds a liveness obligation.
    pub const fn is_owned(&self) -> bool {
        match self {
            Self::Owned => true,
            Self::Unmonitored => false,
        }
    }
}

/// The closed waiting-kind vocabulary.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionWaitKind {
    /// A tool-approval decision is outstanding.
    Approval,
    /// An external gate must change before work continues.
    External,
    /// A delegated child session must settle.
    Child,
    /// A provider backoff is running.
    ProviderRetry,
    /// Dispatch pipeline backlog holds the session.
    Pipeline,
    /// A recorded scheduler fault holds the session.
    Scheduler,
}

/// The machinery that ends one wait.
///
/// A waiting session designates its waker. Each wait kind has
/// exactly one, so the designation is total rather than a free choice a caller
/// could get wrong.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionWaker {
    /// The approval decision path.
    ApprovalDecision,
    /// The external-gate recheck.
    ExternalRecheck,
    /// Child settlement returning the delegated result.
    ChildSettlement,
    /// Provider backoff elapsing.
    ProviderBackoff,
    /// The pipeline draining.
    PipelineDrain,
    /// The eligibility sweep.
    SchedulerSweep,
}

/// The typed waiting detail.
///
/// The waiting state's deadline member is the session's armed deadline record and is
/// not repeated here: a second copy could only disagree with the record the
/// invariant is defined over.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionWait {
    /// Awaiting a tool-approval decision.
    Approval,
    /// Awaiting an external gate.
    External,
    /// Awaiting one exact delegated child.
    Child {
        /// The child session whose settlement is awaited.
        session: SessionId,
    },
    /// Awaiting a provider backoff.
    ProviderRetry,
    /// Awaiting pipeline capacity.
    Pipeline,
    /// Awaiting recovery from a recorded scheduler fault.
    Scheduler,
}

impl SessionWait {
    /// Returns the closed wait kind.
    pub const fn kind(&self) -> SessionWaitKind {
        match self {
            Self::Approval => SessionWaitKind::Approval,
            Self::External => SessionWaitKind::External,
            Self::Child { .. } => SessionWaitKind::Child,
            Self::ProviderRetry => SessionWaitKind::ProviderRetry,
            Self::Pipeline => SessionWaitKind::Pipeline,
            Self::Scheduler => SessionWaitKind::Scheduler,
        }
    }

    /// Returns the designated waker.
    pub const fn waker(&self) -> SessionWaker {
        match self {
            Self::Approval => SessionWaker::ApprovalDecision,
            Self::External => SessionWaker::ExternalRecheck,
            Self::Child { .. } => SessionWaker::ChildSettlement,
            Self::ProviderRetry => SessionWaker::ProviderBackoff,
            Self::Pipeline => SessionWaker::PipelineDrain,
            Self::Scheduler => SessionWaker::SchedulerSweep,
        }
    }
}

/// Which recovery a session is waiting out.
///
/// The three variants are exactly the three `awaiting_*_recovery` turn phases
/// the lifecycle maps onto `recovering`. The recovering state's bound member is
/// the armed deadline record, for the same reason a wait's deadline is.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionRecoveryOperation {
    /// A model call is being reconciled.
    ModelCall,
    /// A tool attempt is being reconciled.
    Tool,
    /// A runner is being recovered.
    Runner,
}

/// Why an owned session waits on a human.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionParkCause {
    /// Budgeted retries ran out with the retryable cause standing.
    RetryBudgetExhausted,
    /// The same input will fail again.
    StructuralFailure,
    /// Failure with no classified cause.
    UnknownFailure,
    /// The active stall deadline expired.
    ActiveStallDeadlineExpired,
    /// A waiting deadline expired.
    WaitingDeadlineExpired,
    /// A recovery bound expired.
    RecoveringDeadlineExpired,
    /// An operator parked the session directly.
    OperatorHold,
    /// A module park drove the session it wraps to core `parked`.
    ModulePark,
}

impl SessionParkCause {
    /// Whether the standing evidence a park carries is what its cause names.
    ///
    /// A closure reads the standing evidence to classify the outcome, so
    /// a park holding evidence its own cause contradicts closes under a
    /// classification the park never supported -- and an exhaustion holding no
    /// evidence at all cannot say what it exhausted retries or structure on.
    #[must_use]
    pub const fn admits_standing(self, standing: Option<SessionFailureCause>) -> bool {
        match (self, standing) {
            (Self::RetryBudgetExhausted, Some(SessionFailureCause::Retryable(_)))
            | (Self::StructuralFailure, Some(SessionFailureCause::Structural(_))) => true,
            (Self::RetryBudgetExhausted | Self::StructuralFailure, _) => false,
            (_, standing) => standing.is_none(),
        }
    }
}

/// Who must act on one park.
///
/// A park carries a third member beside its cause and its instant: who is
/// being waited on. That is the operator queue, or the module whose dispatch
/// the session serves — and nothing else, because a park nobody answers is the
/// stuck session this state exists to make visible.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionParkResponder {
    /// The operator queue.
    Operator,
    /// One exact module.
    Module {
        /// The module that must act.
        module: DispatchingModule,
    },
}

/// A provider or infrastructure failure a retry could still clear.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionRetryableCause {
    /// A transient provider failure.
    ProviderTransient,
    /// Provider quota exhaustion.
    ProviderQuotaExhausted,
    /// Provider overload.
    ProviderOverloaded,
    /// An infrastructure blip below the daemon.
    InfrastructureFailure,
    /// Budgeted retries ran out with the retryable cause standing.
    RetryBudgetExhausted,
}

/// A failure the same input will hit again.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionStructuralCause {
    /// Compaction could not make room.
    ContextCompactionWall,
    /// Context headroom ran out with no compaction available.
    ContextHeadroomExhausted,
    /// The session's toolchain is broken.
    BrokenToolchain,
    /// A moderation block whose resume re-trips the same flag.
    ModerationBlock,
}

/// Why a session that never did the work was retired.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionRetirementCause {
    /// The session never reached its first activity before admission expired.
    AdmissionDeadlineExpired,
    /// The one-time closure of a stranded queued-turn session.
    StrandedQueuedTurn,
}

/// A standing failure cause a park closes with.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionFailureCause {
    /// A retryable cause.
    Retryable(SessionRetryableCause),
    /// A structural cause.
    Structural(SessionStructuralCause),
}

/// Whether a stop suppresses re-dispatch until its source is updated.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StopStickiness {
    /// Re-dispatch is suppressed until the dispatch source is updated.
    Sticky,
    /// Re-dispatch may proceed.
    Redispatchable,
}

/// The closed terminal-outcome vocabulary.
///
/// `stopped{actor}`'s actor member is the transition's recorded
/// [`LifecycleActor`], which the same row carries: the stop is the transition,
/// so a second copy could only disagree with it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTerminalOutcome {
    /// The declared finish check passed. Slots and worktrees are released.
    AchievedVerified,
    /// Achieved on the model's word: no declared finish check passed.
    AchievedDeclared,
    /// The session closed with a retryable cause standing.
    FailedRetryable {
        /// The standing cause.
        cause: SessionRetryableCause,
    },
    /// The session closed with a structural cause standing.
    FailedStructural {
        /// The standing cause.
        cause: SessionStructuralCause,
    },
    /// The session closed with no classified cause.
    FailedUnknown,
    /// A human or rule stopped the session.
    Stopped {
        /// Whether re-dispatch stays suppressed.
        sticky: StopStickiness,
    },
    /// A newer session owns the work, or the work itself is gone.
    Superseded {
        /// The successor, when one exists.
        by: Option<SessionId>,
    },
    /// An operator wrote off a parked session.
    Abandoned,
    /// The session never did the work and never will.
    Retired {
        /// Which admission or closure predicate retired it.
        cause: SessionRetirementCause,
    },
}

/// The outcomes that settle a live goal generation with their own event.
///
/// `achieved_verified` is absent because the goal contract already spells
/// it: a verified achievement settles as `achieved`, and appending a second
/// terminal event for the same act would record one closure twice. A
/// session-level stop records `stopped` here; `user_stopped` stays the goal
/// command's own event.
///
/// The member each outcome carries — the standing cause, the successor, the
/// retirement predicate — stays on the lifecycle satellite the same
/// transaction writes. The goal lineage records which outcome closed it, not a
/// second copy of the outcome's payload.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionClosureOutcome {
    /// The session closed with a retryable cause standing.
    FailedRetryable,
    /// The session closed with a structural cause standing.
    FailedStructural,
    /// The session closed with no classified cause.
    FailedUnknown,
    /// A human or rule stopped the session.
    Stopped,
    /// A newer session owns the work, or the work is gone.
    Superseded,
    /// An operator wrote the session off.
    Abandoned,
    /// The session never did the work and never will.
    Retired,
}

impl SessionTerminalOutcome {
    /// Returns the goal event this outcome owes a live generation.
    ///
    /// `None` means the goal contract already has the event: an achievement
    /// settles as `achieved`.
    pub const fn closure_outcome(&self) -> Option<SessionClosureOutcome> {
        match self {
            Self::AchievedVerified | Self::AchievedDeclared => None,
            Self::Stopped { .. } => Some(SessionClosureOutcome::Stopped),
            Self::FailedRetryable { .. } => Some(SessionClosureOutcome::FailedRetryable),
            Self::FailedStructural { .. } => Some(SessionClosureOutcome::FailedStructural),
            Self::FailedUnknown => Some(SessionClosureOutcome::FailedUnknown),
            Self::Superseded { .. } => Some(SessionClosureOutcome::Superseded),
            Self::Abandoned => Some(SessionClosureOutcome::Abandoned),
            Self::Retired { .. } => Some(SessionClosureOutcome::Retired),
        }
    }

    /// Whether this outcome forbids further escalation or notification.
    ///
    /// `superseded` releases everything: a successor owns the work, so an
    /// escalation against the predecessor would page for work already moving.
    pub const fn forbids_further_escalation(&self) -> bool {
        match self {
            Self::Superseded { .. } => true,
            Self::AchievedVerified
            | Self::AchievedDeclared
            | Self::FailedRetryable { .. }
            | Self::FailedStructural { .. }
            | Self::FailedUnknown
            | Self::Stopped { .. }
            | Self::Abandoned
            | Self::Retired { .. } => false,
        }
    }

    /// Whether this outcome releases the session's held slot, worktree, and
    /// containers.
    ///
    /// Every terminal outcome does, inline at the closure: a session that will
    /// never run again holds nothing.
    pub const fn releases_resources(&self) -> bool {
        true
    }
}

/// One session's durable lifecycle state.
///
/// Exactly one of eight, with the typed detail its state carries. `parked`
/// overrides the turn mapping: parking suspends a live turn in place and
/// terminalizes nothing.
///
/// A state is not an ownership claim: the states describe, ownership governs,
/// and no state arms a deadline on an unmonitored session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLifecycleState {
    /// Created and not yet dispatched.
    Created,
    /// Dispatched, with no turn activated yet.
    Dispatched,
    /// A turn is running.
    Active,
    /// A turn is waiting on something outside itself.
    Waiting {
        /// What is being waited on.
        wait: SessionWait,
    },
    /// A recovery is running for one operation.
    Recovering {
        /// Which operation is being recovered.
        operation: SessionRecoveryOperation,
    },
    /// A blocked goal with no live turn.
    Blocked {
        /// The goal's closed blocked reason.
        reason: GoalBlockedReasonKind,
        /// Resume cycles this blocked generation has already had.
        cycle: u64,
    },
    /// Suspended, waiting on a human.
    Parked {
        /// Why the session parked.
        cause: SessionParkCause,
        /// Who must act.
        responder: SessionParkResponder,
        /// The standing failure cause a closure would carry forward.
        standing: Option<SessionFailureCause>,
    },
    /// Closed with a declared outcome.
    Terminal {
        /// The declared outcome.
        outcome: SessionTerminalOutcome,
    },
}

impl SessionLifecycleState {
    /// Whether the session has closed.
    pub const fn is_terminal(&self) -> bool {
        match self {
            Self::Terminal { .. } => true,
            Self::Created
            | Self::Dispatched
            | Self::Active
            | Self::Waiting { .. }
            | Self::Recovering { .. }
            | Self::Blocked { .. }
            | Self::Parked { .. } => false,
        }
    }

    /// Whether the session is suspended in place.
    ///
    /// A parked session's rows are neither eligibility-sweep candidates nor
    /// liveness-watchdog candidates until it leaves `parked`.
    pub const fn is_parked(&self) -> bool {
        match self {
            Self::Parked { .. } => true,
            Self::Created
            | Self::Dispatched
            | Self::Active
            | Self::Waiting { .. }
            | Self::Recovering { .. }
            | Self::Blocked { .. }
            | Self::Terminal { .. } => false,
        }
    }

    /// Whether this is one of the four states the turn/goal mapping derives.
    const fn is_mapped(&self) -> bool {
        match self {
            Self::Active
            | Self::Waiting { .. }
            | Self::Recovering { .. }
            | Self::Blocked { .. } => true,
            Self::Created | Self::Dispatched | Self::Parked { .. } | Self::Terminal { .. } => false,
        }
    }

    /// Whether a transition to `next` is admitted by the lifecycle algebra.
    ///
    /// The four mapped states move freely between one another because the turn
    /// and goal machines below decide them: the session state follows the
    /// mapping in the same transaction, so a turn moving from an approval wait
    /// straight into recovery moves the session with it.
    pub const fn admits(&self, next: &Self) -> bool {
        match (self, next) {
            (Self::Parked { .. }, Self::Created | Self::Dispatched) => true,
            (Self::Terminal { .. }, _) | (_, Self::Created) => false,
            (_, Self::Terminal { outcome }) => self.admits_outcome(outcome),
            (Self::Created, Self::Dispatched) => true,
            (
                Self::Created | Self::Dispatched,
                Self::Parked {
                    cause: SessionParkCause::ModulePark,
                    ..
                },
            ) => true,
            (Self::Created, _) => false,
            (Self::Dispatched, Self::Parked { .. }) => false,
            (Self::Dispatched | Self::Parked { .. }, _) => next.is_mapped(),
            (_, Self::Parked { .. }) => self.is_mapped(),
            _ => self.is_mapped() && next.is_mapped(),
        }
    }

    /// Whether this state may close with `outcome`.
    ///
    /// `retired` says the session never did the work, so only the two
    /// admission states reach it, with either their shared admission expiry or
    /// a dispatched queued-turn retirement. Every other outcome closes from
    /// any non-terminal state, and the parked-only closures are commands
    /// rather than shapes this algebra can distinguish.
    const fn admits_outcome(&self, outcome: &SessionTerminalOutcome) -> bool {
        match outcome {
            SessionTerminalOutcome::Retired { cause } => match (self, cause) {
                (
                    Self::Created | Self::Dispatched,
                    SessionRetirementCause::AdmissionDeadlineExpired,
                )
                | (Self::Dispatched, SessionRetirementCause::StrandedQueuedTurn) => true,
                (Self::Created, SessionRetirementCause::StrandedQueuedTurn) => false,
                (
                    Self::Active
                    | Self::Waiting { .. }
                    | Self::Recovering { .. }
                    | Self::Blocked { .. }
                    | Self::Parked { .. }
                    | Self::Terminal { .. },
                    _,
                ) => false,
            },
            SessionTerminalOutcome::AchievedVerified
            | SessionTerminalOutcome::AchievedDeclared
            | SessionTerminalOutcome::FailedRetryable { .. }
            | SessionTerminalOutcome::FailedStructural { .. }
            | SessionTerminalOutcome::FailedUnknown
            | SessionTerminalOutcome::Stopped { .. }
            | SessionTerminalOutcome::Superseded { .. }
            | SessionTerminalOutcome::Abandoned => !self.is_terminal(),
        }
    }

    /// Applies one checked transition.
    pub fn transition(self, next: Self) -> Result<Self, SessionLifecycleTransitionError> {
        if self.admits(&next) {
            Ok(next)
        } else {
            Err(SessionLifecycleTransitionError {
                from: self,
                to: next,
            })
        }
    }
}

/// A transition the lifecycle algebra does not admit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionLifecycleTransitionError {
    from: SessionLifecycleState,
    to: SessionLifecycleState,
}

impl SessionLifecycleTransitionError {
    /// Returns the state the session held.
    pub const fn from(&self) -> SessionLifecycleState {
        self.from
    }

    /// Returns the state that was rejected.
    pub const fn to(&self) -> SessionLifecycleState {
        self.to
    }
}

impl core::fmt::Display for SessionLifecycleTransitionError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            formatter,
            "session lifecycle does not admit {:?} from {:?}",
            self.to, self.from
        )
    }
}

impl core::error::Error for SessionLifecycleTransitionError {}

/// The transition one expired deadline fires.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionDeadlineExpiry {
    /// Close the session `terminal{retired}`.
    Retire,
    /// Move the session to `parked`.
    Park,
}

/// The closed vocabulary of armed session deadlines.
///
/// A state with no deadline is unbounded, not broken.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionDeadlineKind {
    /// An owned session that never started working.
    Admission,
    /// An active or recovering session making no progress.
    ActiveStall,
    /// A session waiting on something outside it.
    Waiting,
}

impl SessionDeadlineKind {
    /// Returns the transition this deadline's expiry fires.
    pub const fn on_expiry(&self) -> SessionDeadlineExpiry {
        match self {
            Self::Admission => SessionDeadlineExpiry::Retire,
            Self::ActiveStall | Self::Waiting => SessionDeadlineExpiry::Park,
        }
    }

    /// Returns the deadline one non-terminal state arms.
    pub const fn for_state(state: &SessionLifecycleState) -> Option<Self> {
        match state {
            SessionLifecycleState::Created | SessionLifecycleState::Dispatched => {
                Some(Self::Admission)
            }
            SessionLifecycleState::Active | SessionLifecycleState::Recovering { .. } => {
                Some(Self::ActiveStall)
            }
            SessionLifecycleState::Waiting { .. } => Some(Self::Waiting),
            SessionLifecycleState::Blocked { .. }
            | SessionLifecycleState::Parked { .. }
            | SessionLifecycleState::Terminal { .. } => None,
        }
    }
}

/// How one session's ownership bit reached its current value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionOwnershipTransition {
    /// The session was created owned.
    CreatedOwned,
    /// The session was created as an unmonitored conversation.
    CreatedUnmonitored,
    /// An adopt took the liveness obligation.
    Adopted,
    /// A release dropped the forward obligations.
    Released,
}

impl SessionOwnershipTransition {
    /// Returns the ownership the session holds after this transition.
    pub const fn ownership(&self) -> SessionOwnership {
        match self {
            Self::CreatedOwned | Self::Adopted => SessionOwnership::Owned,
            Self::CreatedUnmonitored | Self::Released => SessionOwnership::Unmonitored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CoreAgency, DispatchingModule, LifecycleActor, ModuleDispatch, SessionClosureOutcome,
        SessionDeadlineExpiry, SessionDeadlineKind, SessionLifecycleState, SessionOwnership,
        SessionOwnershipTransition, SessionParkCause, SessionParkResponder,
        SessionRecoveryOperation, SessionRetirementCause, SessionStructuralCause,
        SessionTerminalOutcome, SessionWait, SessionWaitKind, SessionWaker, StopStickiness,
    };
    use crate::{
        Actor, CommissionedDispatchId, GoalBlockedReasonKind, RepoWatchDispatchId, SessionId,
        test_support::{session_id, tool_request_id, turn_id},
    };

    fn dispatched() -> SessionLifecycleState {
        SessionLifecycleState::Dispatched
    }

    fn parked() -> SessionLifecycleState {
        SessionLifecycleState::Parked {
            cause: SessionParkCause::StructuralFailure,
            responder: SessionParkResponder::Operator,
            standing: None,
        }
    }

    fn module_parked() -> SessionLifecycleState {
        SessionLifecycleState::Parked {
            cause: SessionParkCause::ModulePark,
            responder: SessionParkResponder::Module {
                module: DispatchingModule::RepositoryWatch,
            },
            standing: None,
        }
    }

    fn stopped() -> SessionLifecycleState {
        SessionLifecycleState::Terminal {
            outcome: SessionTerminalOutcome::Stopped {
                sticky: StopStickiness::Sticky,
            },
        }
    }

    fn retired() -> SessionLifecycleState {
        SessionLifecycleState::Terminal {
            outcome: SessionTerminalOutcome::Retired {
                cause: SessionRetirementCause::AdmissionDeadlineExpired,
            },
        }
    }

    #[track_caller]
    fn assert_admits(from: SessionLifecycleState, to: SessionLifecycleState) {
        assert!(from.admits(&to), "{from:?} should admit {to:?}");
    }

    #[track_caller]
    fn assert_rejects(from: SessionLifecycleState, to: SessionLifecycleState) {
        assert!(!from.admits(&to), "{from:?} should reject {to:?}");
        let error = from.transition(to).unwrap_err();
        assert_eq!(error.from(), from);
        assert_eq!(error.to(), to);
    }

    #[test]
    fn admission_states_reach_dispatch_and_activation() {
        assert_admits(SessionLifecycleState::Created, dispatched());
        assert_admits(dispatched(), SessionLifecycleState::Active);
        assert_admits(
            dispatched(),
            SessionLifecycleState::Waiting {
                wait: SessionWait::Approval,
            },
        );
    }

    #[test]
    fn a_creation_never_activates_without_dispatch() {
        assert_rejects(
            SessionLifecycleState::Created,
            SessionLifecycleState::Active,
        );
    }

    #[test]
    fn the_mapped_states_follow_the_turn_between_one_another() {
        assert_admits(
            SessionLifecycleState::Active,
            SessionLifecycleState::Waiting {
                wait: SessionWait::Approval,
            },
        );
        assert_admits(
            SessionLifecycleState::Waiting {
                wait: SessionWait::Approval,
            },
            SessionLifecycleState::Recovering {
                operation: SessionRecoveryOperation::ModelCall,
            },
        );
        assert_admits(
            SessionLifecycleState::Recovering {
                operation: SessionRecoveryOperation::Tool,
            },
            SessionLifecycleState::Active,
        );
        assert_admits(
            SessionLifecycleState::Blocked {
                reason: GoalBlockedReasonKind::UserInputRequired,
                cycle: 0,
            },
            SessionLifecycleState::Active,
        );
    }

    #[test]
    fn parking_suspends_a_mapped_state_and_resuming_returns_to_one() {
        assert_admits(SessionLifecycleState::Active, parked());
        assert_admits(parked(), SessionLifecycleState::Active);
        assert_admits(
            parked(),
            SessionLifecycleState::Waiting {
                wait: SessionWait::Approval,
            },
        );
    }

    #[test]
    fn a_module_can_park_an_admission_state() {
        assert_admits(SessionLifecycleState::Created, module_parked());
        assert_admits(dispatched(), module_parked());
        assert_rejects(SessionLifecycleState::Created, parked());
        assert_rejects(dispatched(), parked());
        assert_admits(module_parked(), SessionLifecycleState::Created);
        assert_admits(module_parked(), dispatched());
    }

    #[test]
    fn a_stranded_queued_turn_only_retires_a_dispatch() {
        assert_admits(
            dispatched(),
            SessionLifecycleState::Terminal {
                outcome: SessionTerminalOutcome::Retired {
                    cause: SessionRetirementCause::StrandedQueuedTurn,
                },
            },
        );
        assert_rejects(
            SessionLifecycleState::Created,
            SessionLifecycleState::Terminal {
                outcome: SessionTerminalOutcome::Retired {
                    cause: SessionRetirementCause::StrandedQueuedTurn,
                },
            },
        );
    }

    #[test]
    fn terminal_is_final() {
        assert_rejects(stopped(), SessionLifecycleState::Active);
        assert_rejects(stopped(), parked());
        assert_rejects(stopped(), retired());
    }

    #[test]
    fn every_non_terminal_state_stops() {
        assert_admits(SessionLifecycleState::Created, stopped());
        assert_admits(dispatched(), stopped());
        assert_admits(SessionLifecycleState::Active, stopped());
        assert_admits(parked(), stopped());
    }

    #[test]
    fn only_an_admission_state_retires() {
        assert_admits(SessionLifecycleState::Created, retired());
        assert_admits(dispatched(), retired());
        assert_rejects(SessionLifecycleState::Active, retired());
        assert_rejects(parked(), retired());
    }

    #[test]
    fn a_park_closes_as_failed_with_its_standing_cause() {
        assert_admits(
            SessionLifecycleState::Parked {
                cause: SessionParkCause::StructuralFailure,
                responder: SessionParkResponder::Module {
                    module: DispatchingModule::RepositoryWatch,
                },
                standing: None,
            },
            SessionLifecycleState::Terminal {
                outcome: SessionTerminalOutcome::FailedStructural {
                    cause: SessionStructuralCause::ContextCompactionWall,
                },
            },
        );
    }

    #[test]
    fn a_transition_returns_the_state_it_moved_to() {
        let moved = SessionLifecycleState::Created
            .transition(SessionLifecycleState::Dispatched)
            .unwrap();
        assert_eq!(moved, SessionLifecycleState::Dispatched);
    }

    #[test]
    fn the_admission_states_share_one_deadline() {
        assert_eq!(
            SessionDeadlineKind::for_state(&SessionLifecycleState::Created),
            Some(SessionDeadlineKind::Admission)
        );
        assert_eq!(
            SessionDeadlineKind::for_state(&dispatched()),
            Some(SessionDeadlineKind::Admission)
        );
    }

    #[test]
    fn active_and_recovering_share_the_stall_deadline() {
        assert_eq!(
            SessionDeadlineKind::for_state(&SessionLifecycleState::Active),
            Some(SessionDeadlineKind::ActiveStall)
        );
        assert_eq!(
            SessionDeadlineKind::for_state(&SessionLifecycleState::Recovering {
                operation: SessionRecoveryOperation::ModelCall,
            }),
            Some(SessionDeadlineKind::ActiveStall)
        );
    }

    /// A state with no deadline is unbounded, not broken: parked, blocked and
    /// terminal sessions arm none.
    #[test]
    fn the_states_without_a_deadline_arm_none() {
        assert_eq!(SessionDeadlineKind::for_state(&parked()), None);
        assert_eq!(SessionDeadlineKind::for_state(&stopped()), None);
    }

    #[test]
    fn every_waiting_kind_arms_the_one_waiting_deadline() {
        assert_eq!(
            SessionDeadlineKind::for_state(&SessionLifecycleState::Waiting {
                wait: SessionWait::Approval,
            }),
            Some(SessionDeadlineKind::Waiting)
        );
        assert_eq!(
            SessionDeadlineKind::for_state(&SessionLifecycleState::Waiting {
                wait: SessionWait::Child {
                    session: session_id(7),
                },
            }),
            Some(SessionDeadlineKind::Waiting)
        );
    }

    #[test]
    fn admission_deadlines_retire_and_post_admission_deadlines_park() {
        assert_eq!(
            SessionDeadlineKind::Admission.on_expiry(),
            SessionDeadlineExpiry::Retire
        );
        assert_eq!(
            SessionDeadlineKind::ActiveStall.on_expiry(),
            SessionDeadlineExpiry::Park
        );
        assert_eq!(
            SessionDeadlineKind::Waiting.on_expiry(),
            SessionDeadlineExpiry::Park
        );
    }

    #[test]
    fn a_wait_designates_exactly_one_waker() {
        assert_eq!(SessionWait::Approval.kind(), SessionWaitKind::Approval);
        assert_eq!(
            SessionWait::Approval.waker(),
            SessionWaker::ApprovalDecision
        );
        assert_eq!(
            SessionWait::Child {
                session: session_id(3),
            }
            .waker(),
            SessionWaker::ChildSettlement
        );
        assert_eq!(
            SessionWait::ProviderRetry.waker(),
            SessionWaker::ProviderBackoff
        );
    }

    #[test]
    fn the_user_classifies_as_operator_and_the_recovery_scan_as_watchdog() {
        assert_eq!(
            LifecycleActor::classify(Actor::User),
            LifecycleActor::Operator
        );
        assert_eq!(
            LifecycleActor::classify(Actor::Recovery),
            LifecycleActor::Watchdog
        );
    }

    #[test]
    fn model_and_tool_agency_classify_as_core_keeping_their_identity() {
        assert_eq!(
            LifecycleActor::classify(Actor::Model { turn: turn_id(4) }),
            LifecycleActor::Core {
                agency: CoreAgency::Model { turn: turn_id(4) },
            }
        );
        assert_eq!(
            LifecycleActor::classify(Actor::Tool {
                request: tool_request_id(5),
            }),
            LifecycleActor::Core {
                agency: CoreAgency::Tool {
                    request: tool_request_id(5),
                },
            }
        );
    }

    #[test]
    fn a_dispatch_names_the_module_that_issued_it() {
        assert_eq!(
            ModuleDispatch::RepositoryWatch {
                dispatch: RepoWatchDispatchId::from_uuid(uuid::Uuid::from_u128(11)),
            }
            .module(),
            DispatchingModule::RepositoryWatch
        );
        assert_eq!(
            ModuleDispatch::Commissioned {
                dispatch: CommissionedDispatchId::from_uuid(uuid::Uuid::from_u128(12)),
            }
            .module(),
            DispatchingModule::CommissionedDispatch
        );
    }

    #[test]
    fn ownership_transitions_state_the_bit_they_leave_behind() {
        assert_eq!(
            SessionOwnershipTransition::CreatedOwned.ownership(),
            SessionOwnership::Owned
        );
        assert_eq!(
            SessionOwnershipTransition::CreatedUnmonitored.ownership(),
            SessionOwnership::Unmonitored
        );
        assert_eq!(
            SessionOwnershipTransition::Adopted.ownership(),
            SessionOwnership::Owned
        );
        assert_eq!(
            SessionOwnershipTransition::Released.ownership(),
            SessionOwnership::Unmonitored
        );
    }

    #[test]
    fn an_achievement_owes_the_goal_no_new_event_and_a_stop_settles_it() {
        assert_eq!(
            SessionTerminalOutcome::AchievedVerified.closure_outcome(),
            None
        );
        assert_eq!(
            SessionTerminalOutcome::Stopped {
                sticky: StopStickiness::Redispatchable,
            }
            .closure_outcome(),
            Some(SessionClosureOutcome::Stopped)
        );
    }

    #[test]
    fn every_other_outcome_settles_the_generation_with_its_own_event() {
        assert_eq!(
            SessionTerminalOutcome::Abandoned.closure_outcome(),
            Some(SessionClosureOutcome::Abandoned)
        );
        assert_eq!(
            SessionTerminalOutcome::FailedStructural {
                cause: SessionStructuralCause::ContextCompactionWall,
            }
            .closure_outcome(),
            Some(SessionClosureOutcome::FailedStructural)
        );
        assert_eq!(
            SessionTerminalOutcome::Retired {
                cause: SessionRetirementCause::AdmissionDeadlineExpired,
            }
            .closure_outcome(),
            Some(SessionClosureOutcome::Retired)
        );
    }

    #[test]
    fn supersession_forbids_further_escalation_and_every_outcome_releases() {
        assert!(
            SessionTerminalOutcome::Superseded {
                by: Some(SessionId::from_uuid(uuid::Uuid::from_u128(9))),
            }
            .forbids_further_escalation()
        );
        assert!(!SessionTerminalOutcome::Abandoned.forbids_further_escalation());
        assert!(SessionTerminalOutcome::AchievedVerified.releases_resources());
        assert!(SessionTerminalOutcome::Abandoned.releases_resources());
    }
}
