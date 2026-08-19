#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{error::Error, num::NonZeroU64, time::Duration};

use signalbox_application::{
    ApprovalJudgeCompletionIdentities, ApprovalJudgeDispatchAuthority,
    ApprovalJudgePullRequestAuthority, AuthorizeModelCallOutcome, ModelCallCredentialReference,
    RepoWatchBranchHead, RepoWatchDispatchService, RepoWatchDispatchTransaction,
    RepoWatchEventContentIdentityV1, RepoWatchEventOccurrenceV1, RepoWatchObservation,
    RepoWatchPullRequestLifecycle, RepoWatchPullRequestState, RepoWatchPullRequestStateInput,
    RepoWatchRepositoryState, RepoWatchRepositoryStateInput, RepoWatchResolvedTemplate,
    RepoWatchRuleEvaluation, RepoWatchRuleEvaluationOutcome, RepoWatchTemplateResolver,
    RepoWatchWorkflowRunObservation, StartEligibleTurnOutcome, StartEligibleTurnService,
    UuidV7RepoWatchDispatchIdGenerator, UuidV7StartEligibleTurnIdGenerator,
};
use signalbox_domain::{
    AcceptedInputId, ActiveTurnPhase, AssistantResponsePart, BranchName, CheckConclusion,
    CommitSha, ContextFrontierId, DangerousToolAutoApproval, DelegateApprovalRecommendation,
    DescendantTerminationScope, DirectModelSelection, DurableCommandId,
    FailedModelCallTurnIdentities, GitHubObjectId, GoalCommandResult, GoalModelProvenance,
    GoalNeed, GoalReport, GoalSchedulerProvenance, GoalState, GoalStatement, GoalUserAction,
    GoalUserCommand, InitialToolApproval, MergeableState, ModelCallId, ModelCallTerminalIdentities,
    ModelCallTerminalObservation, ModelCallTerminalOutcome, ModelSelectionRequest,
    ModelTargetCatalog, ModelTargetDefinition, NormalizedToolArguments, ProviderModelIdentity,
    ProviderReportedTokenUsage, PullRequestBody, PullRequestEventContext,
    PullRequestEventContextInput, PullRequestNumber, PullRequestTitle, RepoWatchActionV1,
    RepoWatchAuthorLogin, RepoWatchEvent, RepoWatchEventId, RepoWatchEventKindNameV1,
    RepoWatchEventKindV1, RepoWatchEventTarget, RepoWatchMatcherV1, RepoWatchMatcherV1Input,
    RepoWatchPattern, RepoWatchRule, RepoWatchRuleActionV1, RepoWatchRuleId,
    RepoWatchRuleIdentityField, RepoWatchRuleVersion, RepoWatchSingletonScope,
    RepoWatchWorkflowRunAttempt, RepositorySlug, ResolvedProviderTarget, SemanticTranscriptEntryId,
    SessionConfigurationDefaults, SessionId, SessionSystemPrompt, SessionTemplateContentDigest,
    SessionTemplateName, SessionTemplateProvenance, ToolCallProposal, ToolDecisionRationale,
    ToolName, ToolRequestId, ToolResponsePartIdentity, ToolRoundModelCallIdentities,
    ToolUsingAssistantResponse, TurnAttemptId, TurnId, UserContent, WorkflowName,
};
use signalbox_persistence::{
    SessionCredentialPin, SessionModelCredential,
    approval_judge::{
        CompleteApprovalJudgeOutcome, PrepareApprovalJudgeOutcome, PreparedApprovalJudge,
    },
    disposable_test_container_labels,
    goal::{GoalCommandHandlingOutcome, GoalRepository, GoalTransitionOutcome},
    goal_turn::GoalTurnCandidates,
    local_test_connection_options, migrate,
    model_execution::{PostgresModelCallRepository, PrepareInitialModelCallOutcome},
    repo_watch::{
        PostgresRepoWatchStore, RepoWatchCommitOutcome, RepoWatchCommitRequest,
        RepoWatchCursorCandidate, RepoWatchCursorGeneration,
    },
    repo_watch_dispatch::{PostgresRepoWatchDispatchStore, RepoWatchDispatchRepositoryError},
    repo_watch_dispatch_obligation::RepoWatchDispatchObligation,
    start_eligible_turn::StartEligibleTurnRepository,
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_repo_watch_dispatch";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const REPOSITORY: &str = "signalbox/repository";
const SECOND_REPOSITORY: &str = "signalbox/second-repository";
const HEAD_REPOSITORY: &str = "contributor/repository";
const BASE_BRANCH: &str = "main";
const HEAD_BRANCH: &str = "feature/repo-watch";
const INITIAL_HEAD: &str = "0000000000000000000000000000000000000000";
const FIRST_HEAD: &str = "1111111111111111111111111111111111111111";
const SECOND_HEAD: &str = "2222222222222222222222222222222222222222";
const THIRD_HEAD: &str = "3333333333333333333333333333333333333333";
const TEMPLATE: &str = "merge-forward";
const RULE: &str = "merge-forward-on-conflict";
const FRESH_RULE: &str = "merge-forward-on-conflict-replacement";
const REPLACEMENT_RULE_VERSION: u64 = 2;
const EAGER_RULE: &str = "merge-forward-on-base-advance";
const AGENT_HEAD_PATTERN: &str = "^agent/.+$";
const BOTTOM_AGENT_BRANCH: &str = "agent/bottom";
const TOP_AGENT_BRANCH: &str = "agent/top";
const BOTTOM_PULL_REQUEST_NUMBER: u64 = 41;
const TOP_PULL_REQUEST_NUMBER: u64 = 42;
const MAIN_OPENED_EVENT_ID: u128 = 0x60_000;
const MAIN_BASE_ADVANCED_EVENT_ID: u128 = 0x60_001;
const STACK_BOTTOM_OPENED_EVENT_ID: u128 = 0x60_100;
const STACK_TOP_OPENED_EVENT_ID: u128 = 0x60_101;
const STACK_PARENT_HEAD_CHANGED_EVENT_ID: u128 = 0x60_102;
const STACK_BASE_ADVANCED_EVENT_ID: u128 = 0x60_103;
const DISPATCH_CONTEXT: &str = r#"{"fixture":"repository-watch"}"#;
const FIRST_TERMINAL_IDENTITY_SEED: u128 = 0x10_000;
const CORRUPT_GOAL_GENERATION: i64 = 2;
const MERGED_GOAL_CUTOFF_EVENT_ID: u128 = 0x51_000;
const CORRUPT_GOAL_FIRST_CUTOFF_EVENT_ID: u128 = 0x51_200;
const CORRUPT_GOAL_SECOND_CUTOFF_EVENT_ID: u128 = 0x51_300;
const TERMINAL_RULE_OPENED_EVENT_ID: u128 = 0x54_000;
const TERMINAL_RULE_MERGED_EVENT_ID: u128 = 0x54_100;
const STARTUP_DRAIN_CUTOFF_EVENT_ID: u128 = 0x57_100;
const STARTUP_DRAIN_STOP_COMMAND_ID: u128 = 0x57_110;
const BRANCH_RULE: &str = "branch-workflow-follow-up";
const WORKFLOW_NAME: &str = "rust";
const WORKFLOW_BRANCH: &str = "main";
const BRANCH_WORKFLOW_RUN_ID: u64 = 9_001;
const BRANCH_WORKFLOW_ID: u64 = 9_002;
const BRANCH_ACTIVATION_EVENT_ID: u128 = 0x58_000;
const BRANCH_WORKFLOW_EVENT_ID: u128 = 0x58_100;
const BRANCH_ACHIEVEMENT_REQUEST_ID: u128 = 0x58_200;
const SUPERSEDING_HEAD_EVENT_ID: u128 = 0x50_700;
const SUPERSEDED_ACHIEVEMENT_REQUEST_ID: u128 = 0x50_701;
const SUCCESSOR_ACHIEVEMENT_REQUEST_ID: u128 = 0x50_702;
const STOPPED_TERMINAL_OPENED_EVENT_ID: u128 = 0x59_000;
const STOPPED_TERMINAL_MERGED_EVENT_ID: u128 = 0x59_100;
const STOPPED_TERMINAL_STOP_COMMAND_ID: u128 = 0x59_200;
const TERMINATION_RACE_STOP_COMMAND_ID: u128 = 0x59_300;
const SUCCESSOR_GOAL_ATTACH_COMMAND_ID: u128 = 0x59_400;
const SUCCESSOR_GOAL_STOP_COMMAND_ID: u128 = 0x59_500;
const RELEASED_ACHIEVEMENT_REQUEST_ID: u128 = 0x59_600;
const SUCCESSOR_GOAL_INPUT_ID: u128 = 0x59_700;
const SUCCESSOR_GOAL_TURN_ID: u128 = 0x59_800;
const REOPENED_CLOSE_OPENED_EVENT_ID: u128 = 0x5a_000;
const REOPENED_CLOSE_CLOSED_EVENT_ID: u128 = 0x5a_100;
const REOPENED_CLOSE_REOPENED_EVENT_ID: u128 = 0x5a_200;
const REOPENED_CLOSE_STOP_COMMAND_ID: u128 = 0x5a_300;
const SIBLING_ACHIEVEMENT_REQUEST_ID: u128 = 0x5b_000;
const SIBLING_ATTACH_COMMAND_ID: u128 = 0x5b_100;
const SIBLING_STOP_COMMAND_ID: u128 = 0x5b_200;
const SIBLING_GOAL_INPUT_ID: u128 = 0x5b_300;
const SIBLING_GOAL_TURN_ID: u128 = 0x5b_400;
const UNKNOWN_DELIVERED_REQUEST_ID: u128 = 0x5c_000;

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_fsync_enabled()
        .with_tag(POSTGRES_IMAGE_TAG)
        .with_labels(disposable_test_container_labels())
        .start()
        .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let database_url =
        format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

fn repository() -> Result<RepositorySlug, Box<dyn Error>> {
    Ok(RepositorySlug::try_new(REPOSITORY.to_owned())?)
}

fn context(head: &str) -> Result<PullRequestEventContext, Box<dyn Error>> {
    Ok(PullRequestEventContext::new(PullRequestEventContextInput {
        number: PullRequestNumber::new(BOTTOM_PULL_REQUEST_NUMBER.try_into()?),
        head_sha: CommitSha::try_new(head.to_owned())?,
        head_repository: RepositorySlug::try_new(HEAD_REPOSITORY.to_owned())?,
        base_branch: BranchName::try_new(BASE_BRANCH.to_owned())?,
        head_branch: BranchName::try_new(HEAD_BRANCH.to_owned())?,
        title: PullRequestTitle::try_new("Merge forward".to_owned())?,
        body: PullRequestBody::try_new("Resolve the conflict.".to_owned())?,
        labels: Vec::new(),
        draft: false,
        author: Some(RepoWatchAuthorLogin::try_new("fixture-author".to_owned())?),
    }))
}

/// Named facts for one same-repository pull request in the eager-rule tests.
struct SameRepositoryContextFacts<'a> {
    number: u64,
    head: &'a str,
    base_branch: &'a str,
    head_branch: &'a str,
}

fn same_repository_context(
    facts: SameRepositoryContextFacts<'_>,
) -> Result<PullRequestEventContext, Box<dyn Error>> {
    Ok(PullRequestEventContext::new(PullRequestEventContextInput {
        number: PullRequestNumber::new(facts.number.try_into()?),
        head_sha: CommitSha::try_new(facts.head.to_owned())?,
        head_repository: repository()?,
        base_branch: BranchName::try_new(facts.base_branch.to_owned())?,
        head_branch: BranchName::try_new(facts.head_branch.to_owned())?,
        title: PullRequestTitle::try_new("Merge forward".to_owned())?,
        body: PullRequestBody::try_new("Advance the dependent.".to_owned())?,
        labels: Vec::new(),
        draft: false,
        author: Some(RepoWatchAuthorLogin::try_new("fixture-author".to_owned())?),
    }))
}

fn observation(context: PullRequestEventContext) -> Result<RepoWatchObservation, Box<dyn Error>> {
    lifecycle_observation(context, RepoWatchPullRequestLifecycle::Open)
}

fn lifecycle_observation(
    context: PullRequestEventContext,
    lifecycle: RepoWatchPullRequestLifecycle,
) -> Result<RepoWatchObservation, Box<dyn Error>> {
    Ok(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![RepoWatchPullRequestState::try_new(
                RepoWatchPullRequestStateInput {
                    context,
                    lifecycle,
                    mergeable_state: MergeableState::Conflicting,
                    completed_check_suites: Vec::new(),
                    completed_check_runs: Vec::new(),
                    reviews: Vec::new(),
                    threads: Vec::new(),
                    reactions: Vec::new(),
                },
            )?],
            workflow_runs: Vec::new(),
            branch_heads: Vec::new(),
        })?,
    ))
}

fn mergeable_observation(
    contexts: Vec<PullRequestEventContext>,
    branch_heads: Vec<RepoWatchBranchHead>,
) -> Result<RepoWatchObservation, Box<dyn Error>> {
    let pull_requests = contexts
        .into_iter()
        .map(|context| {
            RepoWatchPullRequestState::try_new(RepoWatchPullRequestStateInput {
                context,
                lifecycle: RepoWatchPullRequestLifecycle::Open,
                mergeable_state: MergeableState::Mergeable,
                completed_check_suites: Vec::new(),
                completed_check_runs: Vec::new(),
                reviews: Vec::new(),
                threads: Vec::new(),
                reactions: Vec::new(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests,
            workflow_runs: Vec::new(),
            branch_heads,
        })?,
    ))
}

fn opened_event(value: u128, head: &str) -> Result<RepoWatchEvent, Box<dyn Error>> {
    Ok(RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        context(head)?,
        RepoWatchEventKindV1::PullRequestOpened,
    )?)
}

fn merged_event(value: u128, head: &str) -> Result<RepoWatchEvent, Box<dyn Error>> {
    Ok(RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        context(head)?,
        RepoWatchEventKindV1::PullRequestMerged,
    )?)
}

