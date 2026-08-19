//! Daemon-owned scheduling and model declaration for commissioned goals.

use std::{error::Error, fmt, time::Duration};

use sha2::{Digest as _, Sha256};
use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    EligibilityNudge, GoalPassDisposition, InProcessEligibilityNudge, OperatorFailureClass,
    ToolArgumentValidator, ToolDefinition, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorEvidence, ToolInputSchema,
};
use signalbox_domain::{
    AcceptedInputId, DurableCommandId, Goal, GoalBlockProvenance, GoalCommandResult, GoalEvent,
    GoalEventKind, GoalEventOrdinal, GoalModelBlockedReasonKind, GoalModelProvenance, GoalNeed,
    GoalReport, GoalSchedulerProvenance, GoalUserAction, GoalUserCommand, NormalizedToolArguments,
    SessionId, ToolEffectClass, ToolExecutionErrorDetail, ToolName, ToolPermissionDefault, TurnId,
};
use signalbox_persistence::{
    goal::{
        GoalCommandHandlingOutcome, GoalRepository, GoalRepositoryError, GoalTransitionOutcome,
    },
    goal_turn::{GoalTurnCandidates, GoalTurnContinuationOutcome},
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::HubModelConfiguration;

pub(crate) const GOAL_DECLARE_NAME: &str = "goal_declare";
const GOAL_DECLARE_DESCRIPTION: &str = "Declares the current commissioned goal achieved or blocked for the invoking session. Write the exact report or need as assistant text immediately before this final response call.";
/// Object-rooted advertisement of the internally tagged declaration.
///
/// The transition property discriminates and its description carries what a
/// root `oneOf` used to state, because a function tool's parameters must
/// describe an object and no provider is obliged to accept a root
/// combinator. `decode_goal_declaration` still refuses `achieved` with a
/// reason and `blocked` without one.
pub(crate) const GOAL_DECLARE_SCHEMA: &str = r#"{
    "additionalProperties": false,
    "properties": {
        "reason": {
            "description": "`blocked`: why the goal cannot proceed without the operator.",
            "enum": [
                "user_input_required",
                "external_change_required",
                "authorization_required"
            ],
            "type": "string"
        },
        "transition": {
            "description": "`achieved`: the commissioned goal is complete. Takes no other property. `blocked`: the commissioned goal cannot proceed. Requires `reason`.",
            "enum": ["achieved", "blocked"],
            "type": "string"
        }
    },
    "required": ["transition"],
    "type": "object"
}"#;
const GOAL_DECLARE_INVALID_ARGUMENTS: &str = "expected achieved or a model-selectable blocked reason; write the exact report or need as assistant text immediately before the final response call";
const GOAL_DECLARE_REJECTED: &str =
    "goal transition rejected for the invoking session and goal turn";
const GOAL_DECLARE_RESULT: &str = "{\"status\":\"applied\"}";
const EXECUTION_FAILURE_NEED: &str =
    "Resolve the failed goal turn's execution condition, then resume the goal.";
const EXECUTION_FAILURE_RESUMING_NEED: &str = "The goal turn failed to execute and automatic resumption is scheduled. No operator action is required unless automatic resumption is exhausted.";
/// Delay before the first automatic resumption of a failed goal turn.
///
/// Each further consecutive failure doubles this delay up to
/// [`AUTOMATIC_RESUME_BACKOFF_CAP`], so a provider or infrastructure condition
/// that clears in minutes is waited out without operator attention.
// numeric-bound: tunable - controls the first automatic goal-resume delay
const AUTOMATIC_RESUME_BASE_BACKOFF: Duration = Duration::from_secs(120);
/// Longest delay any single automatic resumption waits.
// numeric-bound: ceiling - protects goal latency from unbounded backoff growth
const AUTOMATIC_RESUME_BACKOFF_CAP: Duration = Duration::from_secs(1_800);
/// Consecutive automatic resumptions one blocked goal may spend.
///
/// Deployment configuration cannot raise any of the three constants above: an
/// automatic resumption spends provider budget on a session no operator asked
/// about, so its cadence and its end are product decisions.
// numeric-bound: ceiling - protects provider spend from an endlessly failing goal
const AUTOMATIC_RESUME_ATTEMPT_BUDGET: u32 = 5;
/// Domain separation for the derived automatic-resume command identity.
///
/// The identity must not collide with any other derived durable command, and
/// changing this value changes every derivation, which retires the automatic
/// resumptions already recorded under the old one.
const AUTOMATIC_RESUME_IDENTITY_DOMAIN: &[u8] = b"signalbox.goal.automatic-resume.v1";

/// A static `goal_declare` declaration could not be compiled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GoalDeclarationToolConstructionError {
    Name,
    Schema,
    ErrorDetail,
    Duplicate,
}

impl fmt::Display for GoalDeclarationToolConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Name => "goal_declare static name is invalid",
            Self::Schema => "goal_declare static schema is invalid",
            Self::ErrorDetail => "goal_declare static error detail is invalid",
            Self::Duplicate => "goal_declare catalog is duplicated",
        })
    }
}

impl Error for GoalDeclarationToolConstructionError {}

