//! Immutable commissioned-goal statements and their session-scoped event history.
//!
//! docs/spec/goal-mode.md owns the lifecycle contract. Each statement
//! generation is immutable. Supersession terminalizes one generation and
//! commissions its successor in one append-only event.

use std::{error::Error, fmt, num::NonZeroU64};

use crate::{
    DurableCommandId, LifecycleActor, SessionClosureOutcome, SessionId, ToolRequestId, TurnId,
};

const MAX_GOAL_TEXT_UTF8_BYTES: usize = 1_048_576;

macro_rules! goal_text {
    ($(#[$documentation:meta])* $name:ident) => {
        $(#[$documentation])*
        #[derive(Clone, Debug, Eq, Hash, PartialEq)]
        pub struct $name(String);

        impl $name {
            /// Admits nonempty bounded UTF-8 text without rewriting it.
            pub fn try_new(value: String) -> Result<Self, GoalTextError> {
                validate_goal_text(&value)?;
                Ok(Self(value))
            }

            /// Borrows the exact admitted text.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Transfers the exact admitted text.
            pub fn into_string(self) -> String {
                self.0
            }
        }
    };
}

goal_text!(/// One immutable commissioned statement.
    GoalStatement);
goal_text!(/// A typed statement of what is needed to unblock one goal.
    GoalNeed);
goal_text!(/// Optional user guidance delivered when a blocked goal resumes.
    GoalGuidance);
goal_text!(/// The final report supplied when the model declares achievement.
    GoalReport);
goal_text!(/// A finish condition declared at creation or adoption.
    FinishConditionStatement);

fn validate_goal_text(value: &str) -> Result<(), GoalTextError> {
    if value.is_empty() {
        return Err(GoalTextError::Empty);
    }
    if value.contains('\0') {
        return Err(GoalTextError::ContainsNull);
    }
    let utf8_byte_length = value.len();
    if utf8_byte_length > MAX_GOAL_TEXT_UTF8_BYTES {
        return Err(GoalTextError::Oversized { utf8_byte_length });
    }
    Ok(())
}

/// Why one goal text value was not admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalTextError {
    /// The value is empty.
    Empty,
    /// PostgreSQL text cannot represent U+0000.
    ContainsNull,
    /// The UTF-8 representation exceeds the shared admission bound.
    Oversized {
        /// The rejected representation's UTF-8 byte count.
        utf8_byte_length: usize,
    },
}

impl fmt::Display for GoalTextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("goal text must be nonempty"),
            Self::ContainsNull => formatter.write_str("goal text must not contain U+0000"),
            Self::Oversized { utf8_byte_length } => write!(
                formatter,
                "goal text is {utf8_byte_length} UTF-8 bytes; the maximum is {MAX_GOAL_TEXT_UTF8_BYTES}"
            ),
        }
    }
}

impl Error for GoalTextError {}

/// One positive immutable-statement generation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GoalGeneration(NonZeroU64);

impl GoalGeneration {
    /// Constructs one positive generation.
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    /// Returns the numeric generation.
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    fn first() -> Self {
        Self(NonZeroU64::MIN)
    }