fn conflict_event(value: u128, head: &str) -> Result<RepoWatchEvent, Box<dyn Error>> {
    Ok(RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        context(head)?,
        RepoWatchEventKindV1::MergeableStateChanged {
            current: MergeableState::Conflicting,
        },
    )?)
}

fn identified_event(event: RepoWatchEvent) -> RepoWatchEventOccurrenceV1 {
    let mut identity = [0_u8; 32];
    identity[..16].copy_from_slice(event.id().as_uuid().as_bytes());
    identity[16..].copy_from_slice(event.id().as_uuid().as_bytes());
    RepoWatchEventOccurrenceV1::from_parts(
        event,
        RepoWatchEventContentIdentityV1::from_bytes(identity),
    )
}

fn opened_event_for(
    value: u128,
    context: PullRequestEventContext,
) -> Result<RepoWatchEvent, Box<dyn Error>> {
    Ok(RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        context,
        RepoWatchEventKindV1::PullRequestOpened,
    )?)
}

fn head_changed_event(
    value: u128,
    context: PullRequestEventContext,
    previous: &str,
) -> Result<RepoWatchEvent, Box<dyn Error>> {
    let current = context.head_sha().clone();
    Ok(RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        context,
        RepoWatchEventKindV1::HeadChanged {
            previous: CommitSha::try_new(previous.to_owned())?,
            current,
        },
    )?)
}

fn base_advanced_event(
    value: u128,
    context: PullRequestEventContext,
) -> Result<RepoWatchEvent, Box<dyn Error>> {
    let branch = context.base_branch().clone();
    Ok(RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        context,
        RepoWatchEventKindV1::BaseAdvanced { branch },
    )?)
}

fn rule_with_actions_and_cooldown(
    version: RepoWatchRuleVersion,
    actions: Vec<RepoWatchRuleActionV1>,
    cooldown: Duration,
) -> Result<RepoWatchRule, Box<dyn Error>> {
    Ok(RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(RULE.to_owned())?,
        version,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::MergeableStateChanged],
            mergeable_state: vec![MergeableState::Conflicting],
            ..RepoWatchMatcherV1Input::default()
        }),
        actions,
        RepoWatchSingletonScope::PullRequest,
        cooldown,
    )?)
}

fn rule() -> Result<RepoWatchRule, Box<dyn Error>> {
    rule_at_version(RepoWatchRuleVersion::V1)
}

fn rule_at_version(version: RepoWatchRuleVersion) -> Result<RepoWatchRule, Box<dyn Error>> {
    rule_with_identity(RepoWatchRuleId::try_new(RULE.to_owned())?, version)
}

fn rule_with_identity(
    id: RepoWatchRuleId,
    version: RepoWatchRuleVersion,
) -> Result<RepoWatchRule, Box<dyn Error>> {
    let template = SessionTemplateName::try_new(TEMPLATE.to_owned())?;
    Ok(RepoWatchRule::try_new(
        id,
        version,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::MergeableStateChanged],
            mergeable_state: vec![MergeableState::Conflicting],
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![
            RepoWatchRuleActionV1::DispatchSession {
                template: template.clone(),
            },
            RepoWatchRuleActionV1::DispatchSession { template },
        ],
        RepoWatchSingletonScope::PullRequest,
        Duration::ZERO,
    )?)
}

fn cooldown_rule() -> Result<RepoWatchRule, Box<dyn Error>> {
    rule_with_actions_and_cooldown(
        RepoWatchRuleVersion::V1,
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(TEMPLATE.to_owned())?,
        }],
        Duration::from_secs(60 * 60),
    )
}

fn one_action_rule(cooldown: Duration) -> Result<RepoWatchRule, Box<dyn Error>> {
    rule_with_actions_and_cooldown(
        RepoWatchRuleVersion::V1,
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(TEMPLATE.to_owned())?,
        }],
        cooldown,
    )
}

fn eager_merge_forward_rule() -> Result<RepoWatchRule, Box<dyn Error>> {
    Ok(RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(EAGER_RULE.to_owned())?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::BaseAdvanced],
            repository: Some(repository()?),
            head_branch: Some(RepoWatchPattern::try_new(AGENT_HEAD_PATTERN.to_owned())?),
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(TEMPLATE.to_owned())?,
        }],
        RepoWatchSingletonScope::PullRequest,
        Duration::ZERO,
    )?)
}

fn branch_workflow_rule() -> Result<RepoWatchRule, Box<dyn Error>> {
    Ok(RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(BRANCH_RULE.to_owned())?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::BranchWorkflowRunCompleted],
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(TEMPLATE.to_owned())?,
        }],
        RepoWatchSingletonScope::Repository,
        Duration::ZERO,
    )?)
}

fn branch_workflow_event(value: u128) -> Result<RepoWatchEvent, Box<dyn Error>> {
    Ok(RepoWatchEvent::branch_workflow(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        BranchName::try_new(WORKFLOW_BRANCH.to_owned())?,
        WorkflowName::try_new(WORKFLOW_NAME.to_owned())?,
        CheckConclusion::Failure,
    ))
}

fn branch_observation() -> Result<RepoWatchObservation, Box<dyn Error>> {
    Ok(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: Vec::new(),
            workflow_runs: vec![RepoWatchWorkflowRunObservation::new(
                GitHubObjectId::new(
                    NonZeroU64::new(BRANCH_WORKFLOW_RUN_ID).expect("fixture run id is positive"),
                ),
                GitHubObjectId::new(
                    NonZeroU64::new(BRANCH_WORKFLOW_ID).expect("fixture workflow id is positive"),
                ),
                RepoWatchWorkflowRunAttempt::new(
                    NonZeroU64::new(1).expect("fixture attempt is positive"),
                ),
                BranchName::try_new(WORKFLOW_BRANCH.to_owned())?,
                WorkflowName::try_new(WORKFLOW_NAME.to_owned())?,
                CheckConclusion::Failure,
            )],
            branch_heads: Vec::new(),
        })?,
    ))
}

fn closed_event(value: u128, head: &str) -> Result<RepoWatchEvent, Box<dyn Error>> {
    Ok(RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        context(head)?,
        RepoWatchEventKindV1::PullRequestClosed,
    )?)
}

fn closed_event_rule() -> Result<RepoWatchRule, Box<dyn Error>> {
    Ok(RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(RULE.to_owned())?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::PullRequestClosed],
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(TEMPLATE.to_owned())?,
        }],
        RepoWatchSingletonScope::PullRequest,
        Duration::ZERO,
    )?)
}

fn merged_event_rule() -> Result<RepoWatchRule, Box<dyn Error>> {
    Ok(RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(RULE.to_owned())?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::PullRequestMerged],
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(TEMPLATE.to_owned())?,
        }],
        RepoWatchSingletonScope::PullRequest,
        Duration::ZERO,
    )?)
}

fn conflict_and_merged_event_rule() -> Result<RepoWatchRule, Box<dyn Error>> {
    Ok(RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(RULE.to_owned())?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![
                RepoWatchEventKindNameV1::MergeableStateChanged,
                RepoWatchEventKindNameV1::PullRequestMerged,
            ],
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(TEMPLATE.to_owned())?,
        }],
        RepoWatchSingletonScope::PullRequest,
        Duration::ZERO,
    )?)
}

struct TemplateResolver;

impl RepoWatchTemplateResolver for TemplateResolver {
    fn resolve_repo_watch_template(
        &self,
        name: &SessionTemplateName,
    ) -> Option<RepoWatchResolvedTemplate> {
        Some(RepoWatchResolvedTemplate::new(
            SessionTemplateProvenance::new(
                name.clone(),
                SessionTemplateContentDigest::from_bytes([7; 32]),
            ),
            SessionConfigurationDefaults::complete(
                ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(Uuid::from_u128(
                    901,
                ))),
                DangerousToolAutoApproval::Disabled,
                Some(
                    SessionSystemPrompt::try_new("Merge the base branch forward.".to_owned())
                        .expect("fixture prompt is valid"),
                ),
            ),
        ))
    }
}

struct ObligationTransaction {
    store: PostgresRepoWatchDispatchStore,
    obligation: Option<RepoWatchDispatchObligation>,
}

struct EvaluatedConflict {
    outcome: RepoWatchRuleEvaluationOutcome,
    event_id: RepoWatchEventId,
}

#[derive(Debug, sqlx::FromRow)]
struct OutstandingCooldownVisibility {
    matched_event_count: i64,
    eligible_at_is_future: bool,
    ready: bool,
}

#[derive(Debug, sqlx::FromRow)]
struct OutstandingTerminationVisibility {
    obligation_id: Uuid,
    latest_event_id: Uuid,
    matched_event_count: i64,
    ready: bool,
}

#[derive(Debug, sqlx::FromRow)]
struct HeldSlotVisibility {
    every_action_delivered: bool,
    every_delivery_turn_releasable: bool,
    no_live_runtime_turn: bool,
    every_goal_nonpursuing: bool,
    blockers: Vec<String>,
}

impl RepoWatchDispatchTransaction for ObligationTransaction {
    type Error = RepoWatchDispatchRepositoryError;

    async fn handle_repo_watch_evaluation(
        &mut self,
        evaluation: RepoWatchRuleEvaluation,
    ) -> Result<RepoWatchRuleEvaluationOutcome, Self::Error> {
        let obligation =
            self.obligation
                .take()
                .ok_or(RepoWatchDispatchRepositoryError::Corruption(
                    "test obligation transaction was reused",
                ))?;
        self.store
            .handle_repo_watch_obligation_with_alias_resolver(obligation, evaluation, |_| None)
            .await
    }
}

fn credential_pin() -> SessionCredentialPin {
    SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        "fixture-family",
        "fixture-credential",
    )])
    .expect("fixture credential pin is valid")
}

fn model_credential_reference() -> ModelCallCredentialReference {
    ModelCallCredentialReference::new("fixture-credential")
}

fn model_targets() -> ModelTargetCatalog {
    ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        DirectModelSelection::from_uuid(Uuid::from_u128(901)),
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(902))),
    )])
    .expect("one fixture target forms a catalog")
}

#[track_caller]
fn ready_approval_judge(outcome: PrepareApprovalJudgeOutcome) -> PreparedApprovalJudge {
    let PrepareApprovalJudgeOutcome::Ready(prepared) = outcome else {
        panic!("the delegated fixture prepares a fresh judge call")
    };
    *prepared
}

#[track_caller]
fn prepared_pull_request_authority(
    prepared: &PreparedApprovalJudge,
) -> &ApprovalJudgePullRequestAuthority {
    let Some(ApprovalJudgeDispatchAuthority::PullRequest(authority)) =
        prepared.session_context().dispatch()
    else {
        panic!("the dispatched pull-request session carries its immutable fence")
    };
    authority
}

fn dispatch_context() -> UserContent {
    UserContent::try_text(DISPATCH_CONTEXT.to_owned()).expect("fixture dispatch context is valid")
}

fn generation(outcome: RepoWatchCommitOutcome) -> RepoWatchCursorGeneration {
    match outcome {
        RepoWatchCommitOutcome::Committed(cursor) => cursor.generation(),
        _ => panic!("fixture cursor commit must be new"),
    }
}

fn dispatched(
    outcome: RepoWatchRuleEvaluationOutcome,
) -> (signalbox_domain::RepoWatchDispatchId, Box<[SessionId]>) {
    match outcome {
        RepoWatchRuleEvaluationOutcome::Dispatched {
            dispatch_id,
            sessions,
        } => (dispatch_id, sessions),
        _ => panic!("fixture rule evaluation must dispatch"),
    }
}

fn replayed(
    outcome: RepoWatchRuleEvaluationOutcome,
) -> (signalbox_domain::RepoWatchDispatchId, Box<[SessionId]>) {
    match outcome {
        RepoWatchRuleEvaluationOutcome::Replayed {
            dispatch_id,
            sessions,
        } => (dispatch_id, sessions),
        _ => panic!("fixture obligation must replay its dispatch"),
    }
}

fn pull_request_number(event: &RepoWatchEvent) -> PullRequestNumber {
    let RepoWatchEventTarget::PullRequest(context) = event.target() else {
        panic!("fixture event must target a pull request");
    };
    context.number()
}

fn session_uuids(fixture: &DispatchFixture) -> Vec<Uuid> {
    fixture
        .sessions
        .iter()
        .map(|session| *session.as_uuid())
        .collect()
}

/// Closed classification of one rule-admission refusal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuleAdmissionRefusal {
    ReusedIdentity,
    ChangedField(RepoWatchRuleIdentityField),
    RegressedVersion {
        configured: RepoWatchRuleVersion,
        latest: RepoWatchRuleVersion,
    },
    StorageCorruption,
    NotAnAdmissionRefusal,
}

/// Classifies a reconciliation failure for the assertions below.
///
/// One exhaustive accessor rather than a wildcard per assertion: a repository
/// error variant added later forces a classification decision here instead of
/// silently reading as "not the refusal under test" at every call site.
fn admission_refusal(error: &RepoWatchDispatchRepositoryError) -> RuleAdmissionRefusal {
    match error {
        RepoWatchDispatchRepositoryError::ReusedRuleIdentity { .. } => {
            RuleAdmissionRefusal::ReusedIdentity
        }
        RepoWatchDispatchRepositoryError::ChangedRuleIdentity { field, .. } => {
            RuleAdmissionRefusal::ChangedField(*field)
        }
        RepoWatchDispatchRepositoryError::RegressedRuleVersion {
            rule_version,
            latest_version,
            ..
        } => RuleAdmissionRefusal::RegressedVersion {
            configured: *rule_version,
            latest: *latest_version,
        },
        RepoWatchDispatchRepositoryError::Corruption(_) => RuleAdmissionRefusal::StorageCorruption,
        RepoWatchDispatchRepositoryError::Database(_)
        | RepoWatchDispatchRepositoryError::CommitAmbiguous(_)
        | RepoWatchDispatchRepositoryError::EventStore(_)
        | RepoWatchDispatchRepositoryError::SessionCreation(_)
        | RepoWatchDispatchRepositoryError::InitialInput(_)
        | RepoWatchDispatchRepositoryError::GoalCommission(_)
        | RepoWatchDispatchRepositoryError::GoalCutoff(_) => {
            RuleAdmissionRefusal::NotAnAdmissionRefusal
        }
    }
}