/// Compiled declaration and PostgreSQL executor for model goal transitions.
#[derive(Clone, Debug)]
pub(crate) struct GoalDeclarationTool {
    catalog: CompiledToolCatalog,
    executor: GoalDeclarationExecutor,
}

impl GoalDeclarationTool {
    pub(crate) fn try_new(pool: PgPool) -> Result<Self, GoalDeclarationToolConstructionError> {
        let name = ToolName::try_new(String::from(GOAL_DECLARE_NAME))
            .map_err(|_| GoalDeclarationToolConstructionError::Name)?;
        let schema = ToolInputSchema::try_new(String::from(GOAL_DECLARE_SCHEMA))
            .map_err(|_| GoalDeclarationToolConstructionError::Schema)?;
        let invalid_arguments =
            ToolExecutionErrorDetail::try_new(String::from(GOAL_DECLARE_INVALID_ARGUMENTS))
                .map_err(|_| GoalDeclarationToolConstructionError::ErrorDetail)?;
        let rejected = ToolExecutionErrorDetail::try_new(String::from(GOAL_DECLARE_REJECTED))
            .map_err(|_| GoalDeclarationToolConstructionError::ErrorDetail)?;
        let definition = ToolDefinition::new(
            name,
            String::from(GOAL_DECLARE_DESCRIPTION),
            schema,
            ToolPermissionDefault::Auto,
            ToolEffectClass::ExternalEffect,
        );
        let compiled = CompiledTool::new(
            definition,
            GoalDeclarationArgumentValidator { invalid_arguments },
        );
        let catalog = CompiledToolCatalog::try_new([compiled])
            .map_err(|_| GoalDeclarationToolConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: GoalDeclarationExecutor {
                repository: GoalRepository::new(pool),
                rejected,
            },
        })
    }

    pub(crate) fn into_parts(self) -> (CompiledToolCatalog, GoalDeclarationExecutor) {
        (self.catalog, self.executor)
    }
}

#[derive(Clone, Debug)]
struct GoalDeclarationArgumentValidator {
    invalid_arguments: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for GoalDeclarationArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_goal_declaration(arguments)
            .map(|_| ())
            .map_err(|_| self.invalid_arguments.clone())
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "transition", rename_all = "snake_case", deny_unknown_fields)]
enum GoalDeclarationArguments {
    Achieved {},
    Blocked {
        reason: GoalDeclarationBlockedReason,
    },
}

#[derive(Clone, Copy, Debug, serde::Deserialize)]
enum GoalDeclarationBlockedReason {
    #[serde(rename = "user_input_required")]
    UserInput,
    #[serde(rename = "external_change_required")]
    ExternalChange,
    #[serde(rename = "authorization_required")]
    Authorization,
}

impl From<GoalDeclarationBlockedReason> for GoalModelBlockedReasonKind {
    fn from(value: GoalDeclarationBlockedReason) -> Self {
        match value {
            GoalDeclarationBlockedReason::UserInput => Self::UserInputRequired,
            GoalDeclarationBlockedReason::ExternalChange => Self::ExternalChangeRequired,
            GoalDeclarationBlockedReason::Authorization => Self::AuthorizationRequired,
        }
    }
}

#[derive(Debug)]
enum CheckedGoalDeclaration {
    Achieved,
    Blocked { reason: GoalModelBlockedReasonKind },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InvalidGoalDeclaration;

fn decode_goal_declaration(
    arguments: &NormalizedToolArguments,
) -> Result<CheckedGoalDeclaration, InvalidGoalDeclaration> {
    let decoded: GoalDeclarationArguments =
        serde_json::from_str(arguments.as_str()).map_err(|_| InvalidGoalDeclaration)?;
    match decoded {
        GoalDeclarationArguments::Achieved {} => Ok(CheckedGoalDeclaration::Achieved),
        GoalDeclarationArguments::Blocked { reason } => Ok(CheckedGoalDeclaration::Blocked {
            reason: reason.into(),
        }),
    }
}

/// Failure before trustworthy goal-declaration evidence was available.
#[derive(Debug)]
pub(crate) enum GoalDeclarationExecutorError {
    ArgumentValidationDrift,
    Repository(GoalRepositoryError),
}

impl fmt::Display for GoalDeclarationExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("goal declaration execution failed")
    }
}

impl Error for GoalDeclarationExecutorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ArgumentValidationDrift => None,
            Self::Repository(error) => Some(error),
        }
    }
}