    fn successor(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// One positive contiguous position in a session's goal event stream.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GoalEventOrdinal(NonZeroU64);

impl GoalEventOrdinal {
    /// Constructs one positive event ordinal.
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    /// Returns the numeric ordinal.
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    fn first() -> Self {
        Self(NonZeroU64::MIN)
    }

    fn successor(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// Durable source of one goal-owned autonomous turn.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GoalTurnSource {
    /// A user event began or resumed pursuit.
    UserEvent(GoalEventOrdinal),
    /// A successfully completed goal turn continued pursuit.
    SuccessfulTurn(TurnId),
}

/// Durable user-command provenance for a goal transition.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GoalUserProvenance(DurableCommandId);

impl GoalUserProvenance {
    /// Binds a transition to its durable user command.
    pub const fn new(command: DurableCommandId) -> Self {
        Self(command)
    }

    /// Returns the durable user command.
    pub const fn command(self) -> DurableCommandId {
        self.0
    }
}

/// Trusted dispatch provenance for a model-declared transition.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GoalModelProvenance {
    turn: TurnId,
    tool_request: ToolRequestId,
}

impl GoalModelProvenance {
    /// Retains the turn and request sealed by tool dispatch.
    pub const fn new(turn: TurnId, tool_request: ToolRequestId) -> Self {
        Self { turn, tool_request }
    }

    /// Returns the invoking turn.
    pub const fn turn(self) -> TurnId {
        self.turn
    }

    /// Returns the invoking tool request.
    pub const fn tool_request(self) -> ToolRequestId {
        self.tool_request
    }

    /// Produces the exact transcript reference for an achievement report.
    pub const fn report_ref(self) -> GoalReportRef {
        GoalReportRef {
            turn: self.turn,
            tool_request: self.tool_request,
        }
    }
}

/// Scheduler provenance for execution or continuation-admission failure blocking.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GoalSchedulerProvenance(TurnId);

impl GoalSchedulerProvenance {
    /// Binds the transition to the exact source turn.
    pub const fn new(turn: TurnId) -> Self {
        Self(turn)
    }

    /// Returns the source turn.
    pub const fn turn(self) -> TurnId {
        self.0
    }
}

/// A final-report reference into the transcript's tool invocation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GoalReportRef {
    turn: TurnId,
    tool_request: ToolRequestId,
}

impl GoalReportRef {
    /// Returns the turn containing the report declaration.
    pub const fn turn(self) -> TurnId {
        self.turn
    }

    /// Returns the request immediately preceded by the report transcript part.
    pub const fn tool_request(self) -> ToolRequestId {
        self.tool_request
    }
}

/// Model-selectable reasons for declaring a goal blocked.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GoalModelBlockedReasonKind {
    /// Progress requires information or a decision from the user.
    UserInputRequired,
    /// Progress requires an external state change.
    ExternalChangeRequired,
    /// Progress requires authority the session does not hold.
    AuthorizationRequired,
}

/// Closed reason vocabulary for every blocked goal.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GoalBlockedReasonKind {
    /// Progress requires information or a decision from the user.
    UserInputRequired,
    /// Progress requires an external state change.
    ExternalChangeRequired,
    /// Progress requires authority the session does not hold.
    AuthorizationRequired,
    /// The preceding turn failed and was not silently retried.
    ExecutionFailure,
    /// The declared finish check refused an achievement; the need is its result.
    FinishCheckFailed,
}

impl From<GoalModelBlockedReasonKind> for GoalBlockedReasonKind {
    fn from(value: GoalModelBlockedReasonKind) -> Self {
        match value {
            GoalModelBlockedReasonKind::UserInputRequired => Self::UserInputRequired,
            GoalModelBlockedReasonKind::ExternalChangeRequired => Self::ExternalChangeRequired,
            GoalModelBlockedReasonKind::AuthorizationRequired => Self::AuthorizationRequired,
        }
    }
}

/// Provenance whose shape makes execution failure scheduler-only.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GoalBlockProvenance {
    /// A model declaration using one of the model-selectable reasons.
    Model {
        /// Closed model-selectable reason.
        reason: GoalModelBlockedReasonKind,
        /// Trusted tool-dispatch provenance.
        provenance: GoalModelProvenance,
    },
    /// The scheduler observed a failed turn or could not admit its successor.
    ExecutionFailure {
        /// Exact source turn.
        provenance: GoalSchedulerProvenance,
    },
    /// A failing finish check on the declaring request.
    FinishCheck { provenance: GoalModelProvenance },
}

impl GoalBlockProvenance {
    /// Returns the projected reason kind.
    pub const fn reason_kind(self) -> GoalBlockedReasonKind {
        match self {
            Self::Model {
                reason: GoalModelBlockedReasonKind::UserInputRequired,
                ..
            } => GoalBlockedReasonKind::UserInputRequired,
            Self::Model {
                reason: GoalModelBlockedReasonKind::ExternalChangeRequired,
                ..
            } => GoalBlockedReasonKind::ExternalChangeRequired,
            Self::Model {
                reason: GoalModelBlockedReasonKind::AuthorizationRequired,
                ..
            } => GoalBlockedReasonKind::AuthorizationRequired,
            Self::ExecutionFailure { .. } => GoalBlockedReasonKind::ExecutionFailure,
            Self::FinishCheck { .. } => GoalBlockedReasonKind::FinishCheckFailed,
        }
    }
}