/// The recorded revisions of one rule and whether each is deactivated.
async fn revisions_of(
    fixture: &DispatchFixture,
    rule_id: &str,
) -> Result<Vec<(i64, bool)>, Box<dyn Error>> {
    Ok(sqlx::query_as(
        "SELECT activation.rule_version, deactivation.rule_id IS NOT NULL AS deactivated
           FROM repo_watch_rule_activation AS activation
           LEFT JOIN repo_watch_rule_deactivation AS deactivation
             USING (repository, rule_id, rule_version)
          WHERE activation.repository = $1 AND activation.rule_id = $2
          ORDER BY activation.rule_version",
    )
    .bind(fixture.repository.as_str())
    .bind(rule_id)
    .fetch_all(&fixture.pool)
    .await?)
}

/// Removes the fixture rule's stored fingerprints to stage a corrupt shape.
///
/// The table is append-only in production, so the trigger is disabled only
/// long enough to reach a state reconciliation must refuse.
async fn remove_field_fingerprints(fixture: &DispatchFixture) -> Result<(), Box<dyn Error>> {
    sqlx::query(
        "ALTER TABLE repo_watch_rule_field_fingerprint
         DISABLE TRIGGER repo_watch_rule_field_fingerprint_is_append_only",
    )
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        "DELETE FROM repo_watch_rule_field_fingerprint
          WHERE repository = $1 AND rule_id = $2 AND rule_version = $3",
    )
    .bind(fixture.repository.as_str())
    .bind(fixture.rule.id().as_str())
    .bind(i64::try_from(fixture.rule.version().get())?)
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        "ALTER TABLE repo_watch_rule_field_fingerprint
         ENABLE TRIGGER repo_watch_rule_field_fingerprint_is_append_only",
    )
    .execute(&fixture.pool)
    .await?;
    Ok(())
}

fn outcome_is_dispatched(outcome: &RepoWatchRuleEvaluationOutcome) -> bool {
    matches!(outcome, RepoWatchRuleEvaluationOutcome::Dispatched { .. })
}

#[track_caller]
fn assert_applied_goal_transition(outcome: GoalTransitionOutcome) {
    let GoalTransitionOutcome::Applied(_) = outcome else {
        panic!("fixture goal transition must apply");
    };
}

#[track_caller]
fn assert_applied_goal_command(outcome: GoalCommandHandlingOutcome) {
    let GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(_)) = outcome else {
        panic!("fixture goal command must apply");
    };
}

/// Withdraws the goal commissioned for one dispatched session.
///
/// A dispatched session's pursuing goal holds the batch's singleton on its own,
/// so a test whose subject is the release mechanism has to end that pursuit
/// before the turn it is exercising can release anything.
///
/// This stops the goal rather than failing its turn. A stopped queued or active
/// turn is runtime-irrelevant and therefore no longer owns the singleton.
async fn withdraw_dispatched_goal(
    pool: &PgPool,
    session: SessionId,
    identity_seed: u128,
) -> Result<(), Box<dyn Error>> {
    assert_applied_goal_command(
        GoalRepository::new(pool.clone())
            .handle_user_command(
                GoalUserCommand::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(identity_seed)),
                    session,
                    GoalUserAction::Stop {
                        descendant_scope: DescendantTerminationScope::ParentAlone,
                    },
                ),
                None,
                |_| None,
            )
            .await?,
    );
    Ok(())
}

/// Inserts the exact durable request and transcript shape authenticated by an
/// achieved goal declaration for one dispatched turn.
async fn insert_achievement_declaration_request(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
    request: ToolRequestId,
    report: &str,
) -> Result<(), Box<dyn Error>> {
    let producing_call = Uuid::from_u128(request.as_uuid().as_u128() + 0x1000);
    sqlx::query("ALTER TABLE tool_request DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO tool_request
            (request_id, session_id, turn_id, producing_model_call_id,
             request_ordinal, tool_name, arguments_kind, arguments_text)
         VALUES ($1, $2, $3, $4, 0, 'goal_declare', 'json', $5)",
    )
    .bind(request.as_uuid())
    .bind(session.as_uuid())
    .bind(turn.as_uuid())
    .bind(producing_call)
    .bind(r#"{"transition":"achieved"}"#)
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE semantic_transcript_entry DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             assistant_text_value, producing_model_call_id,
             assistant_response_part_ordinal, assistant_tool_request_id)
         VALUES ($1, $2, 'assistant_text', $4, $3, 0, NULL),
                ($1, $5, 'assistant_tool_use', NULL, $3, 1, $6)",
    )
    .bind(session.as_uuid())
    .bind(Uuid::from_u128(request.as_uuid().as_u128() + 0x2000))
    .bind(producing_call)
    .bind(report)
    .bind(Uuid::from_u128(request.as_uuid().as_u128() + 0x3000))
    .bind(request.as_uuid())
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE semantic_transcript_entry ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE tool_request ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

async fn declare_dispatched_goal_achieved(
    fixture: &DispatchFixture,
    action_ordinal: usize,
    request_seed: u128,
) -> Result<(), Box<dyn Error>> {
    declare_session_goal_achieved(&fixture.pool, fixture.session(action_ordinal), request_seed)
        .await
}

/// Declares the goal of one dispatched session achieved.
///
/// A dispatched session owns exactly one admitted action, so its delivered turn
/// is recoverable from the session alone — including for a successor batch this
/// test module never named a dispatch identifier for.
async fn declare_session_goal_achieved(
    pool: &PgPool,
    session: SessionId,
    request_seed: u128,
) -> Result<(), Box<dyn Error>> {
    let turn = TurnId::from_uuid(
        sqlx::query_scalar::<_, Uuid>(
            "SELECT delivery.turn_id
               FROM repo_watch_dispatch_delivery AS delivery
               JOIN repo_watch_dispatch_action AS action
                 ON action.dispatch_id = delivery.dispatch_id
                AND action.action_ordinal = delivery.action_ordinal
              WHERE action.session_id = $1",
        )
        .bind(session.as_uuid())
        .fetch_one(pool)
        .await?,
    );
    let request = ToolRequestId::from_uuid(Uuid::from_u128(request_seed));
    let report = String::from("the dispatched pull request is converged");
    insert_achievement_declaration_request(pool, session, turn, request, &report).await?;
    assert_applied_goal_transition(
        GoalRepository::new(pool.clone())
            .declare_achieved(
                session,
                GoalReport::try_new(report).expect("fixture goal report is valid"),
                GoalModelProvenance::new(turn, request),
            )
            .await?,
    );
    Ok(())
}

/// Terminalizes a dispatched session's only queued turn as a failure.
///
/// A dispatched session holds exactly one queued turn, so the turn this fails
/// is always first in its session and has no predecessor to name.
async fn mark_queued_turn_failed(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
    identity_seed: u128,
) -> Result<(), Box<dyn Error>> {
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal', start_lineage_kind = 'first_in_session',
                immediate_predecessor_turn_id = NULL, starting_frontier_id = $3,
                terminal_frontier_id = $4, terminal_disposition_kind = 'failed'
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.as_uuid())
    .bind(turn.as_uuid())
    .bind(Uuid::from_u128(identity_seed))
    .bind(Uuid::from_u128(identity_seed + 1))
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

async fn check_completed_turn_for_release(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
) -> Result<(), Box<dyn Error>> {
    sqlx::query("SELECT repo_watch_release_completed_dispatch_batches_for_turn($1, $2)")
        .bind(turn.as_uuid())
        .bind(session.as_uuid())
        .execute(pool)
        .await?;
    Ok(())
}

async fn outstanding_obligation_count(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM repo_watch_dispatch_obligation WHERE settled_kind IS NULL",
    )
    .fetch_one(pool)
    .await
}

async fn dispatched_obligation_count(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_dispatch_obligation
          WHERE settled_kind = 'dispatched'",
    )
    .fetch_one(pool)
    .await
}

async fn release_count(fixture: &DispatchFixture) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_release WHERE dispatch_id = $1")
        .bind(fixture.dispatch_id.as_uuid())
        .fetch_one(&fixture.pool)
        .await
}

async fn wait_for_backend_lock(pool: &PgPool, backend: i32) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                    SELECT 1
                      FROM pg_stat_activity
                     WHERE pid = $1
                       AND wait_event_type = 'Lock'
                )",
            )
            .bind(backend)
            .fetch_one(pool)
            .await?;
            if waiting {
                return Ok::<(), sqlx::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    Ok(())
}

/// Takes, in its own transaction, the singleton advisory key SQL computes for
/// one admitted batch.
async fn hold_singleton_advisory_key(
    fixture: &DispatchFixture,
) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, Box<dyn Error>> {
    let mut holder = fixture.pool.begin().await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(
                    hashtextextended(
                        repo_watch_dispatch_singleton_lock_key(
                            batch.rule_id,
                            batch.rule_version,
                            batch.singleton_scope,
                            batch.singleton_repository,
                            batch.singleton_pull_request_number,
                            batch.singleton_stack_root_pull_request_number
                        ),
                        0
                    )
                )
           FROM repo_watch_dispatch_batch AS batch
          WHERE batch.dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .execute(&mut *holder)
    .await?;
    Ok(holder)
}

async fn wait_for_advisory_lock(pool: &PgPool) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                    SELECT 1
                      FROM pg_locks
                     WHERE locktype = 'advisory'
                       AND NOT granted
                )",
            )
            .fetch_one(pool)
            .await?;
            if waiting {
                return Ok::<(), sqlx::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    Ok(())
}

struct DispatchFixture {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
    repository: RepositorySlug,
    event: RepoWatchEvent,
    observation: RepoWatchObservation,
    rule: RepoWatchRule,
    store: PostgresRepoWatchDispatchStore,
    dispatch_id: signalbox_domain::RepoWatchDispatchId,
    sessions: Box<[SessionId]>,
}

impl DispatchFixture {
    /// The session this dispatch created for the action at the given ordinal.
    #[track_caller]
    fn session(&self, action_ordinal: usize) -> SessionId {
        self.sessions[action_ordinal]
    }
}

async fn dispatch_fixture() -> Result<DispatchFixture, Box<dyn Error>> {
    dispatch_fixture_for(rule()?).await
}

async fn dispatch_fixture_for(rule: RepoWatchRule) -> Result<DispatchFixture, Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let initial_observation = observation(context(INITIAL_HEAD)?)?;
    let initial = RepoWatchCursorCandidate::new(initial_observation);
    let first_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    initial,
                    vec![identified_event(opened_event(100, INITIAL_HEAD)?)],
                ),
            )
            .await?,
    );
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let event = conflict_event(101, FIRST_HEAD)?;
    let observation = observation(context(FIRST_HEAD)?)?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(observation.clone()),
                vec![identified_event(event.clone())],
            ),
        )
        .await?;
    let loaded = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("activated fixture rule sees its first event");
    let outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                loaded,
                &rule,
                &observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    let (dispatch_id, sessions) = dispatched(outcome);
    Ok(DispatchFixture {
        _container: container,
        pool,
        repository,
        event,
        observation,
        rule,
        store: dispatch_store,
        dispatch_id,
        sessions,
    })
}

async fn checkpoint_dispatched_delegated_approval(
    fixture: &DispatchFixture,
    seed: u128,
) -> Result<
    (
        PostgresModelCallRepository,
        PreparedApprovalJudge,
        TurnId,
        ToolRequestId,
    ),
    Box<dyn Error>,
> {
    let session = fixture.session(0);
    let mut activation = StartEligibleTurnService::new(
        UuidV7StartEligibleTurnIdGenerator,
        StartEligibleTurnRepository::new(fixture.pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) = activation.execute(session).await? else {
        panic!("the dispatched work turn activates")
    };
    let turn = activated.turn();
    drop(activated);

    let repository = PostgresModelCallRepository::new(
        fixture.pool.clone(),
        model_targets(),
        model_credential_reference(),
    );
    let call = ModelCallId::from_uuid(Uuid::from_u128(seed));
    let PrepareInitialModelCallOutcome::Checkpointed(checkpointed) = repository
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 1)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 2)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 3)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 4)),
                    TurnId::from_uuid(Uuid::from_u128(seed + 5)),
                )
            },
        )
        .await?
    else {
        panic!("the dispatched turn checkpoints its initial model call")
    };
    assert_eq!(checkpointed, call);
    let PrepareInitialModelCallOutcome::Ready { .. } = repository
        .prepare_initial_call(
            session,
            ModelCallId::from_uuid(Uuid::from_u128(seed + 20)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 21)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 23)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 24)),
                    TurnId::from_uuid(Uuid::from_u128(seed + 25)),
                )
            },
        )
        .await?
    else {
        panic!("the checkpointed initial model call reloads ready")
    };
    let AuthorizeModelCallOutcome::Authorized(authorized) =
        repository.authorize_send(session, call).await?
    else {
        panic!("the initial model call authorizes")
    };
    let request = ToolRequestId::from_uuid(Uuid::from_u128(seed + 6));
    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            ToolCallProposal::new(
                ToolName::try_new(String::from("exec")).expect("the fixture tool name is admitted"),
                NormalizedToolArguments::try_from_provider_text(String::from(
                    r#"{"cmd":"git fetch origin main"}"#,
                ))
                .expect("the fixture arguments are admitted"),
            ),
        )])
        .expect("one proposal forms a tool-using response");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools { response });
    let ModelCallTerminalOutcome::ToolRound(round) = repository
        .apply_terminal_observation(
            session,
            observation,
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![ToolResponsePartIdentity::tool_call(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 7)),
                    request,
                    InitialToolApproval::Delegated,
                )],
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 8)),
                None,
            )),
            |_| panic!("the fixture has no pending steering"),
        )
        .await?
    else {
        panic!("the model call reaches a delegated tool round")
    };
    assert_eq!(
        round.next_phase(),
        &ActiveTurnPhase::AwaitingApproval { request }
    );
    let approval_repository = repository.approval_judge_repository();
    let prepared = ready_approval_judge(
        approval_repository
            .prepare(
                session,
                turn,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 9)),
                Some(DirectModelSelection::from_uuid(Uuid::from_u128(901))),
            )
            .await?,
    );
    Ok((repository, prepared, turn, request))
}