impl ClassifyOperatorFailure for GoalDeclarationExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::ArgumentValidationDrift => OperatorFailureClass::CallerOrHubBug,
            Self::Repository(GoalRepositoryError::Database(_)) => {
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                }
            }
            Self::Repository(GoalRepositoryError::CommitAmbiguous(_)) => {
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: true,
                }
            }
            Self::Repository(GoalRepositoryError::DifferentCommandKind { .. }) => {
                OperatorFailureClass::CallerOrHubBug
            }
            Self::Repository(GoalRepositoryError::Corruption(_)) => {
                OperatorFailureClass::FailClosedCorruption
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct GoalDeclarationExecutor {
    repository: GoalRepository,
    rejected: ToolExecutionErrorDetail,
}

impl ToolExecutor for GoalDeclarationExecutor {
    type Error = GoalDeclarationExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let declaration = decode_goal_declaration(invocation.request().arguments())
            .map_err(|_| GoalDeclarationExecutorError::ArgumentValidationDrift)?;
        let correlation = invocation.correlation();
        let provenance = GoalModelProvenance::new(correlation.turn(), correlation.request());
        let declaration_text = self
            .repository
            .load_model_declaration_text(correlation.session(), provenance)
            .await
            .map_err(GoalDeclarationExecutorError::Repository)?;
        let Some(declaration_text) = declaration_text else {
            return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                detail: Some(self.rejected.clone()),
            }));
        };
        let outcome = match declaration {
            CheckedGoalDeclaration::Achieved => {
                let Ok(report) = GoalReport::try_new(declaration_text) else {
                    return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                        detail: Some(self.rejected.clone()),
                    }));
                };
                self.repository
                    .declare_achieved(correlation.session(), report, provenance)
                    .await
            }
            CheckedGoalDeclaration::Blocked { reason } => {
                let Ok(need) = GoalNeed::try_new(declaration_text) else {
                    return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                        detail: Some(self.rejected.clone()),
                    }));
                };
                self.repository
                    .declare_blocked(correlation.session(), reason, need, provenance)
                    .await
            }
        }
        .map_err(GoalDeclarationExecutorError::Repository)?;
        let evidence = match outcome {
            GoalTransitionOutcome::Applied(_) => {
                ToolExecutorEvidence::CompletedText(String::from(GOAL_DECLARE_RESULT))
            }
            GoalTransitionOutcome::GoalNotAttached
            | GoalTransitionOutcome::Rejected(_)
            | GoalTransitionOutcome::NotCurrentGoalTurn => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.rejected.clone()),
            },
        };
        Ok(invocation.bind(evidence))
    }
}

/// PostgreSQL or static-contract failure while disposing one goal scheduler pass.
#[derive(Debug)]
pub enum PostgresGoalPassDispositionError {
    /// PostgreSQL could not read or append the goal disposition.
    Repository(GoalRepositoryError),
    /// The checked static execution-failure need no longer satisfies the domain.
    InvalidStaticNeed,
    /// The current defaults select an alias absent from daemon configuration.
    UnknownModelAlias,
    /// No successor goal-event ordinal can be represented.
    EventOrdinalExhausted,
    /// No successor accepted-input position can be represented.
    AcceptancePositionExhausted,
}

impl fmt::Display for PostgresGoalPassDispositionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Repository(error) => {
                write!(
                    formatter,
                    "goal scheduler disposition repository failure: {error}"
                )
            }
            Self::InvalidStaticNeed => formatter
                .write_str("goal scheduler disposition static execution-failure need is invalid"),
            Self::UnknownModelAlias => {
                formatter.write_str("goal continuation selected an unavailable model alias")
            }
            Self::EventOrdinalExhausted => {
                formatter.write_str("goal continuation event ordinal is exhausted")
            }
            Self::AcceptancePositionExhausted => {
                formatter.write_str("goal continuation acceptance position is exhausted")
            }
        }
    }
}

impl Error for PostgresGoalPassDispositionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Repository(error) => Some(error),
            Self::InvalidStaticNeed
            | Self::UnknownModelAlias
            | Self::EventOrdinalExhausted
            | Self::AcceptancePositionExhausted => None,
        }
    }
}

impl ClassifyOperatorFailure for PostgresGoalPassDispositionError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Repository(GoalRepositoryError::Database(_)) => {
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                }
            }
            Self::Repository(GoalRepositoryError::CommitAmbiguous(_)) => {
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: true,
                }
            }
            Self::Repository(GoalRepositoryError::DifferentCommandKind { .. })
            | Self::InvalidStaticNeed
            | Self::UnknownModelAlias
            | Self::EventOrdinalExhausted
            | Self::AcceptancePositionExhausted => OperatorFailureClass::CallerOrHubBug,
            Self::Repository(GoalRepositoryError::Corruption(_)) => {
                OperatorFailureClass::FailClosedCorruption
            }
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Repository(GoalRepositoryError::Database(_)) => "goal_disposition_database",
            Self::Repository(GoalRepositoryError::CommitAmbiguous(_)) => {
                "goal_disposition_commit_ambiguous"
            }
            Self::Repository(GoalRepositoryError::DifferentCommandKind { .. }) => {
                "goal_disposition_command_kind"
            }
            Self::Repository(GoalRepositoryError::Corruption(_)) => "goal_disposition_corruption",
            Self::InvalidStaticNeed => "goal_disposition_static_need",
            Self::UnknownModelAlias => "goal_continuation_unknown_model_alias",
            Self::EventOrdinalExhausted => "goal_continuation_event_ordinal_exhausted",
            Self::AcceptancePositionExhausted => "goal_continuation_acceptance_position_exhausted",
        }
    }
}

impl From<GoalRepositoryError> for PostgresGoalPassDispositionError {
    fn from(error: GoalRepositoryError) -> Self {
        Self::Repository(error)
    }
}

