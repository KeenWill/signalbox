//! Daemon-owned scheduling and model declaration for commissioned goals.

use std::{error::Error, fmt, future::Future, pin::Pin, sync::Arc, time::Duration};

use sha2::{Digest as _, Sha256};
use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    EligibilityNudge, GoalPassDisposition, InProcessEligibilityNudge, OperatorFailureClass,
    ToolArgumentValidator, ToolDefinition, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorEvidence, ToolInputSchema,
};
use signalbox_domain::{
    AcceptedInputId, DurableCommandId, FinishCheckVerdict, FinishCondition, Goal,
    GoalBlockProvenance, GoalCommandResult, GoalEvent, GoalEventKind, GoalEventOrdinal,
    GoalGuidance, GoalModelBlockedReasonKind, GoalModelProvenance, GoalNeed, GoalReport,
    GoalSchedulerProvenance, GoalTextError, GoalUserAction, GoalUserCommand,
    NormalizedToolArguments, SessionId, ToolEffectClass, ToolExecutionErrorDetail, ToolName,
    ToolPermissionDefault, TurnId,
};
use signalbox_persistence::{
    goal::{
        GoalCommandHandlingOutcome, GoalExecutionFailureRecoveryCause, GoalRepository,
        GoalRepositoryError, GoalTransitionOutcome,
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
/// root `oneOf` would state, because a function tool's parameters must
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
/// Need text an execution-failure block carries when its durable recovery cause
/// proves no unchanged resumption can progress.
///
/// It is the only need text that distinguishes a block planned from that cause
/// from one planned from block provenance alone, so it is exported for a
/// consumer asserting which of the two a lineage recorded.
pub const CONTEXT_COMPACTION_INPUT_DOES_NOT_FIT_NEED: &str = "No safe context-compaction boundary fits the configured model window. Start a fresh session or reduce the imported context before resuming this goal; no automatic resumption is scheduled.";
const HEADLESS_APPROVAL_ESCALATION_NEED: &str = "Approval-required work reached a headless boundary. Resolve the pending approval before resuming this goal; no automatic resumption is scheduled.";
/// Preamble for an execution-failure block automatic resumption still owes.
///
/// The repair follows it rather than replacing it with a promise of automation,
/// because every way an armed attempt can fail to resume — an exhausted budget,
/// a durably rejected command, a daemon restart, an unreachable database —
/// leaves this text as what the operator reads.
const EXECUTION_FAILURE_RESUMING_PREAMBLE: &str = "The goal turn failed to execute and automatic resumption is scheduled. If the goal is still blocked here once resumption ends, it is waiting for an operator.";
const EXECUTION_FAILURE_UNMONITORED_PREAMBLE: &str = "The goal turn failed to execute and the session is unmonitored, so no automatic resumption is scheduled.";
/// Guidance for a failure the session caused and should not repeat unchanged.
const CHARGEABLE_FAILURE_RESUME_GUIDANCE: &str = "Continue pursuing the commissioned goal. The preceding turn failed to execute. Inspect the durable session state and choose a different safe approach before repeating the failed operation.";
/// Retries one armed attempt may spend on a database that answers nothing.
///
/// These are not resumptions and do not spend the attempt budget: nothing was
/// recorded, so the goal is owed the attempt it was promised. The bound keeps a
/// database outage from holding a task open indefinitely.
// numeric-bound: guard - prevents automatic goal recovery from retrying a dead database forever
const AUTOMATIC_RESUME_INFRASTRUCTURE_RETRIES: u32 = 3;

/// Deployment policy for automatic goal resumption.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GoalModeNumericBounds {
    base_backoff: Option<Duration>,
    backoff_cap: Option<Duration>,
    attempt_budget: Option<u32>,
    attempt_ceiling: Option<u32>,
    startup_retry_delay: Option<Duration>,
}

impl GoalModeNumericBounds {
    /// Binds every automatic-resume limit to validated daemon configuration.
    pub const fn new(
        base_backoff: Option<Duration>,
        backoff_cap: Option<Duration>,
        attempt_budget: Option<u32>,
        attempt_ceiling: Option<u32>,
        startup_retry_delay: Option<Duration>,
    ) -> Self {
        Self {
            base_backoff,
            backoff_cap,
            attempt_budget,
            attempt_ceiling,
            startup_retry_delay,
        }
    }
}
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
                finish_check: Arc::new(UnverifiedFinishCheck),
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

/// Evaluates a session's finish condition against a declared achievement (§2).
pub(crate) trait FinishCheck: Send + Sync + std::fmt::Debug {
    fn check(
        &self,
        session: SessionId,
        condition: &FinishCondition,
        report: &GoalReport,
    ) -> Pin<Box<dyn Future<Output = FinishCheckVerdict> + Send + '_>>;
}

/// No verifier is wired: every declared achievement settles `achieved_declared`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct UnverifiedFinishCheck;

impl FinishCheck for UnverifiedFinishCheck {
    fn check(
        &self,
        _: SessionId,
        _: &FinishCondition,
        _: &GoalReport,
    ) -> Pin<Box<dyn Future<Output = FinishCheckVerdict> + Send + '_>> {
        Box::pin(std::future::ready(FinishCheckVerdict::Unverified))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct GoalDeclarationExecutor {
    repository: GoalRepository,
    finish_check: Arc<dyn FinishCheck>,
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
                let verdict = match self
                    .repository
                    .load_finish_condition(correlation.session())
                    .await
                    .map_err(GoalDeclarationExecutorError::Repository)?
                {
                    None => FinishCheckVerdict::Unverified,
                    Some(condition) => {
                        self.finish_check
                            .check(correlation.session(), &condition, &report)
                            .await
                    }
                };
                self.repository
                    .declare_achieved(correlation.session(), report, provenance, verdict)
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
            GoalTransitionOutcome::Applied(event) => match event.kind() {
                GoalEventKind::Blocked {
                    block: GoalBlockProvenance::FinishCheck { .. },
                    need,
                } => ToolExecutorEvidence::CompletedText(
                    serde_json::json!({ "status": "blocked", "need": need.as_str() }).to_string(),
                ),
                _ => ToolExecutorEvidence::CompletedText(String::from(GOAL_DECLARE_RESULT)),
            },
            GoalTransitionOutcome::SessionClosing => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.rejected.clone()),
            },
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
    numeric_bounds: GoalModeNumericBounds,
}