async fn evaluate_second_conflict(
    fixture: &DispatchFixture,
) -> Result<RepoWatchRuleEvaluationOutcome, Box<dyn Error>> {
    Ok(evaluate_conflict(fixture, 102, SECOND_HEAD).await?.outcome)
}

async fn evaluate_conflict(
    fixture: &DispatchFixture,
    event_id: u128,
    head: &str,
) -> Result<EvaluatedConflict, Box<dyn Error>> {
    let (loaded, observation) = load_conflict(fixture, event_id, head).await?;
    let event_id = loaded.id();
    let outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                loaded,
                &fixture.rule,
                &observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    Ok(EvaluatedConflict { outcome, event_id })
}

async fn load_second_conflict(
    fixture: &DispatchFixture,
) -> Result<(RepoWatchEvent, RepoWatchObservation), Box<dyn Error>> {
    load_conflict(fixture, 102, SECOND_HEAD).await
}

async fn load_conflict(
    fixture: &DispatchFixture,
    event_id: u128,
    head: &str,
) -> Result<(RepoWatchEvent, RepoWatchObservation), Box<dyn Error>> {
    let event_store = PostgresRepoWatchStore::new(fixture.pool.clone());
    let cursor = event_store
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    let event = conflict_event(event_id, head)?;
    let observation = observation(context(head)?)?;
    event_store
        .commit(
            &fixture.repository,
            RepoWatchCommitRequest::new(
                Some(cursor.generation()),
                RepoWatchCursorCandidate::new(observation.clone()),
                vec![identified_event(event)],
            ),
        )
        .await?;
    let loaded = fixture
        .store
        .load_next_event(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?
        .expect("second conflict remains unevaluated");
    Ok((loaded, observation))
}

async fn commit_lifecycle(
    fixture: &DispatchFixture,
    observation: RepoWatchObservation,
    event: RepoWatchEvent,
) -> Result<(), Box<dyn Error>> {
    let event_store = PostgresRepoWatchStore::new(fixture.pool.clone());
    let cursor = event_store
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    event_store
        .commit(
            &fixture.repository,
            RepoWatchCommitRequest::new(
                Some(cursor.generation()),
                RepoWatchCursorCandidate::new(observation),
                vec![identified_event(event)],
            ),
        )
        .await?;
    Ok(())
}

async fn commit_merge(fixture: &DispatchFixture, event_id: u128) -> Result<(), Box<dyn Error>> {
    commit_lifecycle(
        fixture,
        lifecycle_observation(context(SECOND_HEAD)?, RepoWatchPullRequestLifecycle::Merged)?,
        merged_event(event_id, SECOND_HEAD)?,
    )
    .await
}

async fn commit_reopen(fixture: &DispatchFixture, event_id: u128) -> Result<(), Box<dyn Error>> {
    commit_lifecycle(
        fixture,
        lifecycle_observation(context(THIRD_HEAD)?, RepoWatchPullRequestLifecycle::Open)?,
        opened_event(event_id, THIRD_HEAD)?,
    )
    .await
}

async fn commit_second_merge(
    fixture: &DispatchFixture,
    event_id: u128,
) -> Result<(), Box<dyn Error>> {
    commit_lifecycle(
        fixture,
        lifecycle_observation(context(THIRD_HEAD)?, RepoWatchPullRequestLifecycle::Merged)?,
        merged_event(event_id, THIRD_HEAD)?,
    )
    .await
}

async fn corrupt_goal_generation(pool: &PgPool, session: SessionId) -> Result<(), Box<dyn Error>> {
    sqlx::query("ALTER TABLE goal_event DISABLE TRIGGER goal_event_is_append_only")
        .execute(pool)
        .await?;
    sqlx::query("UPDATE goal_event SET generation = $2 WHERE session_id = $1")
        .bind(session.as_uuid())
        .bind(CORRUPT_GOAL_GENERATION)
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE goal_event ENABLE TRIGGER goal_event_is_append_only")
        .execute(pool)
        .await?;
    Ok(())
}

async fn evaluate_obligation(
    fixture: &DispatchFixture,
    obligation: RepoWatchDispatchObligation,
    observation: &RepoWatchObservation,
) -> Result<RepoWatchRuleEvaluationOutcome, Box<dyn Error>> {
    let event = obligation.latest_event().clone();
    Ok(RepoWatchDispatchService::new(
        UuidV7RepoWatchDispatchIdGenerator,
        ObligationTransaction {
            store: fixture.store.clone(),
            obligation: Some(obligation),
        },
    )
    .evaluate(
        event,
        &fixture.rule,
        observation,
        &TemplateResolver,
        dispatch_context(),
    )
    .await?)
}

/// A `main` update dispatches its mergeable dependent directly from the
/// `BaseAdvanced` fact, without any check-completion or conflict event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn eager_main_advance_dispatches_its_mergeable_dependent_immediately()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let rule = eager_merge_forward_rule()?;
    let dependent = same_repository_context(SameRepositoryContextFacts {
        number: BOTTOM_PULL_REQUEST_NUMBER,
        head: INITIAL_HEAD,
        base_branch: BASE_BRANCH,
        head_branch: BOTTOM_AGENT_BRANCH,
    })?;
    let initial_observation = mergeable_observation(
        vec![dependent.clone()],
        vec![RepoWatchBranchHead::new(
            dependent.base_branch().clone(),
            CommitSha::try_new(INITIAL_HEAD.to_owned())?,
        )],
    )?;
    let first_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    RepoWatchCursorCandidate::new(initial_observation),
                    vec![identified_event(opened_event_for(
                        MAIN_OPENED_EVENT_ID,
                        dependent.clone(),
                    )?)],
                ),
            )
            .await?,
    );
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let current_observation = mergeable_observation(
        vec![dependent.clone()],
        vec![RepoWatchBranchHead::new(
            dependent.base_branch().clone(),
            CommitSha::try_new(FIRST_HEAD.to_owned())?,
        )],
    )?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(current_observation.clone()),
                vec![identified_event(base_advanced_event(
                    MAIN_BASE_ADVANCED_EVENT_ID,
                    dependent.clone(),
                )?)],
            ),
        )
        .await?;

    let loaded_dependent = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the main-based pull request remains unevaluated");
    assert_eq!(pull_request_number(&loaded_dependent), dependent.number());
    let outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                loaded_dependent,
                &rule,
                &current_observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    let batch_count: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
        .fetch_one(&pool)
        .await?;

    assert!(outcome_is_dispatched(&outcome));
    assert_eq!(batch_count, 1);
    Ok(())
}

/// A stacked parent update dispatches its mergeable child directly. The
/// parent's own `HeadChanged` fact remains nonmatching, so only the dependent
/// receives the merge-forward session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn eager_parent_advance_dispatches_only_its_mergeable_child_immediately()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let rule = eager_merge_forward_rule()?;
    let initial_parent = same_repository_context(SameRepositoryContextFacts {
        number: BOTTOM_PULL_REQUEST_NUMBER,
        head: INITIAL_HEAD,
        base_branch: BASE_BRANCH,
        head_branch: BOTTOM_AGENT_BRANCH,
    })?;
    let child = same_repository_context(SameRepositoryContextFacts {
        number: TOP_PULL_REQUEST_NUMBER,
        head: SECOND_HEAD,
        base_branch: BOTTOM_AGENT_BRANCH,
        head_branch: TOP_AGENT_BRANCH,
    })?;
    let initial_observation = mergeable_observation(
        vec![initial_parent.clone(), child.clone()],
        vec![RepoWatchBranchHead::new(
            initial_parent.head_branch().clone(),
            initial_parent.head_sha().clone(),
        )],
    )?;
    let first_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    RepoWatchCursorCandidate::new(initial_observation),
                    vec![
                        identified_event(opened_event_for(
                            STACK_BOTTOM_OPENED_EVENT_ID,
                            initial_parent.clone(),
                        )?),
                        identified_event(opened_event_for(
                            STACK_TOP_OPENED_EVENT_ID,
                            child.clone(),
                        )?),
                    ],
                ),
            )
            .await?,
    );
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let advanced_parent = same_repository_context(SameRepositoryContextFacts {
        number: BOTTOM_PULL_REQUEST_NUMBER,
        head: FIRST_HEAD,
        base_branch: BASE_BRANCH,
        head_branch: BOTTOM_AGENT_BRANCH,
    })?;
    let current_observation = mergeable_observation(
        vec![advanced_parent.clone(), child.clone()],
        vec![RepoWatchBranchHead::new(
            advanced_parent.head_branch().clone(),
            advanced_parent.head_sha().clone(),
        )],
    )?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(current_observation.clone()),
                vec![
                    identified_event(head_changed_event(
                        STACK_PARENT_HEAD_CHANGED_EVENT_ID,
                        advanced_parent.clone(),
                        INITIAL_HEAD,
                    )?),
                    identified_event(base_advanced_event(
                        STACK_BASE_ADVANCED_EVENT_ID,
                        child.clone(),
                    )?),
                ],
            ),
        )
        .await?;

    let loaded_parent = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the activated eager rule sees the parent head change");
    assert_eq!(
        pull_request_number(&loaded_parent),
        advanced_parent.number()
    );
    let parent_outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                loaded_parent,
                &rule,
                &current_observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;

    let loaded_child = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the stacked child remains unevaluated");
    assert_eq!(pull_request_number(&loaded_child), child.number());
    let child_outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                loaded_child,
                &rule,
                &current_observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    let batch_count: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
        .fetch_one(&pool)
        .await?;

    assert_eq!(parent_outcome, RepoWatchRuleEvaluationOutcome::NotMatched);
    assert!(outcome_is_dispatched(&child_outcome));
    assert_eq!(batch_count, 1);
    Ok(())
}

/// A user stop retires a queued dispatch turn without changing its physical
/// lifecycle state, so release follows runtime relevance rather than waiting
/// for a terminal state transition that will never happen.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stopped_runtime_irrelevant_turn_releases_its_singleton() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;

    withdraw_dispatched_goal(&fixture.pool, fixture.session(0), 0x50_100).await?;

    assert_eq!(release_count(&fixture).await?, 1);
    Ok(())
}

/// The current taxonomy has no separate stale-dispatch terminal variant: an
/// operator stop after stale classification is the durable `user_stopped`
/// transition. It must retain work before its singleton becomes reusable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stale_stopped_dispatch_requeues_and_redispatches_after_release()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    withdraw_dispatched_goal(&fixture.pool, fixture.session(0), 0x50_200).await?;
    let obligation = fixture
        .store
        .load_next_dispatch_obligation(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?
        .expect("the stopped dispatch obligation is ready after release");
    let cursor = PostgresRepoWatchStore::new(fixture.pool.clone())
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");

    assert_eq!(release_count(&fixture).await?, 1);
    assert_eq!(obligation.latest_event(), &fixture.event);
    let (_successor_dispatch, successor_sessions) = dispatched(
        evaluate_obligation(&fixture, obligation, cursor.candidate().observation()).await?,
    );
    assert_ne!(successor_sessions[0], fixture.session(0));
    Ok(())
}