/// One statement generation's lifecycle state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalState {
    /// The scheduler continues turns without new user input.
    Pursuing,
    /// Scheduling pauses until explicit resume or supersession.
    Blocked {
        /// Closed reason classification.
        reason: GoalBlockedReasonKind,
        /// Exact statement of what is needed.
        need: GoalNeed,
    },
    /// The model declared completion.
    Achieved {
        /// Transcript location of the final report declaration.
        report: GoalReportRef,
    },
    /// The user explicitly stopped the goal.
    UserStopped,
    /// A later immutable statement replaced this generation.
    Superseded {
        /// The successor commissioned by the same event.
        by_generation: GoalGeneration,
    },
    /// The session closed beneath this generation.
    ///
    /// Goal state is the sole continuation-stopping condition in this
    /// contract, so a terminal session settles its live generation here. The
    /// state is terminal in every direction: no resume, no supersession, and
    /// no later commission, because the session that would run them is gone.
    SessionClosed {
        /// The session outcome that closed it.
        outcome: SessionClosureOutcome,
    },
}

impl GoalState {
    fn is_pursuing(&self) -> bool {
        match self {
            Self::Pursuing => true,
            Self::Blocked { .. }
            | Self::Achieved { .. }
            | Self::UserStopped
            | Self::Superseded { .. }
            | Self::SessionClosed { .. } => false,
        }
    }

    fn is_blocked(&self) -> bool {
        match self {
            Self::Blocked { .. } => true,
            Self::Pursuing
            | Self::Achieved { .. }
            | Self::UserStopped
            | Self::Superseded { .. }
            | Self::SessionClosed { .. } => false,
        }
    }

    /// Whether this generation still admits work toward its statement.
    ///
    /// A closed generation has had its authority withdrawn or discharged, so a
    /// consumer deciding what a session is allowed to do must not read one.
    pub const fn is_open(&self) -> bool {
        match self {
            Self::Pursuing | Self::Blocked { .. } => true,
            Self::Achieved { .. }
            | Self::UserStopped
            | Self::Superseded { .. }
            | Self::SessionClosed { .. } => false,
        }
    }

    fn admits_later_commission(&self) -> bool {
        match self {
            Self::Achieved { .. } | Self::UserStopped => true,
            Self::Pursuing
            | Self::Blocked { .. }
            | Self::Superseded { .. }
            | Self::SessionClosed { .. } => false,
        }
    }
}

/// One immutable statement generation and its state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalGenerationSnapshot {
    generation: GoalGeneration,
    statement: GoalStatement,
    state: GoalState,
}

impl GoalGenerationSnapshot {
    /// Returns this generation.
    pub const fn generation(&self) -> GoalGeneration {
        self.generation
    }

    /// Borrows the immutable statement.
    pub const fn statement(&self) -> &GoalStatement {
        &self.statement
    }

    /// Borrows the lifecycle state.
    pub const fn state(&self) -> &GoalState {
        &self.state
    }
}

/// One append-only event in a session's goal lineage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalEvent {
    ordinal: GoalEventOrdinal,
    generation: GoalGeneration,
    kind: GoalEventKind,
}

impl GoalEvent {
    /// Reconstitutes one stored event for checked aggregate replay.
    pub const fn from_stored_parts(
        ordinal: GoalEventOrdinal,
        generation: GoalGeneration,
        kind: GoalEventKind,
    ) -> Self {
        Self {
            ordinal,
            generation,
            kind,
        }
    }

    /// Returns the event position.
    pub const fn ordinal(&self) -> GoalEventOrdinal {
        self.ordinal
    }

    /// Returns the generation acted on.
    pub const fn generation(&self) -> GoalGeneration {
        self.generation
    }

    /// Borrows the event payload.
    pub const fn kind(&self) -> &GoalEventKind {
        &self.kind
    }
}

