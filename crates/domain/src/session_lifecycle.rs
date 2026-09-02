//! Durable session lifecycle: eight states, their typed detail, the closed
//! terminal-outcome vocabulary, ownership, and the actor classification every
//! transition records.
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
/// no spelling here is a placeholder. §6's `module{name}` reads this value.
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

/// The §6 classification recorded with every lifecycle transition.
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
/// watchdogs, no auto-resume, no slot held, and no place in occupancy
/// accounting.
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

/// The closed §1 waiting-kind vocabulary.
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
/// §1 requires a waiting session to designate its waker. Each wait kind has
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

/// §1's typed waiting detail.
///
/// The deadline member §1 names is the session's armed deadline record and is
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
/// §1 maps onto `recovering`. The bound member §1 names is the armed deadline
/// record, for the same reason a wait's deadline is.
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
    /// The active-state progress budget ran out.
    ProgressBudgetExhausted,
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
    /// A blocked generation outlived its bound.
    BlockedDeadlineExpired,
    /// An operator parked the session directly.
    OperatorHold,
    /// A module park drove the session it wraps to core `parked`.
    ModulePark,
}

/// Who owns one park.
///
/// §1 requires a park to name an owner. The owner is who must act: the
/// operator, or the module whose dispatch the session serves.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionParkOwner {
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
    /// The dispatched session never reached `active`.
    DispatchDeadlineExpired,
    /// A held start gate was never released.
    StartGateDeadlineExpired,
    /// An owned creation never received its first input.
    FirstInputDeadlineExpired,
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