/// INV-REPO-WATCH-HEADLESS-APPROVAL-ESCALATION-REARMS: a completed judge
/// escalation in a repository-watch-created session cannot retain the
/// singleton as unattended active work. Its normal failed-turn/blocked-goal
/// closeout releases the dispatch, leaves an auditable escalation record, and
/// makes the same event eligible for a fresh dispatch.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn headless_approval_escalation_releases_rearms_and_redispatches()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let seed = 0x50_240;
    let (model_repository, prepared, turn, request) =
        checkpoint_dispatched_delegated_approval(&fixture, seed).await?;
    let authority = prepared_pull_request_authority(&prepared);
    let expected_context = context(FIRST_HEAD)?;
    let approval_repository = model_repository.approval_judge_repository();
    approval_repository.authorize(&prepared).await?;
    let rationale = ToolDecisionRationale::try_new(String::from(
        "the provider requests authority beyond the immutable dispatch fence",
    ))?;

    let outcome = approval_repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::EscalateToHuman,
            rationale.clone(),
            ProviderReportedTokenUsage::unreported(),
            ApprovalJudgeCompletionIdentities::new(
                TurnAttemptId::from_uuid(Uuid::from_u128(seed + 10)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 11)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 12)),
            ),
            |closed_request| {
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    closed_request.as_uuid().as_u128() + 0x2_000_000,
                ))
            },
        )
        .await?;
    let audit: (
        String,
        Option<String>,
        String,
        Option<String>,
        String,
        bool,
        bool,
    ) = sqlx::query_as(
        "SELECT lifecycle.state_kind,
                    lifecycle.terminal_disposition_kind,
                    latest_goal.event_kind,
                    latest_goal.blocked_reason,
                    audit.rationale,
                    audit.released_at IS NOT NULL,
                    audit.obligation_id IS NOT NULL
               FROM repo_watch_headless_approval_escalation_audit AS audit
               JOIN turn_lifecycle AS lifecycle
                 ON lifecycle.session_id = audit.session_id
                AND lifecycle.turn_id = audit.turn_id
               JOIN LATERAL (
                    SELECT event_kind, blocked_reason
                      FROM goal_event
                     WHERE session_id = audit.session_id
                     ORDER BY event_ordinal DESC
                     LIMIT 1
               ) AS latest_goal ON true
              WHERE audit.model_call_id = $1",
    )
    .bind(prepared.call().as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let obligation = fixture
        .store
        .load_next_dispatch_obligation(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?
        .expect("the headless escalation obligation is ready after release");
    let cursor = PostgresRepoWatchStore::new(fixture.pool.clone())
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    let (_successor_dispatch, successor_sessions) = dispatched(
        evaluate_obligation(&fixture, obligation, cursor.candidate().observation()).await?,
    );

    assert_eq!(authority.dispatch(), fixture.dispatch_id);
    assert_eq!(authority.repository(), &fixture.repository);
    assert_eq!(authority.pull_request(), expected_context.number());
    assert_eq!(authority.head_sha(), expected_context.head_sha());
    assert_eq!(
        authority.head_repository(),
        expected_context.head_repository()
    );
    assert_eq!(authority.head_branch(), expected_context.head_branch());
    assert_eq!(authority.base_branch(), expected_context.base_branch());
    assert_eq!(
        outcome,
        CompleteApprovalJudgeOutcome::HeadlessEscalationReleased
    );
    assert_eq!(audit.0, "terminal");
    assert_eq!(audit.1.as_deref(), Some("failed"));
    assert_eq!(audit.2, "blocked");
    assert_eq!(audit.3.as_deref(), Some("execution_failure"));
    assert_eq!(audit.4, rationale.as_str());
    assert!(audit.5);
    assert!(audit.6);
    assert_ne!(successor_sessions[0], fixture.session(0));
    assert_eq!(prepared.request().id(), request);
    assert_eq!(turn, prepared.request().turn());
    Ok(())
}

/// Restart recovery classifies an invalidated dispatched turn through the
/// existing unsuccessful-turn / execution-failure goal path. Reconstructing
/// the store proves the obligation is durable rather than process-local.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn restart_invalidated_dispatch_recovery_leaves_an_obligation() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let session = fixture.session(0);
    let turn = TurnId::from_uuid(
        sqlx::query_scalar::<_, Uuid>(
            "SELECT turn_id FROM repo_watch_dispatch_delivery WHERE dispatch_id = $1",
        )
        .bind(fixture.dispatch_id.as_uuid())
        .fetch_one(&fixture.pool)
        .await?,
    );
    mark_queued_turn_failed(&fixture.pool, session, turn, 0x50_300).await?;
    check_completed_turn_for_release(&fixture.pool, session, turn).await?;
    assert_applied_goal_transition(
        GoalRepository::new(fixture.pool.clone())
            .block_execution_failure(
                session,
                GoalNeed::try_new(String::from("recover the invalidated dispatch"))
                    .expect("fixture goal need is valid"),
                GoalSchedulerProvenance::new(turn),
            )
            .await?,
    );
    let resumed_store = PostgresRepoWatchDispatchStore::new(fixture.pool.clone(), credential_pin());
    let obligation = resumed_store
        .load_next_dispatch_obligation(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?
        .expect("restart recovery retains the invalidated dispatch obligation");

    assert_eq!(obligation.latest_event(), &fixture.event);
    assert_eq!(obligation.matched_event_count(), 1);
    Ok(())
}

/// A batch admitted before the delivered state was recorded has none, and
/// achievement must not seal from its originating event: a head that returns to
/// an earlier value would then compare equal and seal without ever delivering
/// the state the session was given.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_unknown_delivered_state_never_seals() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    sqlx::query(
        "ALTER TABLE repo_watch_dispatch_batch
             DISABLE TRIGGER repo_watch_dispatch_batch_is_append_only",
    )
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        "UPDATE repo_watch_dispatch_batch
            SET delivered_state_event_id = NULL
          WHERE dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        "ALTER TABLE repo_watch_dispatch_batch
             ENABLE TRIGGER repo_watch_dispatch_batch_is_append_only",
    )
    .execute(&fixture.pool)
    .await?;
    declare_dispatched_goal_achieved(&fixture, 0, UNKNOWN_DELIVERED_REQUEST_ID).await?;

    assert_eq!(release_count(&fixture).await?, 1);
    assert_eq!(
        outstanding_obligation_count(&fixture.pool).await?,
        1,
        "the head is unchanged, so an originating-event fallback would have sealed"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn current_head_achievement_seals_without_requeue() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    declare_dispatched_goal_achieved(&fixture, 0, 0x50_400).await?;
    let obligations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_obligation")
            .fetch_one(&fixture.pool)
            .await?;

    assert_eq!(release_count(&fixture).await?, 1);
    assert_eq!(obligations, 0);
    Ok(())
}

/// A merge-forward dispatch moves the head it was dispatched against, so the
/// exact-head seal must compare the state a batch delivered rather than the
/// event that originated it. The obligation successor replays that still-
/// matching earlier event over collapsed current state; comparing against the
/// stale originating head would owe another batch after every cooldown forever.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn achievement_after_a_head_change_requeues_once_and_then_seals() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let event_store = PostgresRepoWatchStore::new(fixture.pool.clone());
    let cursor = event_store
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    let advanced = observation(context(SECOND_HEAD)?)?;
    event_store
        .commit(
            &fixture.repository,
            RepoWatchCommitRequest::new(
                Some(cursor.generation()),
                RepoWatchCursorCandidate::new(advanced.clone()),
                vec![identified_event(head_changed_event(
                    SUPERSEDING_HEAD_EVENT_ID,
                    context(SECOND_HEAD)?,
                    FIRST_HEAD,
                )?)],
            ),
        )
        .await?;
    declare_dispatched_goal_achieved(&fixture, 0, SUPERSEDED_ACHIEVEMENT_REQUEST_ID).await?;
    let obligation = fixture
        .store
        .load_next_dispatch_obligation(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?
        .expect("achievement against a superseded head owes one current-state batch");
    let (_successor, successor_sessions) =
        dispatched(evaluate_obligation(&fixture, obligation, &advanced).await?);
    declare_session_goal_achieved(
        &fixture.pool,
        successor_sessions[0],
        SUCCESSOR_ACHIEVEMENT_REQUEST_ID,
    )
    .await?;

    let releases: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_release")
        .fetch_one(&fixture.pool)
        .await?;

    assert_eq!(
        releases, 2,
        "both the original batch and its successor release"
    );
    assert_eq!(dispatched_obligation_count(&fixture.pool).await?, 1);
    assert_eq!(
        outstanding_obligation_count(&fixture.pool).await?,
        0,
        "the successor already carried the current head, so its achievement seals"
    );
    Ok(())
}

/// The released-batch guard only excludes successor generations once the whole
/// batch releases. While a sibling action still holds it, an achieved session
/// may accept an unrelated successor goal, so the terminal event that decides
/// the requeue is the one ending the generation the dispatch commissioned.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_successor_goal_beside_a_pursuing_sibling_owes_nothing() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let session = fixture.session(0);
    declare_dispatched_goal_achieved(&fixture, 0, SIBLING_ACHIEVEMENT_REQUEST_ID).await?;
    assert_applied_goal_command(
        GoalRepository::new(fixture.pool.clone())
            .handle_user_command(
                GoalUserCommand::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(SIBLING_ATTACH_COMMAND_ID)),
                    session,
                    GoalUserAction::Attach(GoalStatement::try_new(String::from(
                        "an unrelated successor goal beside a pursuing sibling",
                    ))?),
                ),
                Some(GoalTurnCandidates::new(
                    AcceptedInputId::from_uuid(Uuid::from_u128(SIBLING_GOAL_INPUT_ID)),
                    TurnId::from_uuid(Uuid::from_u128(SIBLING_GOAL_TURN_ID)),
                )),
                |_| None,
            )
            .await?,
    );
    withdraw_dispatched_goal(&fixture.pool, session, SIBLING_STOP_COMMAND_ID).await?;

    assert_eq!(
        release_count(&fixture).await?,
        0,
        "the pursuing sibling still holds the batch"
    );
    assert_eq!(outstanding_obligation_count(&fixture.pool).await?, 0);
    Ok(())
}

/// A released batch has already accounted for its dispatched work. Its session
/// may afterwards accept an unrelated successor goal, whose own termination
/// reaches the release trigger through the same action link; owing a requeue for
/// that generation would redispatch an event that already converged.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_successor_goal_on_a_released_dispatch_owes_nothing() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let session = fixture.session(0);
    declare_dispatched_goal_achieved(&fixture, 0, RELEASED_ACHIEVEMENT_REQUEST_ID).await?;
    assert_applied_goal_command(
        GoalRepository::new(fixture.pool.clone())
            .handle_user_command(
                GoalUserCommand::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(SUCCESSOR_GOAL_ATTACH_COMMAND_ID)),
                    session,
                    GoalUserAction::Attach(GoalStatement::try_new(String::from(
                        "an unrelated successor goal for this session",
                    ))?),
                ),
                Some(GoalTurnCandidates::new(
                    AcceptedInputId::from_uuid(Uuid::from_u128(SUCCESSOR_GOAL_INPUT_ID)),
                    TurnId::from_uuid(Uuid::from_u128(SUCCESSOR_GOAL_TURN_ID)),
                )),
                |_| None,
            )
            .await?,
    );
    withdraw_dispatched_goal(&fixture.pool, session, SUCCESSOR_GOAL_STOP_COMMAND_ID).await?;

    assert_eq!(release_count(&fixture).await?, 1);
    assert_eq!(outstanding_obligation_count(&fixture.pool).await?, 0);
    Ok(())
}

/// A branch target records a workflow conclusion and no durable revision, so no
/// head comparison can ever seal it. Achievement is its own seal there; the
/// pull-request-only comparison would redeliver every successful branch
/// dispatch after each cooldown.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn achieved_branch_dispatch_seals_without_requeue() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let rule = branch_workflow_rule()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let first_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    RepoWatchCursorCandidate::new(observation(context(INITIAL_HEAD)?)?),
                    vec![identified_event(opened_event(
                        BRANCH_ACTIVATION_EVENT_ID,
                        INITIAL_HEAD,
                    )?)],
                ),
            )
            .await?,
    );
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let branch = branch_observation()?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(branch.clone()),
                vec![identified_event(branch_workflow_event(
                    BRANCH_WORKFLOW_EVENT_ID,
                )?)],
            ),
        )
        .await?;
    let loaded = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the branch rule sees its workflow event");
    let (_dispatch, sessions) = dispatched(
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                loaded,
                &rule,
                &branch,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?,
    );
    declare_session_goal_achieved(&pool, sessions[0], BRANCH_ACHIEVEMENT_REQUEST_ID).await?;
    let releases: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_release")
        .fetch_one(&pool)
        .await?;

    assert_eq!(releases, 1);
    assert_eq!(outstanding_obligation_count(&pool).await?, 0);
    Ok(())
}

/// The requeue a terminal-event dispatch owes lasts only while its own cutoff
/// is the latest lifecycle. A reopen makes the close obsolete, and requeueing it
/// would run close automation against an open pull request.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn reopening_a_pull_request_drops_a_stopped_close_dispatch_requeue()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let rule = closed_event_rule()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let first_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    RepoWatchCursorCandidate::new(observation(context(INITIAL_HEAD)?)?),
                    vec![identified_event(opened_event(
                        REOPENED_CLOSE_OPENED_EVENT_ID,
                        INITIAL_HEAD,
                    )?)],
                ),
            )
            .await?,
    );
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let closed =
        lifecycle_observation(context(SECOND_HEAD)?, RepoWatchPullRequestLifecycle::Closed)?;
    let second_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    Some(first_generation),
                    RepoWatchCursorCandidate::new(closed.clone()),
                    vec![identified_event(closed_event(
                        REOPENED_CLOSE_CLOSED_EVENT_ID,
                        SECOND_HEAD,
                    )?)],
                ),
            )
            .await?,
    );
    let loaded = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the close-event rule sees the close event");
    let (_dispatch, sessions) = dispatched(
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                loaded,
                &rule,
                &closed,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?,
    );
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(second_generation),
                RepoWatchCursorCandidate::new(observation(context(THIRD_HEAD)?)?),
                vec![identified_event(opened_event(
                    REOPENED_CLOSE_REOPENED_EVENT_ID,
                    THIRD_HEAD,
                )?)],
            ),
        )
        .await?;
    withdraw_dispatched_goal(&pool, sessions[0], REOPENED_CLOSE_STOP_COMMAND_ID).await?;

    assert_eq!(outstanding_obligation_count(&pool).await?, 0);
    Ok(())
}