impl PostgresGoalPassDisposition {
    /// Binds goal persistence, static alias resolution, and scheduler hints.
    pub fn new(
        pool: PgPool,
        model_configuration: HubModelConfiguration,
        eligibility_nudge: InProcessEligibilityNudge,
        numeric_bounds: GoalModeNumericBounds,
    ) -> Self {
        Self {
            repository: GoalRepository::new(pool),
            model_configuration,
            eligibility_nudge,
            numeric_bounds,
        }
    }

    /// Reconciles automatic-resume timers lost with a prior daemon process.
    ///
    /// A persisted block that promises automatic resumption is treated as due
    /// on restart. Its derived command identity still binds the attempt to that
    /// exact event, so a duplicate startup attempt replays or observes that the
    /// lineage moved rather than appending a second resume. Operator-required
    /// execution-failure blocks carry different need text and are not selected.
    pub async fn reconcile_automatic_resumptions_after_restart(
        &self,
    ) -> Result<usize, PostgresGoalPassDispositionError> {
        let scheduled_need = AutomaticResumption::Scheduled {
            delay: self.numeric_bounds.base_backoff,
        }
        .need()?;
        let unmonitored_need = AutomaticResumption::Unmonitored.need()?;
        let mut remaining = AUTOMATIC_RESUME_INFRASTRUCTURE_RETRIES;
        let pending = loop {
            // An adopted session's block still names the unmonitored need.
            let scheduled = self
                .repository
                .pending_execution_failures_with_need(&scheduled_need)
                .await;
            let adopted = self
                .repository
                .pending_execution_failures_with_need(&unmonitored_need)
                .await;
            match scheduled.and_then(|scheduled| {
                adopted.map(|adopted| {
                    scheduled
                        .into_vec()
                        .into_iter()
                        .chain(adopted.into_vec())
                        .collect::<Vec<_>>()
                })
            }) {
                Ok(pending) => break pending,
                Err(error) if remaining > 0 => {
                    remaining = remaining.saturating_sub(1);
                    tracing::error!(
                        retries_remaining = remaining,
                        cause_code = "goal_automatic_resume_startup_inventory_failed",
                        cause = %error,
                        "startup could not inventory automatic goal resumptions; retrying"
                    );
                    sleep_for_policy(self.numeric_bounds.startup_retry_delay).await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        let count = pending.len();
        for candidate in pending {
            let adapter = self.clone();
            drop(tokio::spawn(async move {
                adapter
                    .resume_after_execution_failure(candidate.session(), candidate.blocked())
                    .await;
            }));
        }
        Ok(count)
    }

    /// Arms §9 resumption for the execution-failure block an adopted session
    /// holds: ownership brings the obligation an unmonitored block was not owed.
    pub fn arm_blocked_goal_resumption(&self, session: SessionId) {
        let adapter = self.clone();
        drop(tokio::spawn(async move {
            let resumption = AutomaticResumption::Scheduled {
                delay: adapter.numeric_bounds.base_backoff,
            };
            let (Ok(unmonitored_need), Ok(scheduled_need)) =
                (AutomaticResumption::Unmonitored.need(), resumption.need())
            else {
                return;
            };
            let mut remaining = AUTOMATIC_RESUME_INFRASTRUCTURE_RETRIES;
            loop {
                match adapter
                    .repository
                    .arm_owned_execution_failure(session, &unmonitored_need, &scheduled_need)
                    .await
                {
                    Ok(Some(blocked)) => {
                        adapter.arm_automatic_resumption(session, blocked, resumption);
                        return;
                    }
                    Ok(None) => return,
                    Err(error) => {
                        tracing::error!(
                            session = %session.into_uuid(),
                            retries_remaining = remaining,
                            cause_code = "goal_blocked_resume_arming_failed",
                            cause = %error,
                            "an adopted goal block could not persist its automatic resumption"
                        );
                        if remaining == 0 {
                            return;
                        }
                        remaining = remaining.saturating_sub(1);
                        sleep_for_policy(adapter.numeric_bounds.base_backoff).await;
                    }
                }
            }
        }));
    }

    /// Resumes the execution-failure block the session still holds under
    /// exactly this need.
    ///
    /// The re-read takes the session lock the block append and every ownership
    /// flip also take, so a release that commits after the need was chosen
    /// leaves the block to its operator instead of to a resume the session no
    /// longer owes.
    async fn resume_owned_execution_failure(&self, session: SessionId, need: &GoalNeed) {
        let mut remaining = AUTOMATIC_RESUME_INFRASTRUCTURE_RETRIES;
        loop {
            match self
                .repository
                .pending_owned_execution_failure_with_need(session, need)
                .await
            {
                Ok(Some(blocked)) => {
                    self.resume_after_execution_failure(session, blocked).await;
                    return;
                }
                Ok(None) => return,
                Err(error) => {
                    tracing::error!(
                        session = %session.into_uuid(),
                        retries_remaining = remaining,
                        cause_code = "goal_blocked_resume_reread_failed",
                        cause = %error,
                        "a blocked goal could not be read under the session lock"
                    );
                    if remaining == 0 {
                        return;
                    }
                    remaining = remaining.saturating_sub(1);
                    sleep_for_policy(self.numeric_bounds.base_backoff).await;
                }
            }
        }
    }

    /// Reads the lineage a pending execution-failure block would extend.
    ///
    /// The plan is read before the block is appended because the appended need
    /// text states whether automatic resumption is still owed.
    ///
    /// Durable recovery evidence for the failed turn is read first and decides
    /// alone, because a recorded cause proves an unchanged resumption cannot
    /// progress whatever the attempt accounting says. Every caller reaches the
    /// classification here: a caller that named no turn derives it from the
    /// generation's current turn, which is the turn a still-undisposed failure
    /// left terminal.
    async fn plan_automatic_resumption(
        &self,
        session: SessionId,
        failed_turn: Option<TurnId>,
    ) -> Result<AutomaticResumption, PostgresGoalPassDispositionError> {
        let goal = self.repository.load_goal(session).await?;
        let failed_turn = match (failed_turn, goal.as_ref()) {
            (Some(turn), _) => Some(turn),
            (None, Some(goal)) => {
                self.repository
                    .load_current_goal_turn(session, goal.current().generation())
                    .await?
            }
            (None, None) => None,
        };
        if let Some(turn) = failed_turn
            && let Some(cause) = self
                .repository
                .execution_failure_recovery_cause(session, turn)
                .await?
        {
            return Ok(AutomaticResumption::OperatorRequired { cause });
        }
        let Some(goal) = goal else {
            return Ok(AutomaticResumption::after_spent_attempts(
                SpentAutomaticResumeAttempts::none(),
                self.numeric_bounds,
            ));
        };
        let spent_failures = automatic_resume_failure_turns(&goal, failed_turn);
        let unchargeable_failures = self
            .repository
            .unchargeable_automatic_resume_turns(session, &spent_failures)
            .await?;
        Ok(AutomaticResumption::after_spent_attempts(
            SpentAutomaticResumeAttempts::over(&spent_failures, &unchargeable_failures),
            self.numeric_bounds,
        ))
    }

    /// A scheduled resumption is owed to an owned session only (§6).
    async fn owed_to_session(
        &self,
        session: SessionId,
        resumption: AutomaticResumption,
    ) -> Result<AutomaticResumption, PostgresGoalPassDispositionError> {
        if matches!(resumption, AutomaticResumption::Scheduled { .. })
            && !self.repository.session_owned(session).await?
        {
            return Ok(AutomaticResumption::Unmonitored);
        }
        Ok(resumption)
    }

    /// Owes one delayed resume attempt to an appended execution-failure block.
    fn arm_automatic_resumption(
        &self,
        session: SessionId,
        blocked: GoalEventOrdinal,
        resumption: AutomaticResumption,
    ) {
        let delay = match resumption {
            AutomaticResumption::Scheduled { delay } => delay,
            AutomaticResumption::Unmonitored => {
                self.arm_blocked_goal_resumption(session);
                return;
            }
            AutomaticResumption::Exhausted { .. } => {
                tracing::warn!(
                    session = %session.into_uuid(),
                    event_ordinal = blocked.get(),
                    attempt_budget = ?self.numeric_bounds.attempt_budget,
                    cause_code = "goal_automatic_resume_exhausted",
                    "blocked goal exhausted automatic resumption and awaits an operator"
                );
                return;
            }
            AutomaticResumption::CeilingReached { .. } => {
                tracing::warn!(
                    session = %session.into_uuid(),
                    event_ordinal = blocked.get(),
                    attempt_ceiling = ?self.numeric_bounds.attempt_ceiling,
                    cause_code = "goal_automatic_resume_ceiling_reached",
                    "blocked goal reached its automatic-resumption ceiling and awaits an operator"
                );
                return;
            }
            AutomaticResumption::OperatorRequired { cause } => {
                tracing::warn!(
                    session = %session.into_uuid(),
                    event_ordinal = blocked.get(),
                    cause_code = cause.code(),
                    "blocked goal has a durable non-resumable execution failure and awaits an operator"
                );
                return;
            }
        };
        let Ok(need) = resumption.need() else {
            return;
        };
        let adapter = self.clone();
        drop(tokio::spawn(async move {
            sleep_for_policy(delay).await;
            adapter.resume_owned_execution_failure(session, &need).await;
        }));
    }

    /// Resumes the goal still blocked by exactly the named failure event,
    /// retrying only while the database keeps the attempt from settling.
    ///
    /// A repository failure leaves the goal blocked with need text that expects
    /// resumption while nothing else re-reads blocked goals, so an attempt that
    /// never reached a durable answer is owed another try. Each retry reuses the
    /// derived identity, so a retry that follows a lost acknowledgement replays
    /// rather than resumes twice.
    async fn resume_after_execution_failure(&self, session: SessionId, blocked: GoalEventOrdinal) {
        let mut remaining = AUTOMATIC_RESUME_INFRASTRUCTURE_RETRIES;
        loop {
            match self.attempt_automatic_resume(session, blocked).await {
                ResumeAttempt::Settled => return,
                ResumeAttempt::OwnershipDeferred => {}
                ResumeAttempt::InfrastructureUnsettled => {
                    if remaining == 0 {
                        tracing::error!(
                            session = %session.into_uuid(),
                            event_ordinal = blocked.get(),
                            retries = AUTOMATIC_RESUME_INFRASTRUCTURE_RETRIES,
                            cause_code = "goal_automatic_resume_abandoned",
                            "automatic goal resumption abandoned a blocked goal to the operator"
                        );
                        return;
                    }
                    remaining = remaining.saturating_sub(1);
                }
            }
            sleep_for_policy(self.numeric_bounds.base_backoff).await;
        }
    }

    /// Issues one automatic resume bound to exactly the named failure event.
    async fn attempt_automatic_resume(
        &self,
        session: SessionId,
        blocked: GoalEventOrdinal,
    ) -> ResumeAttempt {
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
                return ResumeAttempt::InfrastructureUnsettled;
            }
        };
        let Some(goal) = reread else {
            return ResumeAttempt::Settled;
        };
        if !awaits_automatic_resumption(&goal, blocked) {
            return ResumeAttempt::Settled;
        }
        match self.repository.session_owned(session).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!(
                    session = %session.into_uuid(),
                    event_ordinal = blocked.get(),
                    "automatic goal resumption left a released session to its operator"
                );
                return ResumeAttempt::Settled;
            }
            Err(error) => {
                tracing::error!(
                    session = %session.into_uuid(),
                    event_ordinal = blocked.get(),
                    cause_code = "goal_automatic_resume_ownership_reread_failed",
                    cause = %error,
                    "automatic goal resumption cannot confirm the session is still owned"
                );
                return ResumeAttempt::InfrastructureUnsettled;
            }
        }
        let Some(failed_turn) = goal.events().last().and_then(execution_failure_turn) else {
            tracing::error!(
                session = %session.into_uuid(),
                event_ordinal = blocked.get(),
                cause_code = "goal_automatic_resume_failure_turn_missing",
                "automatic goal resumption could not identify its blocked turn"
            );
            return ResumeAttempt::InfrastructureUnsettled;
        };
        let unchargeable = match self
            .repository
            .unchargeable_automatic_resume_turns(session, &[failed_turn])
            .await
        {
            Ok(turns) => turns.contains(&failed_turn),
            Err(error) => {
                tracing::error!(
                    session = %session.into_uuid(),
                    turn = %failed_turn.into_uuid(),
                    event_ordinal = blocked.get(),
                    cause_code = "goal_automatic_resume_failure_classification_failed",
                    cause = %error,
                    "automatic goal resumption could not classify its failed turn"
                );
                return ResumeAttempt::InfrastructureUnsettled;
            }
        };
        let guidance = match automatic_resume_guidance(unchargeable) {
            Ok(guidance) => guidance,
            Err(error) => {
                tracing::error!(
                    session = %session.into_uuid(),
                    event_ordinal = blocked.get(),
                    cause_code = "goal_automatic_resume_guidance_invalid",
                    cause = %error,
                    "automatic goal resumption could not construct its static guidance"
                );
                return ResumeAttempt::InfrastructureUnsettled;
            }
        };
        let strategy_guidance = guidance.is_some();
        let command = GoalUserCommand::new(
            automatic_resume_command(session, blocked),
            session,
            GoalUserAction::Resume(guidance),
        );
        let candidates = GoalTurnCandidates::new(
            AcceptedInputId::from_uuid(Uuid::now_v7()),
            TurnId::from_uuid(Uuid::now_v7()),
        );
        // The reread above is a separate transaction, so the goal may move
        // between it and the session lock. Naming the expected event makes the
        // command apply to that block or to nothing: without it a lineage that
        // reached an operator-required block in that window would be resumed by
        // a command that answered a different failure.
        let outcome = self
            .repository
            .handle_expected_user_command(command, Some(candidates), blocked, |alias| {
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
                    strategy_guidance,
                    "automatically resumed a goal blocked by execution failure"
                );
                ResumeAttempt::Settled
            }
            Ok(GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Rejected(rejection))) => {
                // A durable rejection spends the derived identity, so this
                // block will never be resumed automatically. Its need text
                // already names the operator repair for exactly this case.
                tracing::error!(
                    session = %session.into_uuid(),
                    event_ordinal = blocked.get(),
                    rejection = ?rejection,
                    cause_code = "goal_automatic_resume_rejected",
                    "automatic goal resumption was durably rejected and the goal awaits an operator"
                );
                ResumeAttempt::Settled
            }
            Ok(GoalCommandHandlingOutcome::LineageMoved) => {
                tracing::info!(
                    session = %session.into_uuid(),
                    event_ordinal = blocked.get(),
                    "automatic goal resumption abandoned a goal that moved on"
                );
                ResumeAttempt::Settled
            }
            Ok(GoalCommandHandlingOutcome::TargetBusy {
                session: blocking_session,
            }) => {
                tracing::info!(
                    session = %session.into_uuid(),
                    blocking_session = %blocking_session.into_uuid(),
                    event_ordinal = blocked.get(),
                    "automatic goal resumption deferred behind another commissioned session"
                );
                ResumeAttempt::OwnershipDeferred
            }
            Ok(GoalCommandHandlingOutcome::ConflictingReuse { .. }) => {
                tracing::error!(
                    session = %session.into_uuid(),
                    event_ordinal = blocked.get(),
                    cause_code = "goal_automatic_resume_identity_conflict",
                    "the derived automatic goal-resume identity already means something else"
                );
                ResumeAttempt::Settled
            }
            Err(error) => {
                tracing::error!(
                    session = %session.into_uuid(),
                    event_ordinal = blocked.get(),
                    cause_code = "goal_automatic_resume_failed",
                    cause = %error,
                    "automatic goal resumption could not be recorded"
                );
                ResumeAttempt::InfrastructureUnsettled
            }
        }
    }

    /// Arms the execution-failure block an ambiguous commit may have written.
    ///
    /// A lost commit acknowledgement leaves the caller unable to say whether the
    /// block it was appending exists, and the appended need text expects
    /// automatic resumption either way. Reading the lineage back answers the
    /// question the acknowledgement did not, so the read is owed the same
    /// persistence a resume attempt is owed. It runs off the disposition path
    /// that raised the ambiguity, which returns its error without waiting.
    fn arm_after_ambiguous_commit(
        &self,
        session: SessionId,
        error: &PostgresGoalPassDispositionError,
    ) {
        if !commit_is_ambiguous(error) {
            return;
        }
        let adapter = self.clone();
        drop(tokio::spawn(async move {
            adapter.reconcile_ambiguous_block(session).await;
        }));
    }

    /// Arms whatever execution-failure block the lineage turns out to end at.
    ///
    /// The block's event ordinal is the one thing an ambiguous commit does not
    /// report, and the derived command identity is a function of it, so nothing
    /// can be armed without reading it back — which is why an unavailable
    /// database is retried here rather than abandoned. Arming a block some
    /// other pass already armed is harmless, because both derive the same
    /// identity and the second attempt replays.
    ///
    /// The block's own failed turn is named to the planner, so a block whose
    /// durable cause requires an operator is planned from that cause rather
    /// than from provenance alone: the trailing event's provenance says a turn
    /// failed, never that resuming it could ever progress.
    async fn reconcile_ambiguous_block(&self, session: SessionId) {
        let mut remaining = AUTOMATIC_RESUME_INFRASTRUCTURE_RETRIES;
        loop {
            match self.repository.load_goal(session).await {
                Ok(None) => return,
                Ok(Some(goal)) => {
                    let Some(event) = goal
                        .events()
                        .last()
                        .filter(|event| is_execution_failure_block(event))
                    else {
                        return;
                    };
                    let failed_turn = execution_failure_turn(event);
                    let resumption = match self
                        .plan_automatic_resumption(session, failed_turn)
                        .await
                    {
                        Ok(resumption) => resumption,
                        Err(error) => {
                            tracing::error!(
                                session = %session.into_uuid(),
                                cause_code = "goal_ambiguous_block_budget_reread_failed",
                                cause = %error,
                                "an ambiguous goal block could not plan its automatic resumption"
                            );
                            if remaining == 0 {
                                return;
                            }
                            remaining = remaining.saturating_sub(1);
                            sleep_for_policy(self.numeric_bounds.base_backoff).await;
                            continue;
                        }
                    };
                    self.arm_automatic_resumption(session, event.ordinal(), resumption);
                    return;
                }
                Err(error) => {
                    tracing::error!(
                        session = %session.into_uuid(),
                        cause_code = "goal_ambiguous_block_reread_failed",
                        cause = %error,
                        "an ambiguous goal commit cannot yet be resolved into an armed resumption"
                    );
                    if remaining == 0 {
                        tracing::error!(
                            session = %session.into_uuid(),
                            retries = AUTOMATIC_RESUME_INFRASTRUCTURE_RETRIES,
                            cause_code = "goal_ambiguous_block_abandoned",
                            "an ambiguous goal commit abandoned a possibly blocked goal to the operator"
                        );
                        return;
                    }
                    remaining = remaining.saturating_sub(1);
                    sleep_for_policy(self.numeric_bounds.base_backoff).await;
                }
            }
        }
    }
}