/// Closed event vocabulary for a session goal lineage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalEventKind {
    /// The user attached the first immutable statement.
    Commissioned {
        /// Exact immutable statement.
        statement: GoalStatement,
        /// Durable user-command provenance.
        provenance: GoalUserProvenance,
    },
    /// Pursuit paused with a closed reason and exact need.
    Blocked {
        /// Typed reason authority and provenance.
        block: GoalBlockProvenance,
        /// Exact statement of what is needed.
        need: GoalNeed,
    },
    /// The user resumed a blocked goal.
    Resumed {
        /// Guidance delivered as the next turn's input when present.
        guidance: Option<GoalGuidance>,
        /// Durable user-command provenance.
        provenance: GoalUserProvenance,
    },
    /// The model declared achievement with its final report.
    Achieved {
        /// Exact report carried by the correlated tool request.
        report: GoalReport,
        /// Trusted tool-dispatch provenance.
        provenance: GoalModelProvenance,
    },
    /// The user ended the current goal without achievement.
    UserStopped {
        /// Durable user-command provenance.
        provenance: GoalUserProvenance,
    },
    /// The user replaced the current statement with its successor.
    Superseded {
        /// Newly commissioned immutable statement.
        replacement_statement: GoalStatement,
        /// Durable user-command provenance for both effects.
        provenance: GoalUserProvenance,
    },
    /// The session closed, settling the live generation beneath it.
    ///
    /// The session outcomes that reach this event are the ones with no
    /// existing goal spelling: a stop settles as `user_stopped` and a verified
    /// achievement as `achieved`, because those are the same act seen from the
    /// goal's side.
    SessionClosed {
        /// The outcome the session recorded.
        outcome: SessionClosureOutcome,
        /// The classified actor that closed it.
        provenance: LifecycleActor,
    },
}

/// A session's complete goal lineage and current generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Goal {
    session: SessionId,
    generations: Vec<GoalGenerationSnapshot>,
    events: Vec<GoalEvent>,
}

impl Goal {
    /// Commissions the first immutable statement.
    pub fn commission(
        session: SessionId,
        statement: GoalStatement,
        provenance: GoalUserProvenance,
    ) -> Self {
        let generation = GoalGeneration::first();
        Self {
            session,
            generations: vec![GoalGenerationSnapshot {
                generation,
                statement: statement.clone(),
                state: GoalState::Pursuing,
            }],
            events: vec![GoalEvent {
                ordinal: GoalEventOrdinal::first(),
                generation,
                kind: GoalEventKind::Commissioned {
                    statement,
                    provenance,
                },
            }],
        }
    }

    /// Commissions a new statement after an achieved or user-stopped generation.
    pub fn commission_successor(
        mut self,
        statement: GoalStatement,
        provenance: GoalUserProvenance,
    ) -> Result<Self, GoalTransitionError> {
        if !self.current().state.admits_later_commission() {
            return Err(GoalTransitionError::new(
                self,
                GoalTransitionFailure::RequiresNoActiveGoal,
            ));
        }
        let ordinal = self.next_ordinal()?;
        let generation = self.current().generation.successor().ok_or_else(|| {
            GoalTransitionError::new(self.clone(), GoalTransitionFailure::GenerationExhausted)
        })?;
        self.generations.push(GoalGenerationSnapshot {
            generation,
            statement: statement.clone(),
            state: GoalState::Pursuing,
        });
        self.events.push(GoalEvent {
            ordinal,
            generation,
            kind: GoalEventKind::Commissioned {
                statement,
                provenance,
            },
        });
        Ok(self)
    }

    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Borrows every immutable generation in lineage order.
    pub fn generations(&self) -> &[GoalGenerationSnapshot] {
        &self.generations
    }

    /// Borrows the current generation.
    pub fn current(&self) -> &GoalGenerationSnapshot {
        &self.generations[self.generations.len() - 1]
    }

    /// Borrows the complete event history.
    pub fn events(&self) -> &[GoalEvent] {
        &self.events
    }

    /// Applies a model-declared blocked transition.
    pub fn declare_blocked(
        self,
        reason: GoalModelBlockedReasonKind,
        need: GoalNeed,
        provenance: GoalModelProvenance,
    ) -> Result<Self, GoalTransitionError> {
        self.block(GoalBlockProvenance::Model { reason, provenance }, need)
    }

    /// Blocks after an exact scheduler disposition failure without retrying it.
    pub fn block_execution_failure(
        self,
        need: GoalNeed,
        provenance: GoalSchedulerProvenance,
    ) -> Result<Self, GoalTransitionError> {
        self.block(GoalBlockProvenance::ExecutionFailure { provenance }, need)
    }

    /// Blocks the goal on a failing finish check; the need is the check's result.
    pub fn block_finish_check(
        self,
        need: GoalNeed,
        provenance: GoalModelProvenance,
    ) -> Result<Self, GoalTransitionError> {
        self.block(GoalBlockProvenance::FinishCheck { provenance }, need)
    }