/// Production goal continuation and execution-failure adapter.
#[derive(Clone, Debug)]
pub struct PostgresGoalPassDisposition {
    repository: GoalRepository,
    model_configuration: HubModelConfiguration,
    eligibility_nudge: InProcessEligibilityNudge,
}

impl PostgresGoalPassDisposition {
    /// Binds goal persistence, static alias resolution, and scheduler hints.
    pub fn new(
        pool: PgPool,
        model_configuration: HubModelConfiguration,
        eligibility_nudge: InProcessEligibilityNudge,
    ) -> Self {
        Self {
            repository: GoalRepository::new(pool),
            model_configuration,
            eligibility_nudge,
        }
    }

    /// Reads the lineage a pending execution-failure block would extend.
    ///
    /// The plan is read before the block is appended because the appended need
    /// text states whether automatic resumption is still owed.
    async fn plan_automatic_resumption(
        &self,
        session: SessionId,
    ) -> Result<AutomaticResumption, PostgresGoalPassDispositionError> {
        let goal = self.repository.load_goal(session).await?;
        Ok(AutomaticResumption::after_spent_attempts(
            goal.as_ref().map_or(0, spent_automatic_resume_attempts),
        ))
    }

    /// Owes one delayed resume attempt to an appended execution-failure block.
    fn arm_automatic_resumption(
        &self,
        session: SessionId,
        blocked: GoalEventOrdinal,
        resumption: AutomaticResumption,
    ) {
        let AutomaticResumption::Scheduled { delay } = resumption else {
            tracing::warn!(
                session = %session.into_uuid(),
                event_ordinal = blocked.get(),
                attempt_budget = AUTOMATIC_RESUME_ATTEMPT_BUDGET,
                cause_code = "goal_automatic_resume_exhausted",
                "blocked goal exhausted automatic resumption and awaits an operator"
            );
            return;
        };
        let adapter = self.clone();
        drop(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            adapter
                .resume_after_execution_failure(session, blocked)
                .await;
        }));
    }

    /// Resumes the goal still blocked by exactly the named failure event.
    async fn resume_after_execution_failure(&self, session: SessionId, blocked: GoalEventOrdinal) {
        let reread = match self.repository.load_goal(session).await {
            Ok(goal) => goal,
            Err(error) => {
                tracing::error!(
                    session = %session.into_uuid(),
                    event_ordinal = blocked.get(),
                    cause_code = "goal_automatic_resume_reread_failed",
                    cause = %error,
                    "automatic goal resumption cannot confirm the goal is still blocked"
                );
                return;
            }
        };
        if !reread.is_some_and(|goal| awaits_automatic_resumption(&goal, blocked)) {
            return;
        }
        let command = GoalUserCommand::new(
            automatic_resume_command(session, blocked),
            session,
            GoalUserAction::Resume(None),
        );
        let candidates = GoalTurnCandidates::new(
            AcceptedInputId::from_uuid(Uuid::now_v7()),
            TurnId::from_uuid(Uuid::now_v7()),
        );
        let outcome = self
            .repository
            .handle_user_command(command, Some(candidates), |alias| {
                self.model_configuration.resolve_alias(alias)
            })
            .await;
        match outcome {
            Ok(GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(event))) => {
                let _ = self.eligibility_nudge.nudge(session);
                tracing::info!(
                    session = %session.into_uuid(),
                    event_ordinal = event.ordinal().get(),
                    blocked_event_ordinal = blocked.get(),
                    "automatically resumed a goal blocked by execution failure"
                );
            }
            Ok(GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Rejected(rejection))) => {
                tracing::info!(
                    session = %session.into_uuid(),
                    event_ordinal = blocked.get(),
                    rejection = ?rejection,
                    "automatic goal resumption was durably rejected"
                );
            }
            Ok(GoalCommandHandlingOutcome::ConflictingReuse { .. }) => {
                tracing::error!(
                    session = %session.into_uuid(),
                    event_ordinal = blocked.get(),
                    cause_code = "goal_automatic_resume_identity_conflict",
                    "the derived automatic goal-resume identity already means something else"
                );
            }
            Err(error) => {
                tracing::error!(
                    session = %session.into_uuid(),
                    event_ordinal = blocked.get(),
                    cause_code = "goal_automatic_resume_failed",
                    cause = %error,
                    "automatic goal resumption could not be recorded"
                );
            }
        }
    }
}

impl GoalPassDisposition for PostgresGoalPassDisposition {
    type Error = PostgresGoalPassDispositionError;

    fn reconcile_success(
        &self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let adapter = self.clone();
        async move {
            let resumption = adapter.plan_automatic_resumption(session).await?;
            let candidates = GoalTurnCandidates::new(
                AcceptedInputId::from_uuid(Uuid::now_v7()),
                TurnId::from_uuid(Uuid::now_v7()),
            );
            let outcome = adapter
                .repository
                .reconcile_current_after_execution(
                    session,
                    candidates,
                    resumption.need()?,
                    |alias| adapter.model_configuration.resolve_alias(alias),
                )
                .await?;
            match continuation_disposition(outcome)? {
                ContinuationDisposition::Scheduled => {
                    let _ = adapter.eligibility_nudge.nudge(session);
                }
                ContinuationDisposition::Blocked { event } => {
                    adapter.arm_automatic_resumption(session, event, resumption);
                }
                ContinuationDisposition::Undisposed => {}
            }
            Ok(())
        }
    }