/// Whether a disposition failure left a commit outcome unknown.
///
/// Only an ambiguous commit may have appended the block whose need text expects
/// resumption. Every other failure either appended nothing or already said so,
/// and reading the lineage back for one would arm a block this pass never
/// wrote.
fn commit_is_ambiguous(error: &PostgresGoalPassDispositionError) -> bool {
    matches!(
        error,
        PostgresGoalPassDispositionError::Repository(GoalRepositoryError::CommitAmbiguous(_))
    )
}

/// Whether one automatic resume attempt reached a durable answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResumeAttempt {
    /// The attempt resumed, was refused, or found nothing left to answer.
    Settled,
    /// Another live target session deferred the attempt without spending a retry.
    OwnershipDeferred,
    /// Infrastructure prevented any answer, so the bounded retry is still owed.
    InfrastructureUnsettled,
}

impl GoalPassDisposition for PostgresGoalPassDisposition {
    type Error = PostgresGoalPassDispositionError;

    fn reconcile_success(
        &self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let adapter = self.clone();
        async move {
            let resumption = adapter
                .owed_to_session(
                    session,
                    adapter.plan_automatic_resumption(session, None).await?,
                )
                .await?;
            let candidates = GoalTurnCandidates::new(
                AcceptedInputId::from_uuid(Uuid::now_v7()),
                TurnId::from_uuid(Uuid::now_v7()),
            );
            let outcome = match adapter
                .repository
                .reconcile_current_after_execution(
                    session,
                    candidates,
                    resumption.need()?,
                    |alias| adapter.model_configuration.resolve_alias(alias),
                )
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    let error = PostgresGoalPassDispositionError::from(error);
                    adapter.arm_after_ambiguous_commit(session, &error);
                    return Err(error);
                }
            };
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
            let resumption = adapter
                .plan_automatic_resumption(session, Some(turn))
                .await?;
            let need = resumption.need()?;
            let unmonitored_need = if matches!(resumption, AutomaticResumption::Scheduled { .. }) {
                AutomaticResumption::Unmonitored.need()?
            } else {
                need.clone()
            };
            let outcome = match adapter
                .repository
                .block_execution_failure_for_current_ownership(
                    session,
                    need,
                    unmonitored_need,
                    GoalSchedulerProvenance::new(turn),
                )
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    let error = PostgresGoalPassDispositionError::from(error);
                    adapter.arm_after_ambiguous_commit(session, &error);
                    return Err(error);
                }
            };
            match outcome {
                GoalTransitionOutcome::Applied(event) => {
                    adapter.arm_automatic_resumption(session, event.ordinal(), resumption);
                }
                GoalTransitionOutcome::SessionClosing
                | GoalTransitionOutcome::GoalNotAttached
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
        delay: Option<Duration>,
    },
    /// The chargeable-attempt budget is spent; only an operator can resume.
    Exhausted { attempt_budget: u32 },
    /// Every attempt the lifetime ceiling admits is spent; only an operator can resume.
    CeilingReached { attempt_ceiling: u32 },
    /// Durable failure evidence proves unchanged automatic resumption cannot progress.
    OperatorRequired {
        /// Exact recorded reason the automatic path cannot make progress.
        cause: GoalExecutionFailureRecoveryCause,
    },
    /// The session is unmonitored (§6): no liveness obligation, no resumption.
    Unmonitored,
}