    fn block(
        mut self,
        block: GoalBlockProvenance,
        need: GoalNeed,
    ) -> Result<Self, GoalTransitionError> {
        if !self.current().state.is_pursuing() {
            return Err(GoalTransitionError::new(
                self,
                GoalTransitionFailure::RequiresPursuing,
            ));
        }
        let ordinal = self.next_ordinal()?;
        let generation = self.current().generation;
        self.current_mut().state = GoalState::Blocked {
            reason: block.reason_kind(),
            need: need.clone(),
        };
        self.events.push(GoalEvent {
            ordinal,
            generation,
            kind: GoalEventKind::Blocked { block, need },
        });
        Ok(self)
    }

    /// Resumes a blocked goal with optional next-turn guidance.
    pub fn resume(
        mut self,
        guidance: Option<GoalGuidance>,
        provenance: GoalUserProvenance,
    ) -> Result<Self, GoalTransitionError> {
        if !self.current().state.is_blocked() {
            return Err(GoalTransitionError::new(
                self,
                GoalTransitionFailure::RequiresBlocked,
            ));
        }
        let ordinal = self.next_ordinal()?;
        let generation = self.current().generation;
        self.current_mut().state = GoalState::Pursuing;
        self.events.push(GoalEvent {
            ordinal,
            generation,
            kind: GoalEventKind::Resumed {
                guidance,
                provenance,
            },
        });
        Ok(self)
    }

    /// Declares achievement with a report in the invoking tool request.
    pub fn declare_achieved(
        mut self,
        report: GoalReport,
        provenance: GoalModelProvenance,
    ) -> Result<Self, GoalTransitionError> {
        if !self.current().state.is_pursuing() {
            return Err(GoalTransitionError::new(
                self,
                GoalTransitionFailure::RequiresPursuing,
            ));
        }
        let ordinal = self.next_ordinal()?;
        let generation = self.current().generation;
        self.current_mut().state = GoalState::Achieved {
            report: provenance.report_ref(),
        };
        self.events.push(GoalEvent {
            ordinal,
            generation,
            kind: GoalEventKind::Achieved { report, provenance },
        });
        Ok(self)
    }

    /// Stops a pursuing or blocked goal by explicit user action.
    pub fn stop(mut self, provenance: GoalUserProvenance) -> Result<Self, GoalTransitionError> {
        if !self.current().state.is_open() {
            return Err(GoalTransitionError::new(
                self,
                GoalTransitionFailure::RequiresPursuingOrBlocked,
            ));
        }
        let ordinal = self.next_ordinal()?;
        let generation = self.current().generation;
        self.current_mut().state = GoalState::UserStopped;
        self.events.push(GoalEvent {
            ordinal,
            generation,
            kind: GoalEventKind::UserStopped { provenance },
        });
        Ok(self)
    }

    /// Settles an open generation because its session reached a terminal
    /// outcome.
    ///
    /// Rejected for a closed generation: an achieved or stopped generation is
    /// already settled, and settling it again would record a second terminal
    /// event for one lineage.
    pub fn close_with_session(
        mut self,
        outcome: SessionClosureOutcome,
        provenance: LifecycleActor,
    ) -> Result<Self, GoalTransitionError> {
        if !self.current().state.is_open() {
            return Err(GoalTransitionError::new(
                self,
                GoalTransitionFailure::RequiresPursuingOrBlocked,
            ));
        }
        let ordinal = self.next_ordinal()?;
        let generation = self.current().generation;
        self.current_mut().state = GoalState::SessionClosed { outcome };
        self.events.push(GoalEvent {
            ordinal,
            generation,
            kind: GoalEventKind::SessionClosed {
                outcome,
                provenance,
            },
        });
        Ok(self)
    }

    /// Atomically supersedes an open generation and commissions its successor.
    pub fn supersede(
        mut self,
        replacement_statement: GoalStatement,
        provenance: GoalUserProvenance,
    ) -> Result<Self, GoalTransitionError> {
        if !self.current().state.is_open() {
            return Err(GoalTransitionError::new(
                self,
                GoalTransitionFailure::RequiresPursuingOrBlocked,
            ));
        }
        let ordinal = self.next_ordinal()?;
        let replaced_generation = self.current().generation;
        let replacement_generation = replaced_generation.successor().ok_or_else(|| {
            GoalTransitionError::new(self.clone(), GoalTransitionFailure::GenerationExhausted)
        })?;
        self.current_mut().state = GoalState::Superseded {
            by_generation: replacement_generation,
        };
        self.generations.push(GoalGenerationSnapshot {
            generation: replacement_generation,
            statement: replacement_statement.clone(),
            state: GoalState::Pursuing,
        });
        self.events.push(GoalEvent {
            ordinal,
            generation: replaced_generation,
            kind: GoalEventKind::Superseded {
                replacement_statement,
                provenance,
            },
        });
        Ok(self)
    }