    fn block_execution_failure(
        &self,
        session: SessionId,
        turn: TurnId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let adapter = self.clone();
        async move {
            let resumption = adapter.plan_automatic_resumption(session).await?;
            let outcome = adapter
                .repository
                .block_execution_failure(
                    session,
                    resumption.need()?,
                    GoalSchedulerProvenance::new(turn),
                )
                .await?;
            match outcome {
                GoalTransitionOutcome::Applied(event) => {
                    adapter.arm_automatic_resumption(session, event.ordinal(), resumption);
                }
                GoalTransitionOutcome::GoalNotAttached
                | GoalTransitionOutcome::Rejected(_)
                | GoalTransitionOutcome::NotCurrentGoalTurn => {}
            }
            Ok(())
        }
    }
}

/// What one goal-continuation outcome owes the scheduler next.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContinuationDisposition {
    /// A successor goal turn was queued and wants a scheduler hint.
    Scheduled,
    /// The named event blocked pursuit with execution-failure provenance.
    Blocked {
        /// The appended blocked event.
        event: GoalEventOrdinal,
    },
    /// Nothing was appended and nothing is owed.
    Undisposed,
}

/// Whether automatic resumption is still owed, and how long it waits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AutomaticResumption {
    /// One further attempt is owed after this delay.
    Scheduled {
        /// Backoff before the attempt.
        delay: Duration,
    },
    /// The consecutive-attempt budget is spent; only an operator can resume.
    Exhausted,
}

impl AutomaticResumption {
    fn after_spent_attempts(spent: u32) -> Self {
        if spent >= AUTOMATIC_RESUME_ATTEMPT_BUDGET {
            return Self::Exhausted;
        }
        Self::Scheduled {
            delay: AUTOMATIC_RESUME_BASE_BACKOFF
                .saturating_mul(2_u32.saturating_pow(spent))
                .min(AUTOMATIC_RESUME_BACKOFF_CAP),
        }
    }

    /// Renders the need text the next execution-failure block will carry.
    fn need(self) -> Result<GoalNeed, PostgresGoalPassDispositionError> {
        let text = match self {
            Self::Scheduled { .. } => String::from(EXECUTION_FAILURE_RESUMING_NEED),
            Self::Exhausted => format!(
                "Automatic resumption is exhausted after {AUTOMATIC_RESUME_ATTEMPT_BUDGET} consecutive execution failures. {EXECUTION_FAILURE_NEED}"
            ),
        };
        GoalNeed::try_new(text).map_err(|_| PostgresGoalPassDispositionError::InvalidStaticNeed)
    }
}

/// Counts the automatic resumptions already spent on the current block run.
///
/// The run is the trailing alternation of execution-failure blocks and the
/// automatic resumptions that answered them, so any other event — a
/// model-declared block, an operator resume, a commission, or a supersession —
/// ends it and the budget starts over.
fn spent_automatic_resume_attempts(goal: &Goal) -> u32 {
    let session = goal.session();
    let generation = goal.current().generation();
    let mut events = goal.events().iter().rev();
    let mut head = events.next();
    // The run is the same whether or not the failure that reads it has already
    // been appended, so a trailing failure block is skipped rather than counted.
    if head.is_some_and(is_execution_failure_block) {
        head = events.next();
    }
    let mut spent = 0;
    loop {
        let (Some(resumed), Some(blocked)) = (head, events.next()) else {
            return spent;
        };
        if resumed.generation() != generation || blocked.generation() != generation {
            return spent;
        }
        let GoalEventKind::Resumed { provenance, .. } = resumed.kind() else {
            return spent;
        };
        if !is_execution_failure_block(blocked)
            || provenance.command() != automatic_resume_command(session, blocked.ordinal())
        {
            return spent;
        }
        spent = spent.saturating_add(1);
        head = events.next();
    }
}

/// Whether the goal is still blocked by exactly the named failure event.
fn awaits_automatic_resumption(goal: &Goal, blocked: GoalEventOrdinal) -> bool {
    goal.events()
        .last()
        .is_some_and(|event| event.ordinal() == blocked && is_execution_failure_block(event))
}

fn is_execution_failure_block(event: &GoalEvent) -> bool {
    matches!(
        event.kind(),
        GoalEventKind::Blocked {
            block: GoalBlockProvenance::ExecutionFailure { .. },
            ..
        }
    )
}