impl AutomaticResumption {
    /// Plans the next attempt from what the current run has already spent.
    ///
    /// The lifetime ceiling is tested first because it bounds attempts the
    /// chargeable budget deliberately never charges: a run whose every failure
    /// is exempt spends nothing, so the budget alone can never end it.
    fn after_spent_attempts(
        spent: SpentAutomaticResumeAttempts,
        numeric_bounds: GoalModeNumericBounds,
    ) -> Self {
        if let Some(attempt_ceiling) = numeric_bounds.attempt_ceiling
            && spent.total >= attempt_ceiling
        {
            return Self::CeilingReached { attempt_ceiling };
        }
        if let Some(attempt_budget) = numeric_bounds.attempt_budget
            && spent.chargeable >= attempt_budget
        {
            return Self::Exhausted { attempt_budget };
        }
        Self::Scheduled {
            delay: numeric_bounds.base_backoff.map(|base| {
                let delay = base.saturating_mul(2_u32.saturating_pow(spent.total));
                numeric_bounds
                    .backoff_cap
                    .map_or(delay, |cap| delay.min(cap))
            }),
        }
    }

    /// Renders the need text the next execution-failure block will carry.
    fn need(self) -> Result<GoalNeed, PostgresGoalPassDispositionError> {
        let text = match self {
            Self::Scheduled { .. } => {
                format!("{EXECUTION_FAILURE_RESUMING_PREAMBLE} {EXECUTION_FAILURE_NEED}")
            }
            Self::Exhausted { attempt_budget } => format!(
                "Automatic resumption is exhausted after {attempt_budget} consecutive execution failures. {EXECUTION_FAILURE_NEED}"
            ),
            Self::CeilingReached { attempt_ceiling } => format!(
                "Automatic resumption reached its ceiling of {attempt_ceiling} consecutive attempts. {EXECUTION_FAILURE_NEED}"
            ),
            Self::OperatorRequired {
                cause: GoalExecutionFailureRecoveryCause::ContextCompactionInputDoesNotFit,
            } => String::from(CONTEXT_COMPACTION_INPUT_DOES_NOT_FIT_NEED),
            Self::OperatorRequired {
                cause: GoalExecutionFailureRecoveryCause::HeadlessApprovalEscalation,
            } => String::from(HEADLESS_APPROVAL_ESCALATION_NEED),
            Self::Unmonitored => {
                format!("{EXECUTION_FAILURE_UNMONITORED_PREAMBLE} {EXECUTION_FAILURE_NEED}")
            }
        };
        GoalNeed::try_new(text).map_err(|_| PostgresGoalPassDispositionError::InvalidStaticNeed)
    }
}