    fn current_mut(&mut self) -> &mut GoalGenerationSnapshot {
        let last = self.generations.len() - 1;
        &mut self.generations[last]
    }

    fn next_ordinal(&self) -> Result<GoalEventOrdinal, GoalTransitionError> {
        self.events[self.events.len() - 1]
            .ordinal
            .successor()
            .ok_or_else(|| {
                GoalTransitionError::new(self.clone(), GoalTransitionFailure::EventOrdinalExhausted)
            })
    }
}

/// Why a goal transition was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalTransitionFailure {
    /// The transition requires a pursuing goal.
    RequiresPursuing,
    /// The transition requires a blocked goal.
    RequiresBlocked,
    /// The transition requires a pursuing or blocked goal.
    RequiresPursuingOrBlocked,
    /// A later commission requires no pursuing or blocked goal.
    RequiresNoActiveGoal,
    /// No successor statement generation can be represented.
    GenerationExhausted,
    /// No successor event ordinal can be represented.
    EventOrdinalExhausted,
}

/// A rejected transition retaining the unchanged aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalTransitionError {
    goal: Goal,
    failure: GoalTransitionFailure,
}

impl GoalTransitionError {
    fn new(goal: Goal, failure: GoalTransitionFailure) -> Self {
        Self { goal, failure }
    }

    /// Returns the closed rejection reason.
    pub const fn failure(&self) -> GoalTransitionFailure {
        self.failure
    }

    /// Borrows the unchanged goal.
    pub const fn goal(&self) -> &Goal {
        &self.goal
    }

    /// Returns the unchanged goal.
    pub fn into_goal(self) -> Goal {
        self.goal
    }
}

impl fmt::Display for GoalTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "goal transition rejected: {:?}", self.failure)
    }
}

impl Error for GoalTransitionError {}

/// Complete durable facts supplied to goal reconstitution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalReconstitutionInput {
    session: SessionId,
    events: Vec<GoalEvent>,
}

impl GoalReconstitutionInput {
    /// Retains the requested session and ordered event inventory.
    pub fn new(session: SessionId, events: Vec<GoalEvent>) -> Self {
        Self { session, events }
    }

    /// Replays the event history through domain transition rules.
    pub fn reconstitute(self) -> Result<Goal, GoalReconstitutionError> {
        let mut events = self.events.into_iter();
        let first = events.next().ok_or_else(|| {
            GoalReconstitutionError::new(GoalReconstitutionFailure::MissingCommission)
        })?;
        let (statement, provenance) = match first.kind {
            GoalEventKind::Commissioned {
                statement,
                provenance,
            } => (statement, provenance),
            GoalEventKind::Blocked { .. }
            | GoalEventKind::Resumed { .. }
            | GoalEventKind::Achieved { .. }
            | GoalEventKind::UserStopped { .. }
            | GoalEventKind::Superseded { .. }
            | GoalEventKind::SessionClosed { .. } => {
                return Err(GoalReconstitutionError::new(
                    GoalReconstitutionFailure::MissingCommission,
                ));
            }
        };
        if first.ordinal != GoalEventOrdinal::first() || first.generation != GoalGeneration::first()
        {
            return Err(GoalReconstitutionError::new(
                GoalReconstitutionFailure::EventSequence,
            ));
        }
        let mut goal = Goal::commission(self.session, statement, provenance);
        for stored in events {
            let transitioned = apply_stored_event(goal, &stored).map_err(|_| {
                GoalReconstitutionError::new(GoalReconstitutionFailure::InvalidTransition)
            })?;
            if transitioned.events.last() != Some(&stored) {
                return Err(GoalReconstitutionError::new(
                    GoalReconstitutionFailure::EventSequence,
                ));
            }
            goal = transitioned;
        }
        Ok(goal)
    }
}