/// Derives the durable identity one automatic resumption may ever claim.
///
/// Deriving rather than minting makes a repeated attempt an exact command
/// replay instead of a second resume, and makes the recorded resume event
/// self-identifying: a resume carrying any other identity is an operator's.
fn automatic_resume_command(session: SessionId, blocked: GoalEventOrdinal) -> DurableCommandId {
    let mut digest = Sha256::new();
    digest.update(AUTOMATIC_RESUME_IDENTITY_DOMAIN);
    digest.update(session.into_uuid().as_bytes());
    digest.update(blocked.get().to_be_bytes());
    let hash: [u8; 32] = digest.finalize().into();
    let mut identity = [0_u8; 16];
    identity
        .iter_mut()
        .zip(hash)
        .for_each(|(byte, digested)| *byte = digested);
    // RFC 9562 version 8 and variant bits: the value is a derived name, not a
    // time-ordered or random identity, and must not claim to be one.
    identity[6] = (identity[6] & 0x0f) | 0x80;
    identity[8] = (identity[8] & 0x3f) | 0x80;
    DurableCommandId::from_uuid(Uuid::from_bytes(identity))
}

fn continuation_disposition(
    outcome: GoalTurnContinuationOutcome,
) -> Result<ContinuationDisposition, PostgresGoalPassDispositionError> {
    match outcome {
        GoalTurnContinuationOutcome::Scheduled { .. } => Ok(ContinuationDisposition::Scheduled),
        GoalTurnContinuationOutcome::Blocked { event } => {
            Ok(ContinuationDisposition::Blocked { event })
        }
        GoalTurnContinuationOutcome::NotTerminal
        | GoalTurnContinuationOutcome::NotPursuing
        | GoalTurnContinuationOutcome::NotCurrentGoalTurn
        | GoalTurnContinuationOutcome::AlreadyScheduled => Ok(ContinuationDisposition::Undisposed),
        GoalTurnContinuationOutcome::UnknownModelAlias { .. } => {
            Err(PostgresGoalPassDispositionError::UnknownModelAlias)
        }
        GoalTurnContinuationOutcome::EventOrdinalExhausted => {
            Err(PostgresGoalPassDispositionError::EventOrdinalExhausted)
        }
        GoalTurnContinuationOutcome::AcceptancePositionExhausted { .. } => {
            Err(PostgresGoalPassDispositionError::AcceptancePositionExhausted)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    use signalbox_domain::{GoalStatement, GoalUserProvenance, ToolRequestId};
    use signalbox_persistence::goal::GoalCorruption;

    fn arguments(value: &str) -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(value.to_owned())
            .expect("fixture arguments are admitted")
    }

    fn fixture_session() -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(0x5e))
    }

    fn pursuing_goal() -> Goal {
        Goal::commission(
            fixture_session(),
            GoalStatement::try_new(String::from("pursue the fixture"))
                .expect("the fixture statement is admitted"),
            GoalUserProvenance::new(DurableCommandId::from_uuid(Uuid::from_u128(0xc0))),
        )
    }

    fn failed(goal: Goal, turn: u128) -> Goal {
        goal.block_execution_failure(
            GoalNeed::try_new(String::from("the fixture turn failed"))
                .expect("the fixture need is admitted"),
            GoalSchedulerProvenance::new(TurnId::from_uuid(Uuid::from_u128(turn))),
        )
        .expect("a pursuing goal blocks on execution failure")
    }

    fn automatically_resumed(goal: Goal) -> Goal {
        let blocked = goal
            .events()
            .last()
            .expect("the fixture has recorded events")
            .ordinal();
        let command = automatic_resume_command(goal.session(), blocked);
        goal.resume(None, GoalUserProvenance::new(command))
            .expect("a blocked goal resumes")
    }

    fn operator_resumed(goal: Goal) -> Goal {
        goal.resume(
            None,
            GoalUserProvenance::new(DurableCommandId::from_uuid(Uuid::from_u128(0x09e))),
        )
        .expect("a blocked goal resumes")
    }

    fn model_blocked(goal: Goal) -> Goal {
        goal.declare_blocked(
            GoalModelBlockedReasonKind::AuthorizationRequired,
            GoalNeed::try_new(String::from("grant the fixture authority"))
                .expect("the fixture need is admitted"),
            GoalModelProvenance::new(
                TurnId::from_uuid(Uuid::from_u128(0x77)),
                ToolRequestId::from_uuid(Uuid::from_u128(0x78)),
            ),
        )
        .expect("a pursuing goal blocks on a model declaration")
    }

    fn planned(goal: &Goal) -> AutomaticResumption {
        AutomaticResumption::after_spent_attempts(spent_automatic_resume_attempts(goal))
    }

    #[test]
    fn a_first_execution_failure_owes_one_attempt_after_the_base_backoff() {
        let blocked = failed(pursuing_goal(), 0x01);

        assert_eq!(spent_automatic_resume_attempts(&blocked), 0);
        assert_eq!(
            planned(&blocked),
            AutomaticResumption::Scheduled {
                delay: AUTOMATIC_RESUME_BASE_BACKOFF
            }
        );
    }

    /// The plan is read before the failure is appended and acted on after it,
    /// so both readings must name the same attempt.
    #[test]
    fn the_spent_attempt_count_is_unchanged_by_appending_the_failure_it_plans() {
        let pursuing = automatically_resumed(failed(pursuing_goal(), 0x01));
        let blocked = failed(pursuing.clone(), 0x02);

        assert_eq!(spent_automatic_resume_attempts(&pursuing), 1);
        assert_eq!(spent_automatic_resume_attempts(&blocked), 1);
    }

    #[test]
    fn consecutive_automatic_resumptions_double_the_backoff_up_to_the_cap() {
        let second = failed(automatically_resumed(failed(pursuing_goal(), 0x01)), 0x02);
        let fifth = failed(
            automatically_resumed(failed(
                automatically_resumed(failed(
                    automatically_resumed(failed(
                        automatically_resumed(failed(pursuing_goal(), 0x01)),
                        0x02,
                    )),
                    0x03,
                )),
                0x04,
            )),
            0x05,
        );

        assert_eq!(
            planned(&second),
            AutomaticResumption::Scheduled {
                delay: AUTOMATIC_RESUME_BASE_BACKOFF.saturating_mul(2)
            }
        );
        assert_eq!(spent_automatic_resume_attempts(&fifth), 4);
        assert_eq!(
            planned(&fifth),
            AutomaticResumption::Scheduled {
                delay: AUTOMATIC_RESUME_BACKOFF_CAP
            }
        );
    }

    #[test]
    fn an_exhausted_budget_blocks_permanently_and_states_the_operator_requirement() {
        let exhausted = failed(
            automatically_resumed(failed(
                automatically_resumed(failed(
                    automatically_resumed(failed(
                        automatically_resumed(failed(
                            automatically_resumed(failed(pursuing_goal(), 0x01)),
                            0x02,
                        )),
                        0x03,
                    )),
                    0x04,
                )),
                0x05,
            )),
            0x06,
        );

        assert_eq!(
            spent_automatic_resume_attempts(&exhausted),
            AUTOMATIC_RESUME_ATTEMPT_BUDGET
        );
        assert_eq!(planned(&exhausted), AutomaticResumption::Exhausted);
        assert_eq!(
            AutomaticResumption::Exhausted
                .need()
                .expect("the exhausted need is admitted")
                .as_str(),
            "Automatic resumption is exhausted after 5 consecutive execution failures. Resolve the failed goal turn's execution condition, then resume the goal."
        );
    }

    #[test]
    fn an_operator_resume_restarts_the_attempt_budget() {
        let after_operator = failed(
            operator_resumed(failed(
                automatically_resumed(failed(
                    automatically_resumed(failed(pursuing_goal(), 0x01)),
                    0x02,
                )),
                0x03,
            )),
            0x04,
        );

        assert_eq!(spent_automatic_resume_attempts(&after_operator), 0);
        assert_eq!(
            planned(&after_operator),
            AutomaticResumption::Scheduled {
                delay: AUTOMATIC_RESUME_BASE_BACKOFF
            }
        );
    }

    #[test]
    fn a_model_declared_block_ends_the_automatic_run() {
        let declared = model_blocked(automatically_resumed(failed(pursuing_goal(), 0x01)));

        assert_eq!(spent_automatic_resume_attempts(&declared), 0);
    }

    /// A pending attempt names the exact failure event it answers, so a goal
    /// that has moved on — including into a reason no automatic resumption may
    /// ever clear — is left alone.
    #[test]
    fn a_stale_attempt_is_abandoned_once_the_goal_leaves_its_failure_event() {
        let blocked = failed(pursuing_goal(), 0x01);
        let failure_event = blocked
            .events()
            .last()
            .expect("the fixture has recorded events")
            .ordinal();
        let declared = model_blocked(automatically_resumed(blocked.clone()));

        assert!(awaits_automatic_resumption(&blocked, failure_event));
        assert!(!awaits_automatic_resumption(&declared, failure_event));
    }

    #[test]
    fn the_automatic_resume_identity_is_derived_per_session_and_failure_event() {
        let other_session = SessionId::from_uuid(Uuid::from_u128(0x5f));
        let first = GoalEventOrdinal::new(NonZeroU64::MIN);
        let second = GoalEventOrdinal::new(NonZeroU64::new(2).expect("two is positive"));

        assert_eq!(
            automatic_resume_command(fixture_session(), first),
            automatic_resume_command(fixture_session(), first)
        );
        assert_ne!(
            automatic_resume_command(fixture_session(), first),
            automatic_resume_command(fixture_session(), second)
        );
        assert_ne!(
            automatic_resume_command(fixture_session(), first),
            automatic_resume_command(other_session, first)
        );
        assert_eq!(
            automatic_resume_command(fixture_session(), first)
                .into_uuid()
                .get_version_num(),
            8
        );
    }

    #[test]
    fn goal_declaration_arguments_reject_foreign_session_identity_by_construction() {
        let foreign_session = arguments(r#"{"transition":"achieved","session_id":"foreign"}"#);

        assert_eq!(
            decode_goal_declaration(&foreign_session).expect_err("session identity is not input"),
            InvalidGoalDeclaration
        );
    }

    #[test]
    fn goal_declaration_arguments_reject_scheduler_only_execution_failure_reason() {
        let scheduler_reason =
            arguments(r#"{"transition":"blocked","reason":"execution_failure"}"#);

        assert_eq!(
            decode_goal_declaration(&scheduler_reason)
                .expect_err("execution failure is not model selectable"),
            InvalidGoalDeclaration
        );
    }

    #[test]
    fn goal_declaration_arguments_admit_achievement_without_identity_or_report_text() {
        let achieved = arguments(r#"{"transition":"achieved"}"#);

        let CheckedGoalDeclaration::Achieved =
            decode_goal_declaration(&achieved).expect("achievement is admitted")
        else {
            panic!("fixture decodes as achievement");
        };
    }

    /// Rooting the advertised schema in an object widened what the *schema*
    /// permits, not what serde decodes: the argument type is unchanged, so
    /// both transitions still decode exactly as before and every combination
    /// the flat object no longer excludes is still refused here.
    #[test]
    fn goal_declaration_arguments_decode_unchanged_under_the_object_rooted_schema() {
        let blocked = arguments(r#"{"transition":"blocked","reason":"user_input_required"}"#);
        let achieved_with_reason =
            arguments(r#"{"transition":"achieved","reason":"user_input_required"}"#);
        let blocked_without_reason = arguments(r#"{"transition":"blocked"}"#);
        let untagged = arguments(r#"{"reason":"user_input_required"}"#);

        let CheckedGoalDeclaration::Blocked { reason } =
            decode_goal_declaration(&blocked).expect("a blocked reason is admitted")
        else {
            panic!("fixture decodes as a block");
        };

        assert_eq!(reason, GoalModelBlockedReasonKind::UserInputRequired);
        assert_eq!(
            decode_goal_declaration(&achieved_with_reason)
                .expect_err("achievement takes no reason"),
            InvalidGoalDeclaration
        );
        assert_eq!(
            decode_goal_declaration(&blocked_without_reason).expect_err("a block needs a reason"),
            InvalidGoalDeclaration
        );
        assert_eq!(
            decode_goal_declaration(&untagged).expect_err("the transition is mandatory"),
            InvalidGoalDeclaration
        );
    }

    /// The advertised schema states an object root carrying the discriminating
    /// transition property, never the root `oneOf` a function-tool wire
    /// rejects.
    #[test]
    fn goal_declaration_schema_is_object_rooted() {
        let schema: serde_json::Value =
            serde_json::from_str(GOAL_DECLARE_SCHEMA).expect("the static schema is JSON");

        assert_eq!(schema["type"], serde_json::json!("object"));
        assert_eq!(
            schema["properties"]["transition"]["enum"],
            serde_json::json!(["achieved", "blocked"])
        );
        assert_eq!(schema["required"], serde_json::json!(["transition"]));
        assert!(schema.get("oneOf").is_none());
    }

    #[test]
    fn goal_disposition_error_displays_distinguish_static_and_repository_failures() {
        let repository = PostgresGoalPassDispositionError::Repository(
            GoalRepositoryError::Corruption(GoalCorruption::Missing("turn")),
        );
        let invalid_static_need = PostgresGoalPassDispositionError::InvalidStaticNeed;

        assert_eq!(
            repository.to_string(),
            "goal scheduler disposition repository failure: missing goal turn"
        );
        assert_eq!(
            invalid_static_need.to_string(),
            "goal scheduler disposition static execution-failure need is invalid"
        );
    }

    #[test]
    fn terminal_continuation_boundaries_surface_closed_errors() {
        let unknown_alias =
            continuation_disposition(GoalTurnContinuationOutcome::UnknownModelAlias {
                alias: signalbox_domain::ModelAlias::from_uuid(Uuid::from_u128(0x51)),
            })
            .expect_err("an unavailable continuation alias is surfaced");
        let event_ordinal =
            continuation_disposition(GoalTurnContinuationOutcome::EventOrdinalExhausted)
                .expect_err("event ordinal exhaustion is surfaced");
        let acceptance_position =
            continuation_disposition(GoalTurnContinuationOutcome::AcceptancePositionExhausted {
                last: signalbox_domain::SessionInputPosition::first(),
            })
            .expect_err("acceptance position exhaustion is surfaced");

        assert_eq!(
            unknown_alias.operator_failure_cause_code(),
            "goal_continuation_unknown_model_alias"
        );
        assert_eq!(
            event_ordinal.operator_failure_cause_code(),
            "goal_continuation_event_ordinal_exhausted"
        );
        assert_eq!(
            acceptance_position.operator_failure_cause_code(),
            "goal_continuation_acceptance_position_exhausted"
        );
    }

    #[test]
    fn goal_repository_corruption_classifies_fail_closed_at_both_runtime_seams() {
        let declaration = GoalDeclarationExecutorError::Repository(
            GoalRepositoryError::Corruption(GoalCorruption::Missing("event")),
        );
        let disposition = PostgresGoalPassDispositionError::Repository(
            GoalRepositoryError::Corruption(GoalCorruption::Missing("turn")),
        );

        assert_eq!(
            declaration.operator_failure_class(),
            OperatorFailureClass::FailClosedCorruption
        );
        assert_eq!(
            disposition.operator_failure_class(),
            OperatorFailureClass::FailClosedCorruption
        );
    }
}