async fn sleep_for_policy(delay: Option<Duration>) {
    match delay {
        Some(delay) => tokio::time::sleep(delay).await,
        None => std::future::pending().await,
    }
}

/// Returns failed turns whose automatic resumptions spend the current budget.
///
/// The run is the trailing alternation of execution-failure blocks and the
/// automatic resumptions that answered them, so any other event — a
/// model-declared block, an operator resume, a commission, or a supersession —
/// ends it and the budget starts over. Each resume is associated with the
/// failure *after* it: that is the turn the resume started and therefore the
/// durable restart evidence that may exempt the attempt. Before that failure's
/// block is appended, `current_failure` supplies the same identity.
fn automatic_resume_failure_turns(goal: &Goal, current_failure: Option<TurnId>) -> Vec<TurnId> {
    let session = goal.session();
    let generation = goal.current().generation();
    let mut events = goal.events().iter().rev();
    let mut head = events.next();
    // The run is the same whether or not the failure that reads it has already
    // been appended. Its turn is the outcome of the newest automatic resume;
    // before append the caller supplies that same current turn.
    let mut failed_turn = head.and_then(execution_failure_turn).or(current_failure);
    if head.and_then(execution_failure_turn).is_some() {
        head = events.next();
    }
    let mut failures = Vec::new();
    loop {
        let (Some(resumed), Some(blocked)) = (head, events.next()) else {
            return failures;
        };
        if resumed.generation() != generation || blocked.generation() != generation {
            return failures;
        }
        let GoalEventKind::Resumed { provenance, .. } = resumed.kind() else {
            return failures;
        };
        let Some(previous_failure) = execution_failure_turn(blocked) else {
            return failures;
        };
        if provenance.command() != automatic_resume_command(session, blocked.ordinal()) {
            return failures;
        }
        let Some(spent_failure) = failed_turn else {
            return failures;
        };
        failures.push(spent_failure);
        failed_turn = Some(previous_failure);
        head = events.next();
    }
}