fn apply_stored_event(goal: Goal, event: &GoalEvent) -> Result<Goal, GoalTransitionError> {
    match &event.kind {
        GoalEventKind::Commissioned {
            statement,
            provenance,
        } => goal.commission_successor(statement.clone(), *provenance),
        GoalEventKind::Blocked {
            block: GoalBlockProvenance::Model { reason, provenance },
            need,
        } => goal.declare_blocked(*reason, need.clone(), *provenance),
        GoalEventKind::Blocked {
            block: GoalBlockProvenance::ExecutionFailure { provenance },
            need,
        } => goal.block_execution_failure(need.clone(), *provenance),
        GoalEventKind::Blocked {
            block: GoalBlockProvenance::FinishCheck { provenance },
            need,
        } => goal.block_finish_check(need.clone(), *provenance),
        GoalEventKind::Resumed {
            guidance,
            provenance,
        } => goal.resume(guidance.clone(), *provenance),
        GoalEventKind::Achieved { report, provenance } => {
            goal.declare_achieved(report.clone(), *provenance)
        }
        GoalEventKind::UserStopped { provenance } => goal.stop(*provenance),
        GoalEventKind::Superseded {
            replacement_statement,
            provenance,
        } => goal.supersede(replacement_statement.clone(), *provenance),
        GoalEventKind::SessionClosed {
            outcome,
            provenance,
        } => goal.close_with_session(*outcome, *provenance),
    }
}

/// Why durable goal events could not reconstruct one aggregate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalReconstitutionFailure {
    /// No valid first commission event exists.
    MissingCommission,
    /// An ordinal, generation, or payload is not the exact next event.
    EventSequence,
    /// A stored transition is not admitted from the preceding state.
    InvalidTransition,
}

/// Fail-closed goal history reconstitution error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GoalReconstitutionError(GoalReconstitutionFailure);

impl GoalReconstitutionError {
    fn new(failure: GoalReconstitutionFailure) -> Self {
        Self(failure)
    }

    /// Returns the closed reconstitution failure.
    pub const fn failure(self) -> GoalReconstitutionFailure {
        self.0
    }
}

impl fmt::Display for GoalReconstitutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "goal reconstitution failed: {:?}", self.0)
    }
}