/// A rule may match the close or merge event itself, and that dispatch is the
/// cutoff fact rather than work the cutoff invalidated. Its non-converged
/// termination therefore still owes a requeue, even though the pull request's
/// latest lifecycle is terminal.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stopped_terminal_event_dispatch_keeps_its_requeue() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let rule = merged_event_rule()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let first_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    RepoWatchCursorCandidate::new(observation(context(INITIAL_HEAD)?)?),
                    vec![identified_event(opened_event(
                        STOPPED_TERMINAL_OPENED_EVENT_ID,
                        INITIAL_HEAD,
                    )?)],
                ),
            )
            .await?,
    );
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let merged =
        lifecycle_observation(context(SECOND_HEAD)?, RepoWatchPullRequestLifecycle::Merged)?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(merged.clone()),
                vec![identified_event(merged_event(
                    STOPPED_TERMINAL_MERGED_EVENT_ID,
                    SECOND_HEAD,
                )?)],
            ),
        )
        .await?;
    let loaded = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the terminal-event rule sees the merge event");
    let (_dispatch, sessions) = dispatched(
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                loaded,
                &rule,
                &merged,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?,
    );
    withdraw_dispatched_goal(&pool, sessions[0], STOPPED_TERMINAL_STOP_COMMAND_ID).await?;

    assert_eq!(outstanding_obligation_count(&pool).await?, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn multiple_terminations_collapse_into_the_latest_state_obligation()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    withdraw_dispatched_goal(&fixture.pool, fixture.session(0), 0x50_500).await?;
    let first: OutstandingTerminationVisibility = sqlx::query_as(
        "SELECT obligation_id, latest_event_id, matched_event_count, ready
           FROM repo_watch_outstanding_dispatch_obligation",
    )
    .fetch_one(&fixture.pool)
    .await?;
    let latest = evaluate_conflict(&fixture, 102, SECOND_HEAD).await?;
    withdraw_dispatched_goal(&fixture.pool, fixture.session(1), 0x50_600).await?;
    let collapsed: OutstandingTerminationVisibility = sqlx::query_as(
        "SELECT obligation_id, latest_event_id, matched_event_count, ready
           FROM repo_watch_outstanding_dispatch_obligation",
    )
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(first.latest_event_id, *fixture.event.id().as_uuid());
    assert_eq!(first.matched_event_count, 1);
    assert!(!first.ready);
    assert_eq!(latest.outcome, RepoWatchRuleEvaluationOutcome::Occupied);
    assert_eq!(collapsed.obligation_id, first.obligation_id);
    assert_eq!(collapsed.latest_event_id, *latest.event_id.as_uuid());
    assert_eq!(collapsed.matched_event_count, 2);
    assert!(collapsed.ready);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn held_slot_projection_names_each_failed_release_clause() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;

    let held: HeldSlotVisibility = sqlx::query_as(
        "SELECT every_action_delivered, every_delivery_turn_releasable,
                no_live_runtime_turn, every_goal_nonpursuing, blockers
           FROM repo_watch_held_dispatch_slot
          WHERE dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert!(held.every_action_delivered);
    assert!(!held.every_delivery_turn_releasable);
    assert!(!held.no_live_runtime_turn);
    assert!(!held.every_goal_nonpursuing);
    assert_eq!(
        held.blockers,
        vec![
            String::from("delivery_turn_runtime_relevant"),
            String::from("live_runtime_turn"),
            String::from("pursuing_goal"),
        ]
    );
    Ok(())
}

/// A terminal pull-request lifecycle withdraws only the generation-one goals
/// repository watch commissioned for that pull request.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn merged_pull_request_ends_the_commissioned_goal() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let session = fixture.session(0);
    commit_merge(&fixture, MERGED_GOAL_CUTOFF_EVENT_ID).await?;

    let processed = fixture
        .store
        .process_next_lifecycle_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x51_100))
        })
        .await?;
    let replayed = fixture
        .store
        .process_next_lifecycle_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x51_100))
        })
        .await?;

    let goal = GoalRepository::new(fixture.pool.clone())
        .load_goal(session)
        .await?
        .expect("the dispatched goal remains readable");
    let cutoff_goal_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_lifecycle_cutoff_goal
          WHERE session_id = $1",
    )
    .bind(session.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert!(processed);
    assert!(!replayed);
    assert_eq!(goal.current().state(), &GoalState::UserStopped);
    assert_eq!(cutoff_goal_count, 1);
    assert_eq!(release_count(&fixture).await?, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_cutoff_cleans_dispatches_from_later_same_observation_facts()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(cooldown_rule()?).await?;
    let session = fixture.session(0);
    let occupied = evaluate_conflict(&fixture, 102, SECOND_HEAD).await?;
    assert_eq!(occupied.outcome, RepoWatchRuleEvaluationOutcome::Occupied);
    commit_merge(&fixture, MERGED_GOAL_CUTOFF_EVENT_ID).await?;
    sqlx::query("ALTER TABLE repo_watch_event DISABLE TRIGGER ALL")
        .execute(&fixture.pool)
        .await?;
    sqlx::query(
        "WITH cutoff AS (
             SELECT cursor_generation
               FROM repo_watch_event
              WHERE event_id = $1
         ), ordered AS (
             SELECT event_id,
                    row_number() OVER (ORDER BY event_id) + 1 AS event_ordinal
               FROM repo_watch_event
              WHERE event_id IN (
                    SELECT event_id
                      FROM repo_watch_dispatch_action
                     WHERE session_id = $2
                    UNION ALL
                    SELECT $3
              )
         )
         UPDATE repo_watch_event AS event
            SET cursor_generation = cutoff.cursor_generation,
                event_ordinal = ordered.event_ordinal
           FROM cutoff, ordered
          WHERE event.event_id = ordered.event_id",
    )
    .bind(Uuid::from_u128(MERGED_GOAL_CUTOFF_EVENT_ID))
    .bind(session.as_uuid())
    .bind(occupied.event_id.as_uuid())
    .execute(&fixture.pool)
    .await?;
    sqlx::query("ALTER TABLE repo_watch_event ENABLE TRIGGER ALL")
        .execute(&fixture.pool)
        .await?;

    fixture
        .store
        .process_next_lifecycle_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x51_105))
        })
        .await?;

    let goal = GoalRepository::new(fixture.pool.clone())
        .load_goal(session)
        .await?
        .expect("the stale dispatch goal remains readable");
    let settlement: String = sqlx::query_scalar(
        "SELECT settled_kind
           FROM repo_watch_dispatch_obligation
          WHERE latest_event_id = $1",
    )
    .bind(occupied.event_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(goal.current().state(), &GoalState::UserStopped);
    assert_eq!(settlement, "target_closed");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn close_reopen_close_classifies_each_cutoff_against_its_following_lifecycle()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let first_merge_id = 0x51_110;
    let reopened_id = 0x51_120;
    let second_merge_id = 0x51_130;
    commit_merge(&fixture, first_merge_id).await?;
    commit_reopen(&fixture, reopened_id).await?;
    commit_second_merge(&fixture, second_merge_id).await?;

    let first_processed = fixture
        .store
        .process_next_lifecycle_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x51_140))
        })
        .await?;
    let second_processed = fixture
        .store
        .process_next_lifecycle_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x51_150))
        })
        .await?;
    let cutoffs: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT event_id, disposition_kind
           FROM repo_watch_lifecycle_cutoff
          ORDER BY event_id",
    )
    .fetch_all(&fixture.pool)
    .await?;

    assert!(first_processed);
    assert!(second_processed);
    assert_eq!(
        cutoffs,
        vec![
            (Uuid::from_u128(first_merge_id), String::from("reopened")),
            (Uuid::from_u128(second_merge_id), String::from("terminal")),
        ]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn startup_drain_continues_after_corrupt_goal_cutoff() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    corrupt_goal_generation(&fixture.pool, fixture.session(0)).await?;
    commit_merge(&fixture, CORRUPT_GOAL_FIRST_CUTOFF_EVENT_ID).await?;
    commit_second_merge(&fixture, CORRUPT_GOAL_SECOND_CUTOFF_EVENT_ID).await?;

    fixture
        .store
        .process_pending_lifecycle_cutoffs(|| DurableCommandId::from_uuid(Uuid::now_v7()))
        .await?;
    let cutoff_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_lifecycle_cutoff",
    )
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(cutoff_count, 2);
    Ok(())
}

/// Dispatch admission rechecks durable lifecycle after an event was loaded, so
/// a merge committed in between prevents the stale match from firing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn matching_event_loaded_before_merge_records_target_closed() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let (loaded, stale_open_observation) = load_second_conflict(&fixture).await?;
    let batches_before: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
        .fetch_one(&fixture.pool)
        .await?;
    commit_merge(&fixture, 0x52_000).await?;

    let outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                loaded,
                &fixture.rule,
                &stale_open_observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;

    assert_eq!(outcome, RepoWatchRuleEvaluationOutcome::TargetClosed);
    let batches_after: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
        .fetch_one(&fixture.pool)
        .await?;
    assert_eq!(batches_after, batches_before);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn matching_merged_event_dispatch_survives_its_lifecycle_cutoff() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let rule = merged_event_rule()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let initial = observation(context(INITIAL_HEAD)?)?;
    let first_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    RepoWatchCursorCandidate::new(initial),
                    vec![identified_event(opened_event(
                        TERMINAL_RULE_OPENED_EVENT_ID,
                        INITIAL_HEAD,
                    )?)],
                ),
            )
            .await?,
    );
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let merged =
        lifecycle_observation(context(SECOND_HEAD)?, RepoWatchPullRequestLifecycle::Merged)?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(merged.clone()),
                vec![identified_event(merged_event(
                    TERMINAL_RULE_MERGED_EVENT_ID,
                    SECOND_HEAD,
                )?)],
            ),
        )
        .await?;
    let loaded = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the terminal-event rule sees the merge event");
    let outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                loaded,
                &rule,
                &merged,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    let (_, sessions) = dispatched(outcome);
    let session = sessions[0];
    let cutoff_processed = dispatch_store
        .process_next_lifecycle_cutoff(&repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x54_200))
        })
        .await?;
    let goal = GoalRepository::new(pool.clone())
        .load_goal(session)
        .await?
        .expect("the terminal-event dispatch goal remains readable");
    let cutoff_goal_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_lifecycle_cutoff_goal
          WHERE session_id = $1",
    )
    .bind(session.as_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(sessions.len(), rule.actions().len());
    assert!(cutoff_processed);
    assert_eq!(goal.current().state(), &GoalState::Pursuing);
    assert_eq!(cutoff_goal_count, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn merged_event_before_a_later_terminal_cutoff_records_target_closed()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let rule = merged_event_rule()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool, credential_pin());
    let initial = observation(context(INITIAL_HEAD)?)?;
    let first_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    RepoWatchCursorCandidate::new(initial),
                    vec![identified_event(opened_event(0x54_300, INITIAL_HEAD)?)],
                ),
            )
            .await?,
    );
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let first_merged =
        lifecycle_observation(context(SECOND_HEAD)?, RepoWatchPullRequestLifecycle::Merged)?;
    let second_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    Some(first_generation),
                    RepoWatchCursorCandidate::new(first_merged),
                    vec![identified_event(merged_event(0x54_310, SECOND_HEAD)?)],
                ),
            )
            .await?,
    );
    let reopened =
        lifecycle_observation(context(THIRD_HEAD)?, RepoWatchPullRequestLifecycle::Open)?;
    let third_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    Some(second_generation),
                    RepoWatchCursorCandidate::new(reopened),
                    vec![identified_event(opened_event(0x54_320, THIRD_HEAD)?)],
                ),
            )
            .await?,
    );
    let second_merged =
        lifecycle_observation(context(THIRD_HEAD)?, RepoWatchPullRequestLifecycle::Merged)?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(third_generation),
                RepoWatchCursorCandidate::new(second_merged.clone()),
                vec![identified_event(merged_event(0x54_330, THIRD_HEAD)?)],
            ),
        )
        .await?;
    let first_cutoff = dispatch_store
        .process_next_lifecycle_cutoff(&repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x54_340))
        })
        .await?;
    let second_cutoff = dispatch_store
        .process_next_lifecycle_cutoff(&repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x54_350))
        })
        .await?;
    let loaded = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the older merge remains unevaluated");
    let outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                loaded,
                &rule,
                &second_merged,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;

    assert!(first_cutoff);
    assert!(second_cutoff);
    assert_eq!(outcome, RepoWatchRuleEvaluationOutcome::TargetClosed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_target_settles_owed_work_without_dispatch() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(cooldown_rule()?).await?;
    let _occupied = evaluate_second_conflict(&fixture).await?;
    commit_merge(&fixture, 0x53_000).await?;
    fixture
        .store
        .process_next_lifecycle_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x53_100))
        })
        .await?;
    let pending_obligation = fixture
        .store
        .load_next_dispatch_obligation(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?;
    let settlement: String = sqlx::query_scalar(
        "SELECT settled_kind
           FROM repo_watch_dispatch_obligation",
    )
    .fetch_one(&fixture.pool)
    .await?;

    assert!(pending_obligation.is_none());
    assert_eq!(settlement, "target_closed");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_cutoff_preserves_an_obligation_for_its_own_event() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(conflict_and_merged_event_rule()?).await?;
    commit_merge(&fixture, 0x53_200).await?;
    let merged =
        lifecycle_observation(context(SECOND_HEAD)?, RepoWatchPullRequestLifecycle::Merged)?;
    let loaded = fixture
        .store
        .load_next_event(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?
        .expect("the terminal event remains unevaluated");
    let outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                loaded,
                &fixture.rule,
                &merged,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    let cutoff_processed = fixture
        .store
        .process_next_lifecycle_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x53_210))
        })
        .await?;
    let obligation_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_dispatch_obligation",
    )
    .fetch_one(&fixture.pool)
    .await?;
    let settled_kind: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT settled_kind
           FROM repo_watch_dispatch_obligation",
    )
    .fetch_optional(&fixture.pool)
    .await?
    .flatten();

    assert_eq!(outcome, RepoWatchRuleEvaluationOutcome::Occupied);
    assert!(cutoff_processed);
    assert_eq!(obligation_count, 1);
    assert_eq!(settled_kind, None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn dispatch_batch_creates_every_session_and_audit_row_atomically()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let action_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_watch_dispatch_action WHERE dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let expected_action_count = fixture.rule.actions().len();

    assert_eq!(fixture.sessions.len(), expected_action_count);
    assert_eq!(usize::try_from(action_count)?, expected_action_count);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn equal_event_rule_recovery_replays_the_original_sessions() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let replay =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                fixture.event,
                &fixture.rule,
                &fixture.observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;

    assert_eq!(
        replay,
        RepoWatchRuleEvaluationOutcome::Replayed {
            dispatch_id: fixture.dispatch_id,
            sessions: fixture.sessions,
        }
    );
    Ok(())
}

/// The goal a dispatch synthesizes is committed with the session itself.
///
/// A dispatched session declares nothing about itself, so without this it
/// reaches its first turn with no statement of the authority it was created
/// under, and every consumer that reads session authority — the approval judge
/// above all — has nothing to read.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn dispatched_sessions_are_commissioned_with_their_synthesized_goal()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let expected = synthesized_dispatch_goal(&fixture)?;

    assert_eq!(fixture.sessions.len(), fixture.rule.actions().len());
    assert_commissioned_with(&fixture, fixture.session(0), &expected).await?;
    assert_commissioned_with(&fixture, fixture.session(1), &expected).await?;
    Ok(())
}

/// The dispatched work turn carries the tagged context through submit-input,
/// and the commission that follows records that same turn as its generation's
/// goal turn. This is what lets a consumer read the authority the dispatched
/// work ran under from the turn itself.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_dispatched_work_turn_is_its_generations_goal_turn() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;

    let work_turns_bound_to_a_goal: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_dispatch_delivery AS delivery
           JOIN goal_turn
             ON goal_turn.turn_id = delivery.turn_id
            AND goal_turn.accepted_input_id = delivery.accepted_input_id
          WHERE delivery.dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let expected_action_count = fixture.rule.actions().len();

    assert_eq!(
        usize::try_from(work_turns_bound_to_a_goal)?,
        expected_action_count
    );
    Ok(())
}

/// Commissioning adopts the tagged-context turn instead of scheduling one of
/// its own, so one dispatched event queues exactly one turn and runs its
/// template once. A second queued turn would run that template again against
/// the statement alone once the first terminalized.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_dispatched_session_commits_exactly_one_queued_turn() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;

    assert_eq!(queued_turn_count(&fixture, fixture.session(0)).await?, 1);
    assert_eq!(queued_turn_count(&fixture, fixture.session(1)).await?, 1);
    Ok(())
}

