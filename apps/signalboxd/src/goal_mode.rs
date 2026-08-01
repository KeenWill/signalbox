//! Daemon-owned scheduling and model declaration for commissioned goals.

use std::{error::Error, fmt};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    EligibilityNudge, GoalPassDisposition, InProcessEligibilityNudge, OperatorFailureClass,
    ToolArgumentValidator, ToolDefinition, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorEvidence, ToolInputSchema,
};
use signalbox_domain::{
    AcceptedInputId, GoalModelBlockedReasonKind, GoalModelProvenance, GoalNeed, GoalReport,
    GoalSchedulerProvenance, NormalizedToolArguments, SessionId, ToolEffectClass,
    ToolExecutionErrorDetail, ToolName, ToolPermissionDefault, TurnId,
};
use signalbox_persistence::{
    goal::{GoalRepository, GoalRepositoryError, GoalTransitionOutcome},
    goal_turn::{GoalTurnCandidates, GoalTurnContinuationOutcome},
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::HubModelConfiguration;

pub(crate) const GOAL_DECLARE_NAME: &str = "goal_declare";
const GOAL_DECLARE_DESCRIPTION: &str = "Declares the current commissioned goal achieved or blocked for the invoking session. Write the exact report or need as assistant text immediately before this call.";
const GOAL_DECLARE_SCHEMA: &str = r#"{
    "oneOf": [
        {
            "additionalProperties": false,
            "properties": {
                "transition": {"const": "achieved"}
            },
            "required": ["transition"],
            "type": "object"
        },
        {
            "additionalProperties": false,
            "properties": {
                "reason": {
                    "enum": [
                        "user_input_required",
                        "external_change_required",
                        "authorization_required"
                    ]
                },
                "transition": {"const": "blocked"}
            },
            "required": ["transition", "reason"],
            "type": "object"
        }
    ],
    "type": "object"
}"#;
const GOAL_DECLARE_INVALID_ARGUMENTS: &str = "expected achieved or a model-selectable blocked reason; write the exact report or need as assistant text immediately before the call";
const GOAL_DECLARE_REJECTED: &str =
    "goal transition rejected for the invoking session and goal turn";
const GOAL_DECLARE_RESULT: &str = "{\"status\":\"applied\"}";
const EXECUTION_FAILURE_NEED: &str =
    "Resolve the failed goal turn's execution condition, then resume the goal.";

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
        }
    }
}

impl Error for PostgresGoalPassDispositionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Repository(error) => Some(error),
            Self::InvalidStaticNeed => None,
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
            | Self::InvalidStaticNeed => OperatorFailureClass::CallerOrHubBug,
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
}

impl GoalPassDisposition for PostgresGoalPassDisposition {
    type Error = PostgresGoalPassDispositionError;

    fn reconcile_success(
        &self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let adapter = self.clone();
        async move {
            let need = execution_failure_need()?;
            let candidates = GoalTurnCandidates::new(
                AcceptedInputId::from_uuid(Uuid::now_v7()),
                TurnId::from_uuid(Uuid::now_v7()),
            );
            let outcome = adapter
                .repository
                .reconcile_current_after_execution(session, candidates, need, |alias| {
                    adapter.model_configuration.resolve_alias(alias)
                })
                .await?;
            match outcome {
                GoalTurnContinuationOutcome::Scheduled { .. } => {
                    let _ = adapter.eligibility_nudge.nudge(session);
                }
                GoalTurnContinuationOutcome::NotTerminal
                | GoalTurnContinuationOutcome::Blocked { .. }
                | GoalTurnContinuationOutcome::NotPursuing
                | GoalTurnContinuationOutcome::AlreadyScheduled => {}
            }
            Ok(())
        }
    }

    fn block_execution_failure(
        &self,
        session: SessionId,
        turn: TurnId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let repository = self.repository.clone();
        async move {
            let outcome = repository
                .block_execution_failure(
                    session,
                    execution_failure_need()?,
                    GoalSchedulerProvenance::new(turn),
                )
                .await?;
            match outcome {
                GoalTransitionOutcome::Applied(_)
                | GoalTransitionOutcome::GoalNotAttached
                | GoalTransitionOutcome::Rejected(_)
                | GoalTransitionOutcome::NotCurrentGoalTurn => Ok(()),
            }
        }
    }
}

fn execution_failure_need() -> Result<GoalNeed, PostgresGoalPassDispositionError> {
    GoalNeed::try_new(String::from(EXECUTION_FAILURE_NEED))
        .map_err(|_| PostgresGoalPassDispositionError::InvalidStaticNeed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_persistence::goal::GoalCorruption;

    fn arguments(value: &str) -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(value.to_owned())
            .expect("fixture arguments are admitted")
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