/// What one execution-failure run's automatic resumptions have already spent.
///
/// Two counts, because one number cannot answer both questions the plan asks.
/// Pacing is owed to every attempt already made, spent or exempt, since each
/// one is a model call the daemon issued. The chargeable budget is owed only to
/// failures the session caused, which is what keeps a rate-limiting provider
/// from exhausting work the session did not fail. Keying both to the chargeable
/// count left a run of exempt failures at zero forever: the base delay never
/// doubled and the budget was never reached.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SpentAutomaticResumeAttempts {
    /// Attempts made, whatever their failure evidence proved.
    total: u32,
    /// Attempts whose failure the session caused.
    chargeable: u32,
}

impl SpentAutomaticResumeAttempts {
    /// The run a first execution failure starts, which has spent nothing.
    const fn none() -> Self {
        Self {
            total: 0,
            chargeable: 0,
        }
    }

    /// Counts one run's failed turns against the exempt subset of them.
    fn over(failed_turns: &[TurnId], unchargeable_turns: &[TurnId]) -> Self {
        let chargeable = failed_turns
            .iter()
            .filter(|turn| !unchargeable_turns.contains(turn))
            .count();
        Self {
            total: u32::try_from(failed_turns.len()).unwrap_or(u32::MAX),
            chargeable: u32::try_from(chargeable).unwrap_or(u32::MAX),
        }
    }
}

fn automatic_resume_guidance(unchargeable: bool) -> Result<Option<GoalGuidance>, GoalTextError> {
    if unchargeable {
        return Ok(None);
    }
    GoalGuidance::try_new(String::from(CHARGEABLE_FAILURE_RESUME_GUIDANCE)).map(Some)
}

/// Whether the goal is still blocked by exactly the named failure event.
fn awaits_automatic_resumption(goal: &Goal, blocked: GoalEventOrdinal) -> bool {
    goal.events()
        .last()
        .is_some_and(|event| event.ordinal() == blocked && is_execution_failure_block(event))
}

fn is_execution_failure_block(event: &GoalEvent) -> bool {
    execution_failure_turn(event).is_some()
}