/// The closed §2 terminal-outcome vocabulary.
///
/// `stopped{actor}`'s actor member is the transition's recorded
/// [`LifecycleActor`], which the same row carries: the stop is the transition,
/// so a second copy could only disagree with it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTerminalOutcome {
    /// The declared finish check passed. Slots and worktrees are released.
    AchievedVerified,
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
/// `achieved_verified` and `stopped` are absent because the goal contract
/// already spells them: a verified achievement settles as `achieved` and a
/// session stop as `user_stopped`, and appending a second terminal event for
/// the same act would record one closure twice.
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
    /// settles as `achieved` and a stop as `user_stopped`.
    pub const fn closure_outcome(&self) -> Option<SessionClosureOutcome> {
        match self {
            Self::AchievedVerified | Self::Stopped { .. } => None,
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
            | Self::FailedRetryable { .. }
            | Self::FailedStructural { .. }
            | Self::FailedUnknown
            | Self::Stopped { .. }
            | Self::Abandoned
            | Self::Retired { .. } => false,
        }
    }

    /// Whether this outcome releases the session's held slot and worktree.
    ///
    /// Every terminal outcome does: a session that will never run again holds
    /// nothing. The distinction §2 draws is what else each owes —
    /// `abandoned` additionally records container cleanup obligations, and
    /// `superseded` forbids the escalations the others still permit.
    pub const fn releases_resources(&self) -> bool {
        true
    }

    /// Whether this outcome owes worktree and container cleanup obligations.
    pub const fn records_cleanup_obligations(&self) -> bool {
        match self {
            Self::Abandoned => true,
            Self::AchievedVerified
            | Self::FailedRetryable { .. }
            | Self::FailedStructural { .. }
            | Self::FailedUnknown
            | Self::Stopped { .. }
            | Self::Superseded { .. }
            | Self::Retired { .. } => false,
        }
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
        owner: SessionParkOwner,
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

    /// Whether a transition to `next` is admitted by §1's algebra.
    ///
    /// The four mapped states move freely between one another because the turn
    /// and goal machines below decide them: the session state follows the
    /// mapping in the same transaction, so a turn moving from an approval wait
    /// straight into recovery moves the session with it.
    pub const fn admits(&self, next: &Self) -> bool {
        match (self, next) {
            (Self::Terminal { .. }, _) => false,
            (Self::Created, Self::Dispatched) => true,
            (Self::Dispatched, _) if next.is_mapped() => true,
            (Self::Parked { .. }, _) if next.is_mapped() => true,
            (_, Self::Parked { .. }) if self.is_mapped() => true,
            (Self::Terminal { .. }, _) | (_, Self::Created) => false,
            (_, Self::Terminal { outcome }) => self.admits_outcome(outcome),
            _ => self.is_mapped() && next.is_mapped(),
        }
    }

    /// Whether this state may close with `outcome`.
    ///
    /// `retired` says the session never did the work, so only the two
    /// admission states reach it; every other outcome closes from any
    /// non-terminal state, and the parked-only closures are commands (§7)
    /// rather than shapes this algebra can distinguish.
    const fn admits_outcome(&self, outcome: &SessionTerminalOutcome) -> bool {
        match outcome {
            SessionTerminalOutcome::Retired { .. } => match self {
                Self::Created | Self::Dispatched => true,
                Self::Active
                | Self::Waiting { .. }
                | Self::Recovering { .. }
                | Self::Blocked { .. }
                | Self::Parked { .. }
                | Self::Terminal { .. } => false,
            },
            SessionTerminalOutcome::AchievedVerified
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

/// A transition §1's algebra does not admit.
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
///
/// Every armed deadline has one: an expiry is a transition, never a silent
/// hold. Admission expiries retire; post-admission expiries park; a parked
/// deadline re-notifies and re-arms without moving the session.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionDeadlineExpiry {
    /// Close the session `terminal{retired}`.
    Retire,
    /// Move the session to `parked`.
    Park,
    /// Re-raise the operator alert and re-arm, leaving the state alone.
    Renotify,
}

/// The closed vocabulary of armed session deadlines.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionDeadlineKind {
    /// `dispatched` to `active`.
    Dispatch,
    /// A held start gate.
    StartGate,
    /// An owned ungated creation's first input.
    FirstInput,
    /// An active session making no progress, queued successor turns included.
    ActiveStall,
    /// An outstanding approval decision.
    WaitingApproval,
    /// An external gate.
    WaitingExternal,
    /// A delegated child.
    WaitingChild,
    /// A provider backoff.
    WaitingProviderRetry,
    /// Pipeline backlog.
    WaitingPipeline,
    /// A recorded scheduler fault.
    WaitingScheduler,
    /// A running recovery.
    Recovering,
    /// A blocked generation.
    Blocked,
    /// The parked re-notification interval.
    ParkedRenotify,
}

impl SessionDeadlineKind {
    /// Returns the transition this deadline's expiry fires.
    pub const fn on_expiry(&self) -> SessionDeadlineExpiry {
        match self {
            Self::Dispatch | Self::StartGate | Self::FirstInput => SessionDeadlineExpiry::Retire,
            Self::ParkedRenotify => SessionDeadlineExpiry::Renotify,
            Self::ActiveStall
            | Self::WaitingApproval
            | Self::WaitingExternal
            | Self::WaitingChild
            | Self::WaitingProviderRetry
            | Self::WaitingPipeline
            | Self::WaitingScheduler
            | Self::Recovering
            | Self::Blocked => SessionDeadlineExpiry::Park,
        }
    }

    /// Returns the deadline one non-terminal state arms.
    ///
    /// `created` arms the first-input deadline. The start gate is a core
    /// concept the command surface does not yet expose, so no creation holds
    /// one; the kind exists because the invariant admits it the day a gate can
    /// be held.
    pub const fn for_state(state: &SessionLifecycleState) -> Option<Self> {
        match state {
            SessionLifecycleState::Created => Some(Self::FirstInput),
            SessionLifecycleState::Dispatched => Some(Self::Dispatch),
            SessionLifecycleState::Active => Some(Self::ActiveStall),
            SessionLifecycleState::Waiting { wait } => Some(match wait.kind() {
                SessionWaitKind::Approval => Self::WaitingApproval,
                SessionWaitKind::External => Self::WaitingExternal,
                SessionWaitKind::Child => Self::WaitingChild,
                SessionWaitKind::ProviderRetry => Self::WaitingProviderRetry,
                SessionWaitKind::Pipeline => Self::WaitingPipeline,
                SessionWaitKind::Scheduler => Self::WaitingScheduler,
            }),
            SessionLifecycleState::Recovering { .. } => Some(Self::Recovering),
            SessionLifecycleState::Blocked { .. } => Some(Self::Blocked),
            SessionLifecycleState::Parked { .. } => Some(Self::ParkedRenotify),
            SessionLifecycleState::Terminal { .. } => None,
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
        SessionOwnershipTransition, SessionParkCause, SessionParkOwner, SessionRecoveryOperation,
        SessionRetirementCause, SessionStructuralCause, SessionTerminalOutcome, SessionWait,
        SessionWaitKind, SessionWaker, StopStickiness,
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
            owner: SessionParkOwner::Operator,
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
                cause: SessionRetirementCause::DispatchDeadlineExpired,
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
    fn an_admission_state_never_parks() {
        assert_rejects(SessionLifecycleState::Created, parked());
        assert_rejects(dispatched(), parked());
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
                owner: SessionParkOwner::Module {
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
    fn every_non_terminal_state_defines_its_deadline() {
        assert_eq!(
            SessionDeadlineKind::for_state(&SessionLifecycleState::Created),
            Some(SessionDeadlineKind::FirstInput)
        );
        assert_eq!(
            SessionDeadlineKind::for_state(&dispatched()),
            Some(SessionDeadlineKind::Dispatch)
        );
        assert_eq!(
            SessionDeadlineKind::for_state(&SessionLifecycleState::Active),
            Some(SessionDeadlineKind::ActiveStall)
        );
        assert_eq!(
            SessionDeadlineKind::for_state(&parked()),
            Some(SessionDeadlineKind::ParkedRenotify)
        );
    }

    #[test]
    fn a_terminal_state_defines_no_deadline() {
        assert_eq!(SessionDeadlineKind::for_state(&stopped()), None);
    }

    #[test]
    fn each_waiting_kind_arms_its_own_deadline() {
        assert_eq!(
            SessionDeadlineKind::for_state(&SessionLifecycleState::Waiting {
                wait: SessionWait::Approval,
            }),
            Some(SessionDeadlineKind::WaitingApproval)
        );
        assert_eq!(
            SessionDeadlineKind::for_state(&SessionLifecycleState::Waiting {
                wait: SessionWait::Child {
                    session: session_id(7),
                },
            }),
            Some(SessionDeadlineKind::WaitingChild)
        );
    }

    #[test]
    fn admission_deadlines_retire_and_post_admission_deadlines_park() {
        assert_eq!(
            SessionDeadlineKind::Dispatch.on_expiry(),
            SessionDeadlineExpiry::Retire
        );
        assert_eq!(
            SessionDeadlineKind::FirstInput.on_expiry(),
            SessionDeadlineExpiry::Retire
        );
        assert_eq!(
            SessionDeadlineKind::ActiveStall.on_expiry(),
            SessionDeadlineExpiry::Park
        );
        assert_eq!(
            SessionDeadlineKind::Recovering.on_expiry(),
            SessionDeadlineExpiry::Park
        );
        assert_eq!(
            SessionDeadlineKind::ParkedRenotify.on_expiry(),
            SessionDeadlineExpiry::Renotify
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
    fn an_achievement_and_a_stop_owe_the_goal_no_new_event() {
        assert_eq!(
            SessionTerminalOutcome::AchievedVerified.closure_outcome(),
            None
        );
        assert_eq!(
            SessionTerminalOutcome::Stopped {
                sticky: StopStickiness::Redispatchable,
            }
            .closure_outcome(),
            None
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
                cause: SessionRetirementCause::DispatchDeadlineExpired,
            }
            .closure_outcome(),
            Some(SessionClosureOutcome::Retired)
        );
    }

    #[test]
    fn supersession_forbids_further_escalation_and_abandonment_owes_cleanup() {
        assert!(
            SessionTerminalOutcome::Superseded {
                by: Some(SessionId::from_uuid(uuid::Uuid::from_u128(9))),
            }
            .forbids_further_escalation()
        );
        assert!(!SessionTerminalOutcome::Abandoned.forbids_further_escalation());
        assert!(SessionTerminalOutcome::Abandoned.records_cleanup_obligations());
        assert!(!SessionTerminalOutcome::AchievedVerified.records_cleanup_obligations());
        assert!(SessionTerminalOutcome::AchievedVerified.releases_resources());
    }
}