impl Error for GoalReconstitutionError {}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    const SESSION: u128 = 1;
    const FIRST_COMMAND: u128 = 2;
    const SECOND_COMMAND: u128 = 3;
    const INVOKING_TURN: u128 = 4;
    const INVOKING_TOOL_REQUEST: u128 = 5;

    fn session() -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(SESSION))
    }

    fn user(command: u128) -> GoalUserProvenance {
        GoalUserProvenance::new(DurableCommandId::from_uuid(Uuid::from_u128(command)))
    }

    fn model() -> GoalModelProvenance {
        GoalModelProvenance::new(
            TurnId::from_uuid(Uuid::from_u128(INVOKING_TURN)),
            ToolRequestId::from_uuid(Uuid::from_u128(INVOKING_TOOL_REQUEST)),
        )
    }

    fn statement(value: &str) -> GoalStatement {
        GoalStatement::try_new(String::from(value)).expect("fixture statement is admitted")
    }

    fn need(value: &str) -> GoalNeed {
        GoalNeed::try_new(String::from(value)).expect("fixture need is admitted")
    }

    fn report(value: &str) -> GoalReport {
        GoalReport::try_new(String::from(value)).expect("fixture report is admitted")
    }

    #[test]
    fn scheduler_block_reports_event_ordinal_exhaustion() {
        let mut goal = Goal::commission(
            session(),
            statement("exhaust the event ordinal"),
            user(FIRST_COMMAND),
        );
        goal.events[0].ordinal = GoalEventOrdinal::new(NonZeroU64::MAX);
        let error = goal
            .block_execution_failure(
                need("repair execution"),
                GoalSchedulerProvenance::new(TurnId::from_uuid(Uuid::from_u128(INVOKING_TURN))),
            )
            .expect_err("the maximum event ordinal has no successor");

        assert_eq!(
            error.failure(),
            GoalTransitionFailure::EventOrdinalExhausted
        );
    }

    #[test]
    fn goal_text_rejects_postgresqls_unrepresentable_null_scalar() {
        assert_eq!(
            GoalStatement::try_new(String::from("scope\0change")),
            Err(GoalTextError::ContainsNull)
        );
    }

    /// supersession preserves immutable lineage while commissioning one successor.
    #[test]
    fn supersession_preserves_old_statement_and_commissions_successor() {
        let first_statement = statement("ship the first scope");
        let replacement_statement = statement("ship the replacement scope");
        let goal = Goal::commission(session(), first_statement.clone(), user(FIRST_COMMAND));

        let superseded = goal
            .supersede(replacement_statement.clone(), user(SECOND_COMMAND))
            .expect("a pursuing goal can be superseded");

        assert_eq!(superseded.generations()[0].statement(), &first_statement);
        assert_eq!(
            superseded.generations()[0].state(),
            &GoalState::Superseded {
                by_generation: GoalGeneration::new(NonZeroU64::new(2).expect("two is positive")),
            }
        );
        assert_eq!(superseded.current().statement(), &replacement_statement);
        assert_eq!(superseded.current().state(), &GoalState::Pursuing);
    }

    /// execution failure is a scheduler-only block and never a retry.
    #[test]
    fn execution_failure_blocks_without_model_selectable_provenance() {
        let failed_turn = TurnId::from_uuid(Uuid::from_u128(INVOKING_TURN));
        let required = need("resume after repairing execution");
        let goal = Goal::commission(session(), statement("finish the task"), user(FIRST_COMMAND));

        let blocked = goal
            .block_execution_failure(required.clone(), GoalSchedulerProvenance::new(failed_turn))
            .expect("a pursuing goal blocks after a failed turn");

        assert_eq!(
            blocked.current().state(),
            &GoalState::Blocked {
                reason: GoalBlockedReasonKind::ExecutionFailure,
                need: required,
            }
        );
    }

    /// achievement points at the exact correlated report declaration.
    #[test]
    fn achieved_state_references_exact_report_tool_invocation() {
        let provenance = model();
        let final_report = report("the commissioned result is complete");
        let goal = Goal::commission(session(), statement("finish the task"), user(FIRST_COMMAND));

        let achieved = goal
            .declare_achieved(final_report.clone(), provenance)
            .expect("a pursuing goal can be achieved");

        assert_eq!(
            achieved.current().state(),
            &GoalState::Achieved {
                report: provenance.report_ref(),
            }
        );
        assert_eq!(
            achieved.events().last().map(GoalEvent::kind),
            Some(&GoalEventKind::Achieved {
                report: final_report,
                provenance,
            })
        );
    }

    /// pursuing is the sole scheduler-continuing state; every other
    /// lifecycle state stops that generation.
    #[test]
    fn only_pursuing_state_continues_scheduler_turns() {
        let blocked = GoalState::Blocked {
            reason: GoalBlockedReasonKind::AuthorizationRequired,
            need: need("authorization for the deployment"),
        };
        let achieved = GoalState::Achieved {
            report: model().report_ref(),
        };
        let user_stopped = GoalState::UserStopped;
        let superseded = GoalState::Superseded {
            by_generation: GoalGeneration::new(
                NonZeroU64::new(2).expect("fixture successor generation is positive"),
            ),
        };

        assert!(GoalState::Pursuing.is_pursuing());
        assert!(!blocked.is_pursuing());
        assert!(!achieved.is_pursuing());
        assert!(!user_stopped.is_pursuing());
        assert!(!superseded.is_pursuing());
    }

    /// the complete event history replays to the identical aggregate.
    #[test]
    fn event_history_round_trips_through_checked_reconstitution() {
        let guidance = GoalGuidance::try_new(String::from("use the newly supplied key"))
            .expect("fixture guidance is admitted");
        let commissioned =
            Goal::commission(session(), statement("finish the task"), user(FIRST_COMMAND));
        let blocked = commissioned
            .declare_blocked(
                GoalModelBlockedReasonKind::AuthorizationRequired,
                need("authorization for the deployment"),
                model(),
            )
            .expect("pursuit can block");
        let resumed = blocked
            .resume(Some(guidance), user(SECOND_COMMAND))
            .expect("blocked goal can resume");
        let stored_events = resumed.events().to_vec();

        let reconstituted = GoalReconstitutionInput::new(session(), stored_events)
            .reconstitute()
            .expect("complete history reconstitutes");

        assert_eq!(reconstituted, resumed);
    }
}