fn execution_failure_turn(event: &GoalEvent) -> Option<TurnId> {
    match event.kind() {
        GoalEventKind::Blocked {
            block: GoalBlockProvenance::ExecutionFailure { provenance },
            ..
        } => Some(provenance.turn()),
        GoalEventKind::Commissioned { .. }
        | GoalEventKind::Resumed { .. }
        | GoalEventKind::Blocked {
            block: GoalBlockProvenance::Model { .. } | GoalBlockProvenance::FinishCheck { .. },
            ..
        }
        | GoalEventKind::Achieved { .. }
        | GoalEventKind::UserStopped { .. }
        | GoalEventKind::Superseded { .. }
        | GoalEventKind::SessionClosed { .. } => None,
    }
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

    fn spent_automatic_resume_attempts(goal: &Goal) -> u32 {
        u32::try_from(automatic_resume_failure_turns(goal, None).len()).unwrap_or(u32::MAX)
    }

    fn example_numeric_bounds() -> GoalModeNumericBounds {
        let configured = crate::configuration::checked_in_example_configuration()
            .expect("checked-in example parses");
        let bounds = configured.numeric_bounds();
        GoalModeNumericBounds::new(
            bounds.duration("automatic_resume_base_backoff").flatten(),
            bounds.duration("automatic_resume_backoff_cap").flatten(),
            bounds
                .integer("automatic_resume_attempt_budget")
                .flatten()
                .and_then(|value| u32::try_from(value).ok()),
            bounds
                .integer("automatic_resume_attempt_ceiling")
                .flatten()
                .and_then(|value| u32::try_from(value).ok()),
            bounds
                .duration("automatic_resume_startup_retry_delay")
                .flatten(),
        )
    }

    fn example_attempt_budget() -> u32 {
        example_numeric_bounds()
            .attempt_budget
            .expect("example attempt budget is bounded")
    }

    fn example_attempt_ceiling() -> u32 {
        example_numeric_bounds()
            .attempt_ceiling
            .expect("example attempt ceiling is bounded")
    }

    /// Plans the lineage's next attempt with every failure charged.
    fn planned(goal: &Goal) -> AutomaticResumption {
        let failures = automatic_resume_failure_turns(goal, None);
        AutomaticResumption::after_spent_attempts(
            SpentAutomaticResumeAttempts::over(&failures, &[]),
            example_numeric_bounds(),
        )
    }

    /// Plans the lineage's next attempt with every failure exempt.
    fn planned_with_every_failure_exempt(goal: &Goal) -> AutomaticResumption {
        let failures = automatic_resume_failure_turns(goal, None);
        AutomaticResumption::after_spent_attempts(
            SpentAutomaticResumeAttempts::over(&failures, &failures),
            example_numeric_bounds(),
        )
    }

    #[test]
    fn a_first_execution_failure_owes_one_attempt_after_the_base_backoff() {
        let blocked = failed(pursuing_goal(), 0x01);

        assert_eq!(spent_automatic_resume_attempts(&blocked), 0);
        assert_eq!(
            planned(&blocked),
            AutomaticResumption::Scheduled {
                delay: example_numeric_bounds().base_backoff
            }
        );
    }

    /// The plan is read before the failure is appended and acted on after it,
    /// so both readings must name the same attempt.
    #[test]
    fn the_spent_attempt_count_is_unchanged_by_appending_the_failure_it_plans() {
        let failure = TurnId::from_uuid(Uuid::from_u128(0x02));
        let pursuing = automatically_resumed(failed(pursuing_goal(), 0x01));
        let blocked = failed(pursuing.clone(), 0x02);

        assert_eq!(
            automatic_resume_failure_turns(&pursuing, Some(failure)).len(),
            1
        );
        assert_eq!(spent_automatic_resume_attempts(&blocked), 1);
    }

    #[test]
    fn automatic_resume_attempts_are_associated_with_the_turn_they_started() {
        let first_failure = TurnId::from_uuid(Uuid::from_u128(0x02));
        let second_failure = TurnId::from_uuid(Uuid::from_u128(0x03));
        let after_first_resume = automatically_resumed(failed(pursuing_goal(), 0x01));
        let after_second_resume = automatically_resumed(failed(after_first_resume, 0x02));

        assert_eq!(
            automatic_resume_failure_turns(&after_second_resume, Some(second_failure)),
            vec![second_failure, first_failure]
        );
    }

    #[test]
    fn failures_outside_the_session_do_not_spend_resume_attempts() {
        let runtime_failure = TurnId::from_uuid(Uuid::from_u128(0x02));
        let external_failure = TurnId::from_uuid(Uuid::from_u128(0x03));
        let failures = [external_failure, runtime_failure];

        assert_eq!(
            SpentAutomaticResumeAttempts::over(&failures, &[external_failure]).chargeable,
            1
        );
        assert_eq!(
            SpentAutomaticResumeAttempts::over(&failures, &failures).chargeable,
            0
        );
    }

    /// Every attempt is a model call the daemon issued, whatever the failure
    /// evidence later proved about who caused it, so the count that paces the
    /// next attempt counts them all.
    #[test]
    fn every_attempt_counts_toward_the_total_however_its_failure_is_classified() {
        let runtime_failure = TurnId::from_uuid(Uuid::from_u128(0x02));
        let external_failure = TurnId::from_uuid(Uuid::from_u128(0x03));
        let failures = [external_failure, runtime_failure];

        assert_eq!(
            SpentAutomaticResumeAttempts::over(&failures, &[external_failure]).total,
            2
        );
        assert_eq!(
            SpentAutomaticResumeAttempts::over(&failures, &failures).total,
            2
        );
    }

    /// The backoff is owed to every attempt already made, not only the charged
    /// ones. Keying the exponent to the chargeable count instead pinned a run
    /// of exempt failures at the base delay: a resume every base delay for as
    /// long as the exempting condition lasted.
    #[test]
    fn exempt_failures_still_double_the_backoff_by_total_attempts() {
        let second = failed(automatically_resumed(failed(pursuing_goal(), 0x01)), 0x02);

        assert_eq!(
            SpentAutomaticResumeAttempts::over(
                &automatic_resume_failure_turns(&second, None),
                &automatic_resume_failure_turns(&second, None)
            )
            .chargeable,
            0
        );
        assert_eq!(
            planned_with_every_failure_exempt(&second),
            AutomaticResumption::Scheduled {
                delay: example_numeric_bounds()
                    .base_backoff
                    .map(|base| base.saturating_mul(2))
            }
        );
    }

    /// A run whose every failure is exempt charges nothing, so the chargeable
    /// budget can never end it. The lifetime ceiling is the only limit that
    /// does, and without it such a run resumed forever, appending a goal event
    /// per cycle.
    #[test]
    fn a_run_of_exempt_failures_ends_at_the_lifetime_ceiling() {
        let ceiling = example_attempt_ceiling();
        let exempt_run = SpentAutomaticResumeAttempts::over(&[], &[]);
        let at_ceiling = SpentAutomaticResumeAttempts {
            total: ceiling,
            chargeable: 0,
        };

        assert_eq!(exempt_run.chargeable, 0);
        assert_eq!(
            AutomaticResumption::after_spent_attempts(at_ceiling, example_numeric_bounds()),
            AutomaticResumption::CeilingReached {
                attempt_ceiling: ceiling
            }
        );
        // Startup inventories blocks by the exact scheduled need text, so a
        // ceiling-parked block must not carry it or a restart would re-arm the
        // run the ceiling just ended.
        assert_ne!(
            AutomaticResumption::CeilingReached {
                attempt_ceiling: ceiling
            }
            .need()
            .expect("the ceiling need is admitted"),
            AutomaticResumption::Scheduled {
                delay: example_numeric_bounds().base_backoff
            }
            .need()
            .expect("the scheduled need is admitted")
        );
    }

    /// The two limits are separate: reaching the chargeable budget still ends a
    /// run far below the ceiling, and the ceiling ends one that never charged.
    #[test]
    fn the_chargeable_budget_and_the_lifetime_ceiling_end_a_run_independently() {
        let budget = example_attempt_budget();
        let ceiling = example_attempt_ceiling();
        let charged = SpentAutomaticResumeAttempts {
            total: budget,
            chargeable: budget,
        };
        let below_both = SpentAutomaticResumeAttempts {
            total: budget,
            chargeable: budget.saturating_sub(1),
        };

        assert!(budget < ceiling);
        assert_eq!(
            AutomaticResumption::after_spent_attempts(charged, example_numeric_bounds()),
            AutomaticResumption::Exhausted {
                attempt_budget: budget
            }
        );
        assert_eq!(
            AutomaticResumption::after_spent_attempts(below_both, example_numeric_bounds()),
            AutomaticResumption::Scheduled {
                delay: example_numeric_bounds().backoff_cap
            }
        );
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
                delay: example_numeric_bounds()
                    .base_backoff
                    .map(|base| base.saturating_mul(2))
            }
        );
        assert_eq!(spent_automatic_resume_attempts(&fifth), 4);
        assert_eq!(
            planned(&fifth),
            AutomaticResumption::Scheduled {
                delay: example_numeric_bounds().backoff_cap
            }
        );
    }

    #[test]
    fn an_exhausted_budget_blocks_permanently_and_states_the_operator_requirement() {
        let attempt_budget = example_attempt_budget();
        let spent = SpentAutomaticResumeAttempts {
            total: attempt_budget,
            chargeable: attempt_budget,
        };
        let exhausted = AutomaticResumption::Exhausted { attempt_budget };
        assert_eq!(
            AutomaticResumption::after_spent_attempts(spent, example_numeric_bounds()),
            exhausted
        );
        assert_eq!(
            exhausted
                .need()
                .expect("the exhausted need is admitted")
                .as_str(),
            format!(
                "Automatic resumption is exhausted after {attempt_budget} consecutive execution failures. {EXECUTION_FAILURE_NEED}"
            )
        );
    }

    #[test]
    fn unbounded_attempts_remain_scheduled_without_a_finite_delay() {
        let bounds = GoalModeNumericBounds::new(None, None, None, None, None);
        let spent = SpentAutomaticResumeAttempts {
            total: u32::MAX,
            chargeable: u32::MAX,
        };

        assert_eq!(
            AutomaticResumption::after_spent_attempts(spent, bounds),
            AutomaticResumption::Scheduled { delay: None }
        );
    }

    /// Only an ambiguous commit may have appended a block this pass cannot see,
    /// so only an ambiguous commit is worth reading the lineage back for.
    #[test]
    fn only_an_ambiguous_commit_asks_for_a_lineage_reread() {
        assert!(commit_is_ambiguous(
            &PostgresGoalPassDispositionError::Repository(GoalRepositoryError::CommitAmbiguous(
                sqlx::Error::PoolClosed
            ))
        ));
        assert!(!commit_is_ambiguous(
            &PostgresGoalPassDispositionError::Repository(GoalRepositoryError::Database(
                sqlx::Error::PoolClosed
            ))
        ));
        assert!(!commit_is_ambiguous(
            &PostgresGoalPassDispositionError::UnknownModelAlias
        ));
        assert!(!commit_is_ambiguous(
            &PostgresGoalPassDispositionError::InvalidStaticNeed
        ));
    }

    /// An armed attempt can fail to resume by being durably rejected, by losing
    /// the process that armed it, or by never reaching the database, and in
    /// each case the need text an operator reads is the one written before the
    /// attempt. Every one of them therefore names the repair.
    #[test]
    fn every_execution_failure_need_names_the_operator_repair() {
        let scheduled = AutomaticResumption::Scheduled {
            delay: example_numeric_bounds().base_backoff,
        }
        .need()
        .expect("the scheduled need is admitted");
        let exhausted = AutomaticResumption::Exhausted {
            attempt_budget: example_attempt_budget(),
        }
        .need()
        .expect("the exhausted need is admitted");
        let ceiling_reached = AutomaticResumption::CeilingReached {
            attempt_ceiling: example_attempt_ceiling(),
        }
        .need()
        .expect("the ceiling need is admitted");
        let operator_required = AutomaticResumption::OperatorRequired {
            cause: GoalExecutionFailureRecoveryCause::ContextCompactionInputDoesNotFit,
        }
        .need()
        .expect("the operator-required need is admitted");
        let headless_approval = AutomaticResumption::OperatorRequired {
            cause: GoalExecutionFailureRecoveryCause::HeadlessApprovalEscalation,
        }
        .need()
        .expect("the headless-approval need is admitted");

        let unmonitored = AutomaticResumption::Unmonitored
            .need()
            .expect("the unmonitored need is admitted");

        assert!(scheduled.as_str().ends_with(EXECUTION_FAILURE_NEED));
        assert!(unmonitored.as_str().ends_with(EXECUTION_FAILURE_NEED));
        assert!(exhausted.as_str().ends_with(EXECUTION_FAILURE_NEED));
        assert!(ceiling_reached.as_str().ends_with(EXECUTION_FAILURE_NEED));
        assert_eq!(
            operator_required.as_str(),
            CONTEXT_COMPACTION_INPUT_DOES_NOT_FIT_NEED
        );
        assert_eq!(
            headless_approval.as_str(),
            HEADLESS_APPROVAL_ESCALATION_NEED
        );
        assert_eq!(
            scheduled.as_str(),
            "The goal turn failed to execute and automatic resumption is scheduled. If the goal is still blocked here once resumption ends, it is waiting for an operator. Resolve the failed goal turn's execution condition, then resume the goal."
        );
    }

    #[test]
    fn a_chargeable_failure_changes_the_next_turn_input() {
        let guidance = automatic_resume_guidance(false)
            .expect("the static guidance is admitted")
            .expect("a chargeable failure carries guidance");

        assert_eq!(guidance.as_str(), CHARGEABLE_FAILURE_RESUME_GUIDANCE);
    }

    #[test]
    fn an_unchargeable_failure_reuses_the_commissioned_statement() {
        assert_eq!(
            automatic_resume_guidance(true).expect("no guidance needs admission"),
            None
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
                delay: example_numeric_bounds().base_backoff
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