async fn queued_turn_count(
    fixture: &DispatchFixture,
    session: SessionId,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM turn_lifecycle
          WHERE session_id = $1 AND state_kind = 'queued'",
    )
    .bind(session.as_uuid())
    .fetch_one(&fixture.pool)
    .await
}

fn synthesized_dispatch_goal(fixture: &DispatchFixture) -> Result<GoalStatement, Box<dyn Error>> {
    let actions = fixture.rule.actions_for_event(&fixture.event)?;
    // One variant, so this destructuring is irrefutable rather than a branch.
    let RepoWatchActionV1::DispatchSession(action) = &actions[0];
    Ok(action.synthesized_goal_statement(fixture.rule.id())?)
}

async fn assert_commissioned_with(
    fixture: &DispatchFixture,
    session: SessionId,
    expected: &GoalStatement,
) -> Result<(), Box<dyn Error>> {
    let goal = GoalRepository::new(fixture.pool.clone())
        .load_goal(session)
        .await?
        .ok_or("a dispatched session is commissioned when it is created")?;

    assert_eq!(goal.current().statement(), expected);
    assert_eq!(goal.generations().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn dispatched_sessions_commit_their_initial_context_and_queued_turn_atomically()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let delivery_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_watch_dispatch_delivery WHERE dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let queued_context_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_dispatch_delivery AS delivery
           JOIN turn_lifecycle AS turn ON turn.turn_id = delivery.turn_id
           JOIN submit_input_command AS command
             ON command.command_id = delivery.submit_command_id
          WHERE delivery.dispatch_id = $1
            AND turn.state_kind = 'queued'
            AND command.content_text = $2",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .bind(DISPATCH_CONTEXT)
    .fetch_one(&fixture.pool)
    .await?;
    let expected_action_count = fixture.rule.actions().len();

    assert_eq!(usize::try_from(delivery_count)?, expected_action_count);
    assert_eq!(
        usize::try_from(queued_context_count)?,
        expected_action_count
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn retired_rule_identity_cannot_resume_from_its_old_activation() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture().await?;
    fixture
        .store
        .reconcile_rules(&fixture.repository, &[])
        .await?;
    let error = fixture
        .store
        .reconcile_rules(&fixture.repository, std::slice::from_ref(&fixture.rule))
        .await
        .expect_err("retired rule identity must not reactivate");

    assert_eq!(
        admission_refusal(&error),
        RuleAdmissionRefusal::ReusedIdentity
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn removed_repository_deactivates_its_rule_identities() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let no_configured_repositories = [];
    fixture
        .store
        .deactivate_unconfigured_repositories(&no_configured_repositories)
        .await?;
    let error = fixture
        .store
        .reconcile_rules(&fixture.repository, std::slice::from_ref(&fixture.rule))
        .await
        .expect_err("a rule from a removed repository must be retired");

    assert_eq!(
        admission_refusal(&error),
        RuleAdmissionRefusal::ReusedIdentity
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn startup_drain_processes_cutoff_after_repository_removal() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let session = fixture.session(0);
    commit_merge(&fixture, STARTUP_DRAIN_CUTOFF_EVENT_ID).await?;
    fixture
        .store
        .deactivate_unconfigured_repositories(&[])
        .await?;

    fixture
        .store
        .process_pending_lifecycle_cutoffs(|| {
            DurableCommandId::from_uuid(Uuid::from_u128(STARTUP_DRAIN_STOP_COMMAND_ID))
        })
        .await?;

    let goal = GoalRepository::new(fixture.pool.clone())
        .load_goal(session)
        .await?
        .expect("the removed repository goal remains readable");
    assert_eq!(goal.current().state(), &GoalState::UserStopped);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn active_rule_identity_names_the_matcher_field_changed_without_a_version_bump()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let changed_rule = merged_event_rule()?;
    let error = fixture
        .store
        .reconcile_rules(&fixture.repository, std::slice::from_ref(&changed_rule))
        .await
        .expect_err("active rule semantics require a new identity");

    assert_eq!(
        admission_refusal(&error),
        RuleAdmissionRefusal::ChangedField(RepoWatchRuleIdentityField::MatcherEventKinds)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn active_activation_without_field_fingerprints_is_storage_corruption()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    remove_field_fingerprints(&fixture).await?;

    let error = fixture
        .store
        .reconcile_rules(&fixture.repository, std::slice::from_ref(&fixture.rule))
        .await
        .expect_err("an active activation without fingerprints is not a tolerated shape");

    assert_eq!(
        admission_refusal(&error),
        RuleAdmissionRefusal::StorageCorruption
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn retiring_a_removed_repository_validates_its_field_fingerprints()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    remove_field_fingerprints(&fixture).await?;

    let error = fixture
        .store
        .deactivate_unconfigured_repositories(&[])
        .await
        .expect_err("a removed repository is retired against a validated stored shape");

    assert_eq!(
        admission_refusal(&error),
        RuleAdmissionRefusal::StorageCorruption
    );
    let deactivated: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM repo_watch_rule_deactivation
              WHERE repository = $1 AND rule_id = $2 AND rule_version = $3
         )",
    )
    .bind(fixture.repository.as_str())
    .bind(fixture.rule.id().as_str())
    .bind(i64::try_from(fixture.rule.version().get())?)
    .fetch_one(&fixture.pool)
    .await?;
    assert!(!deactivated);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn retiring_an_omitted_rule_validates_its_field_fingerprints() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    remove_field_fingerprints(&fixture).await?;

    let error = fixture
        .store
        .reconcile_rules(&fixture.repository, &[])
        .await
        .expect_err("an omitted rule is retired against a validated stored shape");

    assert_eq!(
        admission_refusal(&error),
        RuleAdmissionRefusal::StorageCorruption
    );
    let deactivated: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM repo_watch_rule_deactivation
              WHERE repository = $1 AND rule_id = $2 AND rule_version = $3
         )",
    )
    .bind(fixture.repository.as_str())
    .bind(fixture.rule.id().as_str())
    .bind(i64::try_from(fixture.rule.version().get())?)
    .fetch_one(&fixture.pool)
    .await?;
    assert!(!deactivated);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn every_active_activation_carries_its_field_fingerprints() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let retired: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM repo_watch_rule_activation AS activation
              WHERE NOT EXISTS (
                    SELECT 1
                      FROM repo_watch_rule_field_fingerprint AS fingerprint
                     WHERE fingerprint.repository = activation.repository
                       AND fingerprint.rule_id = activation.rule_id
                       AND fingerprint.rule_version = activation.rule_version
              )
                AND NOT EXISTS (
                    SELECT 1
                      FROM repo_watch_rule_deactivation AS deactivation
                     WHERE deactivation.repository = activation.repository
                       AND deactivation.rule_id = activation.rule_id
                       AND deactivation.rule_version = activation.rule_version
              )
         )",
    )
    .fetch_one(&fixture.pool)
    .await?;

    assert!(!retired);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_revision_below_the_highest_recorded_revision_is_refused() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(rule_at_version(
        RepoWatchRuleVersion::new(
            NonZeroU64::new(REPLACEMENT_RULE_VERSION).expect("recorded version is positive"),
        )
        .expect("recorded version is within the durable range"),
    )?)
    .await?;

    let error = fixture
        .store
        .reconcile_rules(
            &fixture.repository,
            std::slice::from_ref(&rule_at_version(RepoWatchRuleVersion::V1)?),
        )
        .await
        .expect_err("a revision below the highest recorded revision is not a replacement");

    assert_eq!(
        admission_refusal(&error),
        RuleAdmissionRefusal::RegressedVersion {
            configured: RepoWatchRuleVersion::V1,
            latest: RepoWatchRuleVersion::new(
                NonZeroU64::new(REPLACEMENT_RULE_VERSION).expect("recorded version is positive")
            )
            .expect("recorded version is within the durable range")
        }
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn repeating_an_admission_commit_changes_nothing() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let replacement_version = RepoWatchRuleVersion::new(
        NonZeroU64::new(REPLACEMENT_RULE_VERSION).expect("replacement version is positive"),
    )
    .expect("replacement version is within the durable range");
    let replacement = rule_at_version(replacement_version)?;
    let repositories = [fixture.repository.clone()];
    fixture
        .store
        .reconcile_configured_rules(&repositories, std::slice::from_ref(&replacement))
        .await?;
    let after_first: Vec<(i64, bool)> = revisions_of(&fixture, replacement.id().as_str()).await?;

    // The rerun that resolves a lost commit response takes this path.
    fixture
        .store
        .reconcile_configured_rules(&repositories, std::slice::from_ref(&replacement))
        .await?;

    let after_second: Vec<(i64, bool)> = revisions_of(&fixture, replacement.id().as_str()).await?;
    assert_eq!(
        after_first,
        [
            (i64::try_from(fixture.rule.version().get())?, true),
            (i64::try_from(replacement.version().get())?, false)
        ]
    );
    assert_eq!(after_second, after_first);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn validating_an_admissible_revision_consumes_no_revision() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let replacement_version = RepoWatchRuleVersion::new(
        NonZeroU64::new(REPLACEMENT_RULE_VERSION).expect("replacement version is positive"),
    )
    .expect("replacement version is within the durable range");
    let replacement = rule_at_version(replacement_version)?;

    fixture
        .store
        .validate_configured_rules(
            std::slice::from_ref(&fixture.repository),
            std::slice::from_ref(&replacement),
        )
        .await?;

    let revisions: Vec<(i64, bool)> = sqlx::query_as(
        "SELECT activation.rule_version, deactivation.rule_id IS NOT NULL AS deactivated
           FROM repo_watch_rule_activation AS activation
           LEFT JOIN repo_watch_rule_deactivation AS deactivation
             USING (repository, rule_id, rule_version)
          WHERE activation.repository = $1 AND activation.rule_id = $2
          ORDER BY activation.rule_version",
    )
    .bind(fixture.repository.as_str())
    .bind(replacement.id().as_str())
    .fetch_all(&fixture.pool)
    .await?;

    assert_eq!(
        revisions,
        [(i64::try_from(fixture.rule.version().get())?, false)]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn validation_refuses_an_inadmissible_rule_before_any_write() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let changed_rule = merged_event_rule()?;

    let error = fixture
        .store
        .validate_configured_rules(
            std::slice::from_ref(&fixture.repository),
            std::slice::from_ref(&changed_rule),
        )
        .await
        .expect_err("an unbumped semantic change is refused during validation");

    assert_eq!(
        admission_refusal(&error),
        RuleAdmissionRefusal::ChangedField(RepoWatchRuleIdentityField::MatcherEventKinds)
    );
    let deactivated: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM repo_watch_rule_deactivation WHERE repository = $1)",
    )
    .bind(fixture.repository.as_str())
    .fetch_one(&fixture.pool)
    .await?;
    assert!(!deactivated);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_refused_repository_leaves_every_other_repository_unmutated() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture().await?;
    let second_repository = RepositorySlug::try_new(SECOND_REPOSITORY.to_owned())?;
    let replacement_version = RepoWatchRuleVersion::new(
        NonZeroU64::new(REPLACEMENT_RULE_VERSION).expect("replacement version is positive"),
    )
    .expect("replacement version is within the durable range");
    let replacement = rule_at_version(replacement_version)?;
    fixture
        .store
        .reconcile_rules(&second_repository, std::slice::from_ref(&replacement))
        .await?;
    fixture
        .store
        .reconcile_rules(&second_repository, &[])
        .await?;

    let error = fixture
        .store
        .reconcile_configured_rules(
            &[fixture.repository.clone(), second_repository.clone()],
            std::slice::from_ref(&replacement),
        )
        .await
        .expect_err("the second repository retired the configured identity");

    assert_eq!(
        admission_refusal(&error),
        RuleAdmissionRefusal::ReusedIdentity
    );
    let revisions: Vec<(i64, bool)> = sqlx::query_as(
        "SELECT activation.rule_version, deactivation.rule_id IS NOT NULL AS deactivated
           FROM repo_watch_rule_activation AS activation
           LEFT JOIN repo_watch_rule_deactivation AS deactivation
             USING (repository, rule_id, rule_version)
          WHERE activation.repository = $1 AND activation.rule_id = $2
          ORDER BY activation.rule_version",
    )
    .bind(fixture.repository.as_str())
    .bind(fixture.rule.id().as_str())
    .fetch_all(&fixture.pool)
    .await?;
    assert_eq!(
        revisions,
        [(i64::try_from(fixture.rule.version().get())?, false)]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn version_bump_retires_the_old_rule_and_activates_the_replacement_after_history()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let replacement_version = RepoWatchRuleVersion::new(
        NonZeroU64::new(REPLACEMENT_RULE_VERSION).expect("replacement version is positive"),
    )
    .expect("replacement version is within the durable range");
    let replacement = rule_at_version(replacement_version)?;

    fixture
        .store
        .reconcile_rules(&fixture.repository, std::slice::from_ref(&replacement))
        .await?;

    let revisions: Vec<(i64, bool)> = sqlx::query_as(
        "SELECT activation.rule_version, deactivation.rule_id IS NOT NULL AS deactivated
           FROM repo_watch_rule_activation AS activation
           LEFT JOIN repo_watch_rule_deactivation AS deactivation
             USING (repository, rule_id, rule_version)
          WHERE activation.repository = $1 AND activation.rule_id = $2
          ORDER BY activation.rule_version",
    )
    .bind(fixture.repository.as_str())
    .bind(replacement.id().as_str())
    .fetch_all(&fixture.pool)
    .await?;
    assert_eq!(
        revisions,
        [
            (i64::try_from(fixture.rule.version().get())?, true),
            (i64::try_from(replacement.version().get())?, false)
        ]
    );
    assert_eq!(
        fixture
            .store
            .load_next_event(&fixture.repository, replacement.id(), replacement.version())
            .await?,
        None
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn fresh_rule_identity_still_replaces_an_active_rule() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let replacement_id = RepoWatchRuleId::try_new(FRESH_RULE.to_owned())?;
    let replacement = rule_with_identity(replacement_id, RepoWatchRuleVersion::V1)?;

    fixture
        .store
        .reconcile_rules(&fixture.repository, std::slice::from_ref(&replacement))
        .await?;

    let active_rule: String = sqlx::query_scalar(
        "SELECT activation.rule_id
           FROM repo_watch_rule_activation AS activation
          WHERE activation.repository = $1
            AND NOT EXISTS (
                SELECT 1 FROM repo_watch_rule_deactivation AS deactivation
                 WHERE deactivation.repository = activation.repository
                   AND deactivation.rule_id = activation.rule_id
                   AND deactivation.rule_version = activation.rule_version
            )",
    )
    .bind(fixture.repository.as_str())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(active_rule, replacement.id().as_str());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn deactivated_rule_cannot_dispatch_an_already_loaded_event() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let (loaded, observation) = load_second_conflict(&fixture).await?;
    let batches_before: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
        .fetch_one(&fixture.pool)
        .await?;
    fixture
        .store
        .reconcile_rules(&fixture.repository, &[])
        .await?;
    let outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                loaded,
                &fixture.rule,
                &observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    let batches_after: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
        .fetch_one(&fixture.pool)
        .await?;

    assert_eq!(outcome, RepoWatchRuleEvaluationOutcome::Inactive);
    assert_eq!(batches_after, batches_before);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cooldown_uses_the_terminal_transition_time_not_the_next_evaluation_time()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(cooldown_rule()?).await?;
    sqlx::query(
        "INSERT INTO repo_watch_dispatch_release (dispatch_id, released_at)
         VALUES ($1, transaction_timestamp() - interval '2 hours')",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .execute(&fixture.pool)
    .await?;
    let release_age_seconds: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM (transaction_timestamp() - released_at))::float8
           FROM repo_watch_dispatch_release
          WHERE dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let outcome = evaluate_second_conflict(&fixture).await?;

    assert!(release_age_seconds >= 7_199.0);
    assert!(outcome_is_dispatched(&outcome));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pursuing_goal_holds_singleton_until_its_terminal_transition() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let session = fixture.sessions[0];
    let dispatched_turn = TurnId::from_uuid(
        sqlx::query_scalar::<_, Uuid>(
            "SELECT turn_id FROM repo_watch_dispatch_delivery WHERE dispatch_id = $1",
        )
        .bind(fixture.dispatch_id.as_uuid())
        .fetch_one(&fixture.pool)
        .await?,
    );
    mark_queued_turn_failed(&fixture.pool, session, dispatched_turn, 0x4_000).await?;
    check_completed_turn_for_release(&fixture.pool, session, dispatched_turn).await?;
    assert_eq!(release_count(&fixture).await?, 0);
    assert_applied_goal_transition(
        GoalRepository::new(fixture.pool.clone())
            .block_execution_failure(
                session,
                GoalNeed::try_new(String::from("repair the failed goal turn"))
                    .expect("fixture goal need is valid"),
                GoalSchedulerProvenance::new(dispatched_turn),
            )
            .await?,
    );
    assert_eq!(release_count(&fixture).await?, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn release_timestamp_is_sampled_after_dispatch_lock_wait() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let turn: Uuid = sqlx::query_scalar(
        "SELECT turn_id FROM repo_watch_dispatch_delivery WHERE dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    sqlx::query(
        "ALTER TABLE goal_event
         DISABLE TRIGGER repo_watch_dispatch_release_on_terminal_goal",
    )
    .execute(&fixture.pool)
    .await?;
    withdraw_dispatched_goal(&fixture.pool, fixture.sessions[0], 0x20_000).await?;
    sqlx::query(
        "ALTER TABLE goal_event
         ENABLE TRIGGER repo_watch_dispatch_release_on_terminal_goal",
    )
    .execute(&fixture.pool)
    .await?;
    let mut dispatch_lock = fixture.pool.begin().await?;
    sqlx::query("SELECT 1 FROM repo_watch_dispatch_batch WHERE dispatch_id = $1 FOR UPDATE")
        .bind(fixture.dispatch_id.as_uuid())
        .execute(&mut *dispatch_lock)
        .await?;
    let mut release_connection = fixture.pool.acquire().await?;
    let release_backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *release_connection)
        .await?;
    let release = tokio::spawn(async move {
        sqlx::query("SELECT repo_watch_release_completed_dispatch_batches_for_turn($1, $2)")
            .bind(turn)
            .bind(fixture.sessions[0].as_uuid())
            .execute(&mut *release_connection)
            .await
    });

    wait_for_backend_lock(&fixture.pool, release_backend).await?;
    let serialized_at: f64 =
        sqlx::query_scalar("SELECT extract(epoch FROM clock_timestamp())::float8")
            .fetch_one(&fixture.pool)
            .await?;
    dispatch_lock.commit().await?;
    release.await??;
    let released_at: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM released_at)::float8
           FROM repo_watch_dispatch_release
          WHERE dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert!(released_at >= serialized_at);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn concurrent_terminal_batch_checks_serialize_on_the_dispatch() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture().await?;
    let turns: Vec<Uuid> = sqlx::query_scalar(
        "SELECT turn_id
           FROM repo_watch_dispatch_delivery
          WHERE dispatch_id = $1
          ORDER BY action_ordinal",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_all(&fixture.pool)
    .await?;
    sqlx::query(
        "ALTER TABLE goal_event
         DISABLE TRIGGER repo_watch_dispatch_release_on_terminal_goal",
    )
    .execute(&fixture.pool)
    .await?;
    withdraw_dispatched_goal(&fixture.pool, fixture.sessions[0], 0x30_000).await?;
    withdraw_dispatched_goal(&fixture.pool, fixture.sessions[1], 0x40_000).await?;
    sqlx::query(
        "ALTER TABLE goal_event
         ENABLE TRIGGER repo_watch_dispatch_release_on_terminal_goal",
    )
    .execute(&fixture.pool)
    .await?;
    mark_queued_turn_failed(
        &fixture.pool,
        fixture.sessions[0],
        TurnId::from_uuid(turns[0]),
        FIRST_TERMINAL_IDENTITY_SEED,
    )
    .await?;
    let mut first = fixture.pool.begin().await?;
    sqlx::query("SELECT repo_watch_release_completed_dispatch_batches_for_turn($1, $2)")
        .bind(turns[0])
        .bind(fixture.sessions[0].as_uuid())
        .execute(&mut *first)
        .await?;
    let mut second = fixture.pool.acquire().await?;
    let second_backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *second)
        .await?;
    let second_check = tokio::spawn(async move {
        sqlx::query("SELECT repo_watch_release_completed_dispatch_batches_for_turn($1, $2)")
            .bind(turns[1])
            .bind(fixture.sessions[1].as_uuid())
            .execute(&mut *second)
            .await
    });

    wait_for_backend_lock(&fixture.pool, second_backend).await?;
    first.commit().await?;
    second_check.await??;
    let release_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_dispatch_release
          WHERE dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(release_count, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cooldown_clock_is_sampled_after_singleton_lock_wait() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    sqlx::query(
        "INSERT INTO repo_watch_dispatch_release (dispatch_id, released_at)
         VALUES ($1, clock_timestamp() + interval '2 seconds')",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .execute(&fixture.pool)
    .await?;
    let (loaded, observation) = load_second_conflict(&fixture).await?;
    let mut repository_lock = fixture.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(fixture.repository.as_str())
        .execute(&mut *repository_lock)
        .await?;
    let store = fixture.store.clone();
    let rule = fixture.rule.clone();
    let evaluation = tokio::spawn(async move {
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, store)
            .evaluate(
                loaded,
                &rule,
                &observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await
    });

    wait_for_advisory_lock(&fixture.pool).await?;
    tokio::time::sleep(Duration::from_millis(2_100)).await;
    repository_lock.commit().await?;
    let outcome = evaluation.await??;

    assert!(outcome_is_dispatched(&outcome));
    Ok(())
}

/// Release reproduces admission's singleton advisory key in SQL, and a drift
/// between the two spellings would stop serializing silently rather than fail.
/// Holding the key SQL computes must therefore block admission itself.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn admission_waits_for_the_singleton_key_computed_in_sql() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let (loaded, observation) = load_second_conflict(&fixture).await?;
    let holder = hold_singleton_advisory_key(&fixture).await?;
    let store = fixture.store.clone();
    let rule = fixture.rule.clone();
    let evaluation = tokio::spawn(async move {
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, store)
            .evaluate(
                loaded,
                &rule,
                &observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await
    });

    wait_for_advisory_lock(&fixture.pool).await?;
    holder.commit().await?;

    assert_eq!(evaluation.await??, RepoWatchRuleEvaluationOutcome::Occupied);
    Ok(())
}

/// Termination takes the singleton key before it reads durable state or writes
/// an obligation, so a match racing it joins that obligation instead of
/// aborting on the active-singleton index.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn termination_waits_for_the_singleton_key_before_owing_a_requeue()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let holder = hold_singleton_advisory_key(&fixture).await?;
    let pool = fixture.pool.clone();
    let session = fixture.session(0);
    let termination = tokio::spawn(async move {
        GoalRepository::new(pool)
            .handle_user_command(
                GoalUserCommand::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(TERMINATION_RACE_STOP_COMMAND_ID)),
                    session,
                    GoalUserAction::Stop {
                        descendant_scope: DescendantTerminationScope::ParentAlone,
                    },
                ),
                None,
                |_| None,
            )
            .await
    });

    wait_for_advisory_lock(&fixture.pool).await?;
    let owed_while_blocked = outstanding_obligation_count(&fixture.pool).await?;
    holder.commit().await?;
    assert_applied_goal_command(termination.await??);

    assert_eq!(owed_while_blocked, 0);
    assert_eq!(outstanding_obligation_count(&fixture.pool).await?, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn occupied_pull_request_singleton_suppresses_a_later_match() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let outcome = evaluate_second_conflict(&fixture).await?;

    assert_eq!(outcome, RepoWatchRuleEvaluationOutcome::Occupied);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn occupied_matches_collapse_into_one_visible_dispatch_obligation()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let second = evaluate_second_conflict(&fixture).await?;
    let third = evaluate_conflict(&fixture, 103, THIRD_HEAD).await?;
    let visible: (i64, Uuid, String, Uuid, Vec<Uuid>, bool) = sqlx::query_as(
        "SELECT matched_event_count, latest_event_id,
                singleton_pull_request_number::text, occupying_dispatch_id,
                occupying_session_ids, ready
           FROM repo_watch_outstanding_dispatch_obligation",
    )
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(second, RepoWatchRuleEvaluationOutcome::Occupied);
    assert_eq!(third.outcome, RepoWatchRuleEvaluationOutcome::Occupied);
    assert_eq!(visible.0, 2);
    assert_eq!(visible.1, *third.event_id.as_uuid());
    assert_eq!(
        visible.2,
        pull_request_number(&fixture.event).get().to_string()
    );
    assert_eq!(visible.3, *fixture.dispatch_id.as_uuid());
    assert_eq!(visible.4, session_uuids(&fixture));
    assert!(!visible.5);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn released_obligation_dispatches_latest_state_once_and_replays_that_delivery()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let _second = evaluate_second_conflict(&fixture).await?;
    sqlx::query(
        "INSERT INTO repo_watch_dispatch_release (dispatch_id)
         VALUES ($1)",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .execute(&fixture.pool)
    .await?;
    let third = evaluate_conflict(&fixture, 103, THIRD_HEAD).await?;
    let cursor = PostgresRepoWatchStore::new(fixture.pool.clone())
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    let obligation = fixture
        .store
        .load_next_dispatch_obligation(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?
        .expect("released obligation is ready");
    let replay_candidate = obligation.clone();

    assert_eq!(third.outcome, RepoWatchRuleEvaluationOutcome::Occupied);
    assert_eq!(obligation.matched_event_count(), 2);
    assert_eq!(obligation.latest_event().id(), third.event_id);
    let (dispatch_id, sessions) = dispatched(
        evaluate_obligation(&fixture, obligation, cursor.candidate().observation()).await?,
    );
    let outstanding: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_outstanding_dispatch_obligation")
            .fetch_one(&fixture.pool)
            .await?;
    let batch_count: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
        .fetch_one(&fixture.pool)
        .await?;
    let (replayed_dispatch, replayed_sessions) = replayed(
        evaluate_obligation(&fixture, replay_candidate, cursor.candidate().observation()).await?,
    );
    let replayed_batch_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
            .fetch_one(&fixture.pool)
            .await?;

    assert_eq!(outstanding, 0);
    assert_eq!(batch_count, 2);
    assert_eq!(replayed_dispatch, dispatch_id);
    assert_eq!(replayed_sessions, sessions);
    assert_eq!(replayed_batch_count, batch_count);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn dispatch_obligation_waits_visibly_through_configured_cooldown()
-> Result<(), Box<dyn Error>> {
    let fixture =
        dispatch_fixture_for(one_action_rule(Duration::from_secs(i64::MAX as u64))?).await?;
    let _outcome = evaluate_second_conflict(&fixture).await?;
    sqlx::query(
        "INSERT INTO repo_watch_dispatch_release (dispatch_id)
         VALUES ($1)",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .execute(&fixture.pool)
    .await?;
    let obligation = fixture
        .store
        .load_next_dispatch_obligation(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?;
    let visible: OutstandingCooldownVisibility = sqlx::query_as(
        "SELECT matched_event_count,
                eligible_at > clock_timestamp() AS eligible_at_is_future, ready
           FROM repo_watch_outstanding_dispatch_obligation",
    )
    .fetch_one(&fixture.pool)
    .await?;

    assert!(obligation.is_none());
    assert_eq!(visible.matched_event_count, 1);
    assert!(visible.eligible_at_is_future);
    assert!(!visible.ready);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn rule_deactivation_settles_its_outstanding_dispatch_obligation()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let _outcome = evaluate_second_conflict(&fixture).await?;
    fixture
        .store
        .reconcile_rules(&fixture.repository, &[])
        .await?;
    let outstanding: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_outstanding_dispatch_obligation")
            .fetch_one(&fixture.pool)
            .await?;
    let settlement: String = sqlx::query_scalar(
        "SELECT settled_kind
           FROM repo_watch_dispatch_obligation",
    )
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(outstanding, 0);
    assert_eq!(settlement, "deactivated");
    Ok(())
}
