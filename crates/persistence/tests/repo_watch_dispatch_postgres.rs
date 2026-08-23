#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{error::Error, num::NonZeroU64, time::Duration};

use signalbox_application::{
    ApprovalJudgeCompletionIdentities, ApprovalJudgeDispatchAuthority,
    ApprovalJudgeDispatchProvenance, ApprovalJudgePullRequestAuthority, AuthorizeModelCallOutcome,
    CommissionDispatchRequest, CommissionedDispatchFence, ModelCallCredentialReference,
    RepoWatchBranchHead, RepoWatchDispatchService, RepoWatchDispatchTransaction,
    RepoWatchEventContentIdentityV1, RepoWatchEventOccurrenceV1, RepoWatchObservation,
    RepoWatchPullRequestLifecycle, RepoWatchPullRequestState, RepoWatchPullRequestStateInput,
    RepoWatchRepositoryState, RepoWatchRepositoryStateInput, RepoWatchResolvedTemplate,
    RepoWatchRuleEvaluation, RepoWatchRuleEvaluationOutcome, RepoWatchTemplateResolver,
    RepoWatchWorkflowRunObservation, StartEligibleTurnOutcome, StartEligibleTurnService,
    UuidV7CommissionedDispatchIdGenerator, UuidV7RepoWatchDispatchIdGenerator,
    UuidV7StartEligibleTurnIdGenerator,
};
use signalbox_domain::{
    AcceptedInputId, ActiveTurnPhase, AssistantResponsePart, BranchName,
    CancelledModelCallTurnIdentities, CheckConclusion, CommissionedDispatchId, CommitSha,
    ContextFrontierId, CreateSession, DangerousToolAutoApproval, DelegateApprovalRecommendation,
    DeliveryRequest, DescendantTerminationScope, DirectModelSelection, DurableCommandId,
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
    SessionConfigurationDefaults, SessionCreationCause, SessionCreationProvenance, SessionId,
    SessionSystemPrompt, SessionTemplateContentDigest, SessionTemplateName,
    SessionTemplateProvenance, SubmitInput, ToolCallProposal, ToolDecisionRationale, ToolName,
    ToolRequestId, ToolResponsePartIdentity, ToolRoundModelCallIdentities,
    ToolUsingAssistantResponse, TranscriptAncestry, TurnAttemptId, TurnId, UserContent,
    WorkflowName,
};
use signalbox_persistence::{
    SessionCredentialPin, SessionModelCredential,
    approval_judge::{
        ApprovalJudgeCorruption, ApprovalJudgeRepositoryError, CompleteApprovalJudgeOutcome,
        PrepareApprovalJudgeOutcome, PreparedApprovalJudge,
    },
    commissioned_dispatch::{CommissionDispatchOutcome, PostgresCommissionedDispatchStore},
    create_session::{
        CreateSessionHandlingOutcome, CreateSessionRepository, CreateSessionRepositoryError,
    },
    disposable_postgres_server_args, disposable_postgres_state_tmpfs,
    disposable_test_container_labels,
    goal::{GoalCommandHandlingOutcome, GoalRepository, GoalTransitionOutcome},
    goal_turn::GoalTurnCandidates,
    local_test_connection_options, migrate,
    model_execution::{PostgresModelCallRepository, PrepareInitialModelCallOutcome},
    repo_watch::{
        PostgresRepoWatchStore, RepoWatchCommitOutcome, RepoWatchCommitRequest,
        RepoWatchCursorCandidate, RepoWatchCursorGeneration, RepoWatchObservedReviewState,
        RepoWatchStaleReviewClearanceOutcome,
    },
    repo_watch_dispatch::{PostgresRepoWatchDispatchStore, RepoWatchDispatchRepositoryError},
    repo_watch_dispatch_obligation::{
        RepoWatchDispatchObligation, RepoWatchDispatchRetryPolicy, RepoWatchObligationParkRelease,
    },
    start_eligible_turn::StartEligibleTurnRepository,
    submit_input::SubmitInputRepository,
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
const BASE_REVISION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ADVANCED_BASE_REVISION: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const STALE_REVIEW_NODE_ID: &str = "PRR_stale_review_fixture";
const STALE_REVIEWER: &str = "review-bot[bot]";
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
const PARK_FIRST_STOP_COMMAND_ID: u128 = 0x58_100;
const CROSS_TARGET_OPENED_EVENT_ID: u128 = 0x58_500;
const PARKED_TARGET_CUTOFF_EVENT_ID: u128 = 0x58_800;
const SPENT_FRONTIER_OLDER_EVENT_ID: u128 = 0x58_900;
const SPENT_FRONTIER_NEWER_EVENT_ID: u128 = 0x58_901;
const SPENT_FRONTIER_STOP_COMMAND_ID: u128 = 0x58_902;
const CROSS_TARGET_SPEND_PROGRESS_EVENT_ID: u128 = 0x58_910;
const CROSS_TARGET_SPEND_STOP_COMMAND_ID: u128 = 0x58_911;
const CROSS_TARGET_SPEND_NEIGHBOUR_EVENT_ID: u128 = 0x58_912;
const PARKED_TARGET_CUTOFF_COMMAND_ID: u128 = 0x58_801;
const NONMATCHING_PROGRESS_EVENT_ID: u128 = 0x58_600;
const SIBLING_COUNT_FIRST_STOP_COMMAND_ID: u128 = 0x58_700;
const SIBLING_COUNT_SECOND_STOP_COMMAND_ID: u128 = 0x58_701;
const CROSS_TARGET_CONFLICT_EVENT_ID: u128 = 0x58_501;
const REPOSITORY_SCOPED_RULE: &str = "merge-forward-per-repository";
const BATCH_DELAY_FIRST_STOP_COMMAND_ID: u128 = 0x58_400;
const BATCH_DELAY_SECOND_STOP_COMMAND_ID: u128 = 0x58_401;
const PARK_RELEASING_EVENT_ID: u128 = 0x58_200;
const PARK_STALE_EVENT_ID: u128 = 0x58_300;
const PARK_RELEASE_ACTOR: &str = "operator-under-test";
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
const SUCCESSOR_JUDGE_SUPERSEDE_COMMAND_ID: u128 = 0x5d_000;
const SUCCESSOR_JUDGE_INPUT_ID: u128 = 0x5d_100;
const SUCCESSOR_JUDGE_TURN_ID: u128 = 0x5d_200;
const STEERED_DISPATCH_COMMAND_ID: u128 = 0x5d_300;
const STEERED_DISPATCH_INPUT_ID: u128 = 0x5d_400;
const RESUMED_DISPATCH_COMMAND_ID: u128 = 0x5d_500;
const RESUMED_DISPATCH_INPUT_ID: u128 = 0x5d_600;
const RESUMED_DISPATCH_TURN_ID: u128 = 0x5d_700;
const STALE_DISPATCH_RESUME_COMMAND_ID: u128 = 0x5d_800;
const STALE_DISPATCH_INPUT_ID: u128 = 0x5d_900;
const STALE_DISPATCH_TURN_ID: u128 = 0x5d_a00;
const STALE_DISPATCH_STOP_COMMAND_ID: u128 = 0x5d_b00;
const HELD_BATCH_RESUME_COMMAND_ID: u128 = 0x5d_c00;
const HELD_BATCH_RESUME_INPUT_ID: u128 = 0x5d_d00;
const HELD_BATCH_RESUME_TURN_ID: u128 = 0x5d_e00;
const TEMPLATE_MODEL_SELECTION_ID: u128 = 901;
const APPROVAL_JUDGE_PROVIDER_ID: u128 = 902;
const FIXTURE_CREDENTIAL_REFERENCE: &str = "fixture-credential";
const CLOSED_RESULT_ID_OFFSET: u128 = 0x2_000_000;

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_cmd(disposable_postgres_server_args())
        .with_mount(disposable_postgres_state_tmpfs())
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
    pull_request_observation(context, lifecycle, MergeableState::Conflicting)
}

fn pull_request_observation(
    context: PullRequestEventContext,
    lifecycle: RepoWatchPullRequestLifecycle,
    mergeable_state: MergeableState,
) -> Result<RepoWatchObservation, Box<dyn Error>> {
    pull_request_observation_at_base(context, lifecycle, mergeable_state, BASE_REVISION)
}

fn pull_request_observation_at_base(
    context: PullRequestEventContext,
    lifecycle: RepoWatchPullRequestLifecycle,
    mergeable_state: MergeableState,
    base_revision: &str,
) -> Result<RepoWatchObservation, Box<dyn Error>> {
    Ok(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![RepoWatchPullRequestState::try_new(
                RepoWatchPullRequestStateInput {
                    context,
                    lifecycle,
                    mergeable_state,
                    completed_check_suites: Vec::new(),
                    completed_check_runs: Vec::new(),
                    reviews: Vec::new(),
                    threads: Vec::new(),
                    reactions: Vec::new(),
                },
            )?],
            workflow_runs: Vec::new(),
            branch_heads: vec![RepoWatchBranchHead::new(
                BranchName::try_new(BASE_BRANCH.to_owned())?,
                CommitSha::try_new(base_revision.to_owned())?,
            )],
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
    mergeable_event(value, head, MergeableState::Conflicting)
}

fn mergeable_event(
    value: u128,
    head: &str,
    current: MergeableState,
) -> Result<RepoWatchEvent, Box<dyn Error>> {
    Ok(RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        context(head)?,
        RepoWatchEventKindV1::MergeableStateChanged { current },
    )?)
}

fn merge_ready_assessment(head: &str) -> Result<RepoWatchConvergenceAssessment, Box<dyn Error>> {
    assessment_with_review_decision(head, RepoWatchReviewDecision::None)
}

fn assessment_with_review_decision(
    head: &str,
    review_decision: RepoWatchReviewDecision,
) -> Result<RepoWatchConvergenceAssessment, Box<dyn Error>> {
    assessment_at_base(head, BASE_REVISION, review_decision)
}

fn assessment_at_base(
    head: &str,
    base_revision: &str,
    review_decision: RepoWatchReviewDecision,
) -> Result<RepoWatchConvergenceAssessment, Box<dyn Error>> {
    Ok(RepoWatchConvergenceAssessment::try_new(
        RepoWatchConvergenceAssessmentInput {
            number: PullRequestNumber::new(41_u64.try_into()?),
            head_sha: CommitSha::try_new(head.to_owned())?,
            base_branch: BranchName::try_new(BASE_BRANCH.to_owned())?,
            base_revision: CommitSha::try_new(base_revision.to_owned())?,
            mergeable_state: MergeableState::Mergeable,
            review_decision,
            unresolved_threads: Vec::new(),
            gating_check_count: 0,
            non_green_gating_checks: Vec::<CheckRunName>::new(),
        },
    )?)
}

fn conflict_event_for(
    value: u128,
    context: PullRequestEventContext,
) -> Result<RepoWatchEvent, Box<dyn Error>> {
    Ok(RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        context,
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

/// One conflict rule whose singleton collapses every pull request in the
/// repository onto a single obligation.
fn repository_scoped_conflict_rule() -> Result<RepoWatchRule, Box<dyn Error>> {
    Ok(RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(REPOSITORY_SCOPED_RULE.to_owned())?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::MergeableStateChanged],
            mergeable_state: vec![MergeableState::Conflicting],
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(TEMPLATE.to_owned())?,
        }],
        RepoWatchSingletonScope::Repository,
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
                    TEMPLATE_MODEL_SELECTION_ID,
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
struct ParkedObligationVisibility {
    obligation_id: Uuid,
    failed_attempts: i64,
    head_sha: Option<String>,
}

#[derive(Debug, Eq, PartialEq, sqlx::FromRow)]
struct ParkTransitionVisibility {
    transition_kind: String,
    failed_attempts: i64,
    release_reason: Option<String>,
    release_actor: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct HeldSlotVisibility {
    every_action_delivered: bool,
    every_delivery_turn_releasable: bool,
    no_live_runtime_turn: bool,
    every_goal_nonpursuing: bool,
    blockers: Vec<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct HeadlessEscalationVisibility {
    lifecycle_state: String,
    terminal_disposition: Option<String>,
    goal_event_kind: String,
    blocked_reason: Option<String>,
    rationale: String,
    dispatch_released: bool,
    replacement_owed: bool,
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
        FIXTURE_CREDENTIAL_REFERENCE,
    )])
    .expect("fixture credential pin is valid")
}

fn model_credential_reference() -> ModelCallCredentialReference {
    ModelCallCredentialReference::new(FIXTURE_CREDENTIAL_REFERENCE)
}

fn model_targets() -> ModelTargetCatalog {
    ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        DirectModelSelection::from_uuid(Uuid::from_u128(TEMPLATE_MODEL_SELECTION_ID)),
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(
            APPROVAL_JUDGE_PROVIDER_ID,
        ))),
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
        Vec<ToolRequestId>,
    ),
    Box<dyn Error>,
> {
    checkpoint_dispatched_delegated_approval_for(
        fixture,
        seed,
        &[("exec", r#"{"cmd":"git fetch origin main"}"#)],
    )
    .await
}

async fn checkpoint_dispatched_delegated_approval_for(
    fixture: &DispatchFixture,
    seed: u128,
    proposals: &[(&str, &str)],
) -> Result<
    (
        PostgresModelCallRepository,
        PreparedApprovalJudge,
        TurnId,
        Vec<ToolRequestId>,
    ),
    Box<dyn Error>,
> {
    checkpoint_delegated_approval_at(&fixture.pool, fixture.session(0), seed, proposals).await
}

/// Parks the given session's already-queued first turn on a delegated
/// approval, however the session was dispatched.
async fn checkpoint_delegated_approval_at(
    pool: &PgPool,
    session: SessionId,
    seed: u128,
    proposals: &[(&str, &str)],
) -> Result<
    (
        PostgresModelCallRepository,
        PreparedApprovalJudge,
        TurnId,
        Vec<ToolRequestId>,
    ),
    Box<dyn Error>,
> {
    let mut activation = StartEligibleTurnService::new(
        UuidV7StartEligibleTurnIdGenerator,
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) = activation.execute(session).await? else {
        panic!("the dispatched work turn activates")
    };
    let turn = activated.turn();
    drop(activated);

    let repository = PostgresModelCallRepository::new(
        pool.clone(),
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
    let requests = proposals
        .iter()
        .enumerate()
        .map(|(ordinal, _)| {
            ToolRequestId::from_uuid(Uuid::from_u128(
                seed + 6 + u128::try_from(ordinal).expect("fixture ordinal fits u128"),
            ))
        })
        .collect::<Vec<_>>();
    let response = ToolUsingAssistantResponse::try_from_parts(
        proposals
            .iter()
            .map(|(name, arguments)| {
                AssistantResponsePart::ToolCall(ToolCallProposal::new(
                    ToolName::try_new(String::from(*name))
                        .expect("the fixture tool name is admitted"),
                    NormalizedToolArguments::try_from_provider_text(String::from(*arguments))
                        .expect("the fixture arguments are admitted"),
                ))
            })
            .collect(),
    )
    .expect("the proposals form a tool-using response");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools { response });
    let ModelCallTerminalOutcome::ToolRound(round) = repository
        .apply_terminal_observation(
            session,
            observation,
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                requests
                    .iter()
                    .enumerate()
                    .map(|(ordinal, request)| {
                        ToolResponsePartIdentity::tool_call(
                            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                                seed + 100
                                    + u128::try_from(ordinal).expect("fixture ordinal fits u128"),
                            )),
                            *request,
                            InitialToolApproval::Delegated,
                        )
                    })
                    .collect(),
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
        &ActiveTurnPhase::AwaitingApproval {
            request: requests[0],
        }
    );
    let approval_repository = repository.approval_judge_repository();
    let prepared = ready_approval_judge(
        approval_repository
            .prepare(
                session,
                turn,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 9)),
                Some(DirectModelSelection::from_uuid(Uuid::from_u128(
                    TEMPLATE_MODEL_SELECTION_ID,
                ))),
            )
            .await?,
    );
    Ok((repository, prepared, turn, requests))
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

/// The shipped delay would hold every requeued obligation past the life of a
/// test container. Only the delay is lowered; the attempt budget belongs to the
/// schema and is spent for real by the tests that park.
fn immediate_retry_policy() -> RepoWatchDispatchRetryPolicy {
    RepoWatchDispatchRetryPolicy::production().lowered_to(Duration::ZERO, Duration::ZERO)
}

async fn dispatch_attempt_budget(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT repo_watch_dispatch_attempt_budget()")
        .fetch_one(pool)
        .await
}

/// Fails one dispatched session and redispatches the obligation it leaves,
/// returning the session the successor created.
async fn spend_one_attempt(
    fixture: &DispatchFixture,
    session: SessionId,
    identity_seed: u128,
) -> Result<SessionId, Box<dyn Error>> {
    withdraw_dispatched_goal(&fixture.pool, session, identity_seed).await?;
    let requeued = load_next_obligation(fixture)
        .await?
        .expect("a failed attempt inside the budget leaves a dispatchable obligation");
    let cursor = PostgresRepoWatchStore::new(fixture.pool.clone())
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    let (_successor, successor_sessions) =
        dispatched(evaluate_obligation(fixture, requeued, cursor.candidate().observation()).await?);
    Ok(successor_sessions[0])
}

/// Spends the whole shipped budget on the fixture's lineage, returning the
/// obligation its exhausting attempt parked.
///
/// Written out rather than iterated: the budget is a schema constant, and a
/// walk of named attempts fails on the one whose behavior changed.
async fn park_dispatch_obligation(
    fixture: &DispatchFixture,
    identity_seed: u128,
) -> Result<Uuid, Box<dyn Error>> {
    assert_eq!(dispatch_attempt_budget(&fixture.pool).await?, 6);
    let second = spend_one_attempt(fixture, fixture.session(0), identity_seed).await?;
    let third = spend_one_attempt(fixture, second, identity_seed + 1).await?;
    let fourth = spend_one_attempt(fixture, third, identity_seed + 2).await?;
    let fifth = spend_one_attempt(fixture, fourth, identity_seed + 3).await?;
    let sixth = spend_one_attempt(fixture, fifth, identity_seed + 4).await?;
    withdraw_dispatched_goal(&fixture.pool, sixth, identity_seed + 5).await?;
    Ok(
        sqlx::query_scalar("SELECT obligation_id FROM repo_watch_parked_dispatch_obligation")
            .fetch_one(&fixture.pool)
            .await?,
    )
}

/// Opens a second pull request in the watched repository and matches the rule
/// against a conflict on it, leaving both pull requests durably open.
async fn evaluate_neighbour_conflict(
    fixture: &DispatchFixture,
) -> Result<RepoWatchRuleEvaluationOutcome, Box<dyn Error>> {
    let neighbour = same_repository_context(SameRepositoryContextFacts {
        number: TOP_PULL_REQUEST_NUMBER,
        head: SECOND_HEAD,
        base_branch: BASE_BRANCH,
        head_branch: TOP_AGENT_BRANCH,
    })?;
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
                RepoWatchCursorCandidate::new(mergeable_observation(
                    vec![context(FIRST_HEAD)?, neighbour.clone()],
                    Vec::new(),
                )?),
                vec![
                    identified_event(opened_event_for(
                        CROSS_TARGET_OPENED_EVENT_ID,
                        neighbour.clone(),
                    )?),
                    identified_event(conflict_event_for(
                        CROSS_TARGET_CONFLICT_EVENT_ID,
                        neighbour,
                    )?),
                ],
            ),
        )
        .await?;
    let observation = mergeable_observation(vec![context(FIRST_HEAD)?], Vec::new())?;
    let opened = fixture
        .store
        .load_next_event(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?
        .expect("the neighbour's opened event is unevaluated");
    let opened_outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                opened,
                &fixture.rule,
                &observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    assert_eq!(opened_outcome, RepoWatchRuleEvaluationOutcome::NotMatched);
    let conflicting = fixture
        .store
        .load_next_event(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?
        .expect("the neighbour's conflict remains unevaluated");
    Ok(
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                conflicting,
                &fixture.rule,
                &observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?,
    )
}

/// Commits and evaluates a head change on the fixture's pull request. The
/// fixture rule matches conflicts only, so this event is durable progress on the
/// parked target that the parked rule itself never matches.
async fn evaluate_nonmatching_head_change(
    fixture: &DispatchFixture,
) -> Result<RepoWatchRuleEvaluationOutcome, Box<dyn Error>> {
    let event_store = PostgresRepoWatchStore::new(fixture.pool.clone());
    let cursor = event_store
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    let observation = observation(context(SECOND_HEAD)?)?;
    event_store
        .commit(
            &fixture.repository,
            RepoWatchCommitRequest::new(
                Some(cursor.generation()),
                RepoWatchCursorCandidate::new(observation.clone()),
                vec![identified_event(head_changed_event(
                    NONMATCHING_PROGRESS_EVENT_ID,
                    context(SECOND_HEAD)?,
                    FIRST_HEAD,
                )?)],
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
        .expect("the head change remains unevaluated");
    Ok(
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                loaded,
                &fixture.rule,
                &observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?,
    )
}

/// The fixture's pull request at the head it stalled on, restated with one
/// field changed.
///
/// The event store refuses a commit that carries events without moving the
/// observation, and what this exercises is a matching event carrying no new
/// head, so something other than the head has to differ.
fn restated_context() -> Result<PullRequestEventContext, Box<dyn Error>> {
    Ok(PullRequestEventContext::new(PullRequestEventContextInput {
        number: PullRequestNumber::new(BOTTOM_PULL_REQUEST_NUMBER.try_into()?),
        head_sha: CommitSha::try_new(FIRST_HEAD.to_owned())?,
        head_repository: RepositorySlug::try_new(HEAD_REPOSITORY.to_owned())?,
        base_branch: BranchName::try_new(BASE_BRANCH.to_owned())?,
        head_branch: BranchName::try_new(HEAD_BRANCH.to_owned())?,
        title: PullRequestTitle::try_new("Merge forward, restated".to_owned())?,
        body: PullRequestBody::try_new("Resolve the conflict.".to_owned())?,
        labels: Vec::new(),
        draft: false,
        author: Some(RepoWatchAuthorLogin::try_new("fixture-author".to_owned())?),
    }))
}

/// Commits and evaluates a matching conflict on the stalled head.
async fn evaluate_restated_conflict(
    fixture: &DispatchFixture,
) -> Result<RepoWatchRuleEvaluationOutcome, Box<dyn Error>> {
    let event_store = PostgresRepoWatchStore::new(fixture.pool.clone());
    let cursor = event_store
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    let context = restated_context()?;
    let observation = observation(context.clone())?;
    event_store
        .commit(
            &fixture.repository,
            RepoWatchCommitRequest::new(
                Some(cursor.generation()),
                RepoWatchCursorCandidate::new(observation.clone()),
                vec![identified_event(conflict_event_for(
                    PARK_STALE_EVENT_ID,
                    context,
                )?)],
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
        .expect("the restated conflict remains unevaluated");
    Ok(
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                loaded,
                &fixture.rule,
                &observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?,
    )
}

async fn load_next_obligation(
    fixture: &DispatchFixture,
) -> Result<Option<RepoWatchDispatchObligation>, Box<dyn Error>> {
    Ok(fixture
        .store
        .load_next_dispatch_obligation(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
            immediate_retry_policy(),
        )
        .await?)
}

async fn outstanding_failed_attempts(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT failed_attempts
           FROM repo_watch_dispatch_obligation
          WHERE settled_kind IS NULL",
    )
    .fetch_one(pool)
    .await
}

async fn failed_attempt_epoch(pool: &PgPool) -> Result<f64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT extract(epoch FROM last_failed_attempt_at)::double precision
           FROM repo_watch_dispatch_obligation
          WHERE settled_kind IS NULL",
    )
    .fetch_one(pool)
    .await
}

async fn batch_release_epoch(fixture: &DispatchFixture) -> Result<f64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT extract(epoch FROM released_at)::double precision
           FROM repo_watch_dispatch_release
          WHERE dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await
}

async fn park_transitions(pool: &PgPool) -> Result<Vec<ParkTransitionVisibility>, sqlx::Error> {
    sqlx::query_as(
        "SELECT transition_kind, failed_attempts, release_reason, release_actor
           FROM repo_watch_dispatch_obligation_park
          ORDER BY obligation_id, transition_ordinal",
    )
    .fetch_all(pool)
    .await
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
            immediate_retry_policy(),
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

/// INV-069: a completed judge escalation in a repository-watch-created session
/// cannot retain the singleton as unattended active work. Its normal
/// failed-turn/blocked-goal closeout releases the dispatch, leaves an auditable
/// escalation record, and makes the same event eligible for a fresh dispatch.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn headless_approval_escalation_releases_rearms_and_redispatches()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let seed = 0x50_240;
    let (model_repository, prepared, turn, requests) =
        checkpoint_dispatched_delegated_approval(&fixture, seed).await?;
    let [request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
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
                    closed_request.as_uuid().as_u128() + CLOSED_RESULT_ID_OFFSET,
                ))
            },
        )
        .await?;
    let audit: HeadlessEscalationVisibility = sqlx::query_as(
        "SELECT lifecycle.state_kind AS lifecycle_state,
                    lifecycle.terminal_disposition_kind AS terminal_disposition,
                    latest_goal.event_kind AS goal_event_kind,
                    latest_goal.blocked_reason,
                    audit.rationale,
                    audit.released_at IS NOT NULL AS dispatch_released,
                    audit.obligation_id IS NOT NULL AS replacement_owed
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
    // The requeue an escalation owes is a counted failed attempt, so under the
    // production policy it waits out that delay before redispatching; this
    // reads it through the immediate policy, because what is under test is the
    // release and the replacement it opens rather than the delay's own bounds.
    let obligation = load_next_obligation(&fixture)
        .await?
        .expect("the headless escalation obligation is ready after release");
    let cursor = PostgresRepoWatchStore::new(fixture.pool.clone())
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    let (_successor_dispatch, successor_sessions) = dispatched(
        evaluate_obligation(&fixture, obligation, cursor.candidate().observation()).await?,
    );

    assert_eq!(
        authority.dispatch(),
        ApprovalJudgeDispatchProvenance::RepoWatch(fixture.dispatch_id)
    );
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
        CompleteApprovalJudgeOutcome::HeadlessEscalationTerminalized
    );
    assert_eq!(audit.lifecycle_state, "terminal");
    assert_eq!(audit.terminal_disposition.as_deref(), Some("failed"));
    assert_eq!(audit.goal_event_kind, "blocked");
    assert_eq!(audit.blocked_reason.as_deref(), Some("execution_failure"));
    assert_eq!(audit.rationale, rationale.as_str());
    assert!(audit.dispatch_released);
    assert!(audit.replacement_owed);
    assert_ne!(successor_sessions[0], fixture.session(0));
    assert_eq!(prepared.request().id(), *request);
    assert_eq!(turn, prepared.request().turn());
    Ok(())
}

/// INV-069: the dispatch fence describes the generation the dispatch
/// commissioned and nothing else. A session that goes on to accept an unrelated
/// successor goal judges that goal's requests without the fence, so its
/// escalation parks for the user whose goal it is instead of taking the
/// headless path that fails the turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_successor_generation_is_judged_without_the_dispatch_fence() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let session = fixture.session(0);
    let successor_turn = TurnId::from_uuid(Uuid::from_u128(SUCCESSOR_JUDGE_TURN_ID));
    assert_applied_goal_command(
        GoalRepository::new(fixture.pool.clone())
            .handle_user_command(
                GoalUserCommand::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(
                        SUCCESSOR_JUDGE_SUPERSEDE_COMMAND_ID,
                    )),
                    session,
                    GoalUserAction::Supersede(GoalStatement::try_new(String::from(
                        "an unrelated successor goal this session accepted",
                    ))?),
                ),
                Some(GoalTurnCandidates::new(
                    AcceptedInputId::from_uuid(Uuid::from_u128(SUCCESSOR_JUDGE_INPUT_ID)),
                    successor_turn,
                )),
                |_| None,
            )
            .await?,
    );

    let (_repository, prepared, judged_turn, _requests) =
        checkpoint_dispatched_delegated_approval(&fixture, 0x50_280).await?;

    assert_eq!(
        judged_turn, successor_turn,
        "supersession retires the dispatched turn, so the successor's turn is the eligible one"
    );
    assert!(
        prepared.session_context().dispatch().is_none(),
        "the dispatch authority describes only the generation it commissioned"
    );
    Ok(())
}

/// A headless escalation terminalizes its turn under three fresh identities.
/// Replaying the completion with any other one is a structurally different call
/// rather than the same one twice, so it is reported instead of replayed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_headless_escalation_replay_binds_every_terminal_identity() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let seed = 0x50_2a0;
    let (model_repository, prepared, _turn, _requests) =
        checkpoint_dispatched_delegated_approval(&fixture, seed).await?;
    let approval_repository = model_repository.approval_judge_repository();
    approval_repository.authorize(&prepared).await?;
    let rationale = ToolDecisionRationale::try_new(String::from(
        "the provider requests authority beyond the immutable dispatch fence",
    ))?;
    let identities = ApprovalJudgeCompletionIdentities::new(
        TurnAttemptId::from_uuid(Uuid::from_u128(seed + 10)),
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 11)),
        ContextFrontierId::from_uuid(Uuid::from_u128(seed + 12)),
    );
    let complete = async |identities| {
        approval_repository
            .complete(
                &prepared,
                DelegateApprovalRecommendation::EscalateToHuman,
                rationale.clone(),
                ProviderReportedTokenUsage::unreported(),
                identities,
                |closed_request| {
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                        closed_request.as_uuid().as_u128() + CLOSED_RESULT_ID_OFFSET,
                    ))
                },
            )
            .await
    };
    let escalated = complete(identities).await?;

    let other_failure_entry = complete(ApprovalJudgeCompletionIdentities::new(
        identities.continuation_attempt(),
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 31)),
        identities.terminal_frontier(),
    ))
    .await
    .expect_err("a replay naming another failure entry is not the same completion");
    let other_terminal_frontier = complete(ApprovalJudgeCompletionIdentities::new(
        identities.continuation_attempt(),
        identities.failure_entry(),
        ContextFrontierId::from_uuid(Uuid::from_u128(seed + 32)),
    ))
    .await
    .expect_err("a replay naming another terminal frontier is not the same completion");
    let other_attempt = complete(ApprovalJudgeCompletionIdentities::new(
        TurnAttemptId::from_uuid(Uuid::from_u128(seed + 30)),
        identities.failure_entry(),
        identities.terminal_frontier(),
    ))
    .await
    .expect_err("a replay naming another terminal attempt is not the same completion");
    let replayed = complete(identities).await?;

    assert_eq!(
        escalated,
        CompleteApprovalJudgeOutcome::HeadlessEscalationTerminalized
    );
    assert_eq!(release_count(&fixture).await?, 1);
    assert_mismatched_replay(other_failure_entry);
    assert_mismatched_replay(other_terminal_frontier);
    assert_mismatched_replay(other_attempt);
    assert_eq!(
        replayed, escalated,
        "the completion still replays under the identities it committed"
    );
    Ok(())
}

/// Fails naming the error a mismatched headless replay produced, so an
/// unrelated failure is not read as the replay refusal under test.
#[track_caller]
fn assert_mismatched_replay(error: ApprovalJudgeRepositoryError) {
    let ApprovalJudgeRepositoryError::Corruption(ApprovalJudgeCorruption::Inconsistent(
        "completed judge replay",
    )) = error
    else {
        panic!("a mismatched headless replay is reported as one: {error:?}")
    };
}

/// A denial decided earlier in the same delegated batch remains a denial when
/// a later request escalates and terminalizes the unattended turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn headless_escalation_preserves_an_earlier_delegate_denial() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let seed = 0x50_260;
    let (model_repository, first, turn, requests) = checkpoint_dispatched_delegated_approval_for(
        &fixture,
        seed,
        &[("exec", "{}"), ("workspace", "{}")],
    )
    .await?;
    let [denied_request, escalated_request] = requests.as_slice() else {
        panic!("the fixture has two delegated requests")
    };
    let repository = model_repository.approval_judge_repository();
    repository.authorize(&first).await?;
    let denied = repository
        .complete(
            &first,
            DelegateApprovalRecommendation::Deny,
            ToolDecisionRationale::try_new(String::from("the first request is denied"))?,
            ProviderReportedTokenUsage::unreported(),
            ApprovalJudgeCompletionIdentities::new(
                TurnAttemptId::from_uuid(Uuid::from_u128(seed + 30)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 31)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 32)),
            ),
            |request| {
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    request.as_uuid().as_u128() + CLOSED_RESULT_ID_OFFSET,
                ))
            },
        )
        .await?;
    let second = ready_approval_judge(
        repository
            .prepare(
                fixture.session(0),
                turn,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 40)),
                None,
            )
            .await?,
    );
    repository.authorize(&second).await?;
    let escalated = repository
        .complete(
            &second,
            DelegateApprovalRecommendation::EscalateToHuman,
            ToolDecisionRationale::try_new(String::from("the second request needs a human"))?,
            ProviderReportedTokenUsage::unreported(),
            ApprovalJudgeCompletionIdentities::new(
                TurnAttemptId::from_uuid(Uuid::from_u128(seed + 41)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 42)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 43)),
            ),
            |request| {
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    request.as_uuid().as_u128() + CLOSED_RESULT_ID_OFFSET,
                ))
            },
        )
        .await?;
    let result_kinds: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT tool_result_request_id, payload_kind
           FROM semantic_transcript_entry
          WHERE tool_result_request_id IN ($1, $2)
          ORDER BY tool_result_request_id",
    )
    .bind(denied_request.as_uuid())
    .bind(escalated_request.as_uuid())
    .fetch_all(&fixture.pool)
    .await?;

    assert_eq!(denied, CompleteApprovalJudgeOutcome::Decided);
    assert_eq!(
        escalated,
        CompleteApprovalJudgeOutcome::HeadlessEscalationTerminalized
    );
    assert_eq!(release_count(&fixture).await?, 1);
    assert_eq!(
        result_kinds,
        vec![
            (*denied_request.as_uuid(), String::from("tool_denied")),
            (
                *escalated_request.as_uuid(),
                String::from("tool_closed_by_turn_end"),
            ),
        ]
    );
    Ok(())
}

/// A steer accepted while the dispatched turn awaits its judge is a user
/// attending the session, and terminalizing that turn would strand the steer
/// against `turn_lifecycle_pending_steering_closed`. The escalation parks for
/// that user instead, leaving the turn active and the dispatch owned.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_steered_dispatched_turn_escalates_to_its_user() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let seed = 0x50_2c0;
    let (model_repository, prepared, turn, _requests) =
        checkpoint_dispatched_delegated_approval(&fixture, seed).await?;
    SubmitInputRepository::new(fixture.pool.clone())
        .handle_with_candidates(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(STEERED_DISPATCH_COMMAND_ID)),
                fixture.session(0),
                UserContent::try_text(String::from("narrow the change to the failing test"))
                    .expect("the fixture steering content is admitted"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: turn,
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(STEERED_DISPATCH_INPUT_ID)),
            None,
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 60)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 61)),
            ),
            |_| panic!("the steer's source turn is still active"),
            |_| panic!("the steer cancels no tool request without a terminal observation"),
        )
        .await?;
    let approval_repository = model_repository.approval_judge_repository();
    approval_repository.authorize(&prepared).await?;

    let outcome = approval_repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::EscalateToHuman,
            ToolDecisionRationale::try_new(String::from(
                "the provider requests authority beyond the immutable dispatch fence",
            ))?,
            ProviderReportedTokenUsage::unreported(),
            ApprovalJudgeCompletionIdentities::new(
                TurnAttemptId::from_uuid(Uuid::from_u128(seed + 10)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 11)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 12)),
            ),
            |closed_request| {
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    closed_request.as_uuid().as_u128() + CLOSED_RESULT_ID_OFFSET,
                ))
            },
        )
        .await?;
    let lifecycle: String = sqlx::query_scalar(
        "SELECT state_kind FROM turn_lifecycle WHERE turn_id = $1 AND session_id = $2",
    )
    .bind(turn.as_uuid())
    .bind(fixture.session(0).as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let escalations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_watch_headless_approval_escalation WHERE session_id = $1",
    )
    .bind(fixture.session(0).as_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(outcome, CompleteApprovalJudgeOutcome::EscalatedToHuman);
    assert_eq!(lifecycle, "active");
    assert_eq!(escalations, 0);
    assert_eq!(release_count(&fixture).await?, 0);
    Ok(())
}

/// Once the dispatch has released, the unattended path has nothing left to do:
/// it cannot free a singleton this session no longer holds, and
/// `repo_watch_owe_dispatch_requeue` owes no second replacement. Work an
/// operator resumed by hand from that state therefore escalates to that
/// operator instead of failing another turn under a promise nothing keeps.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_released_dispatch_escalates_resumed_work_to_its_operator() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let seed = 0x50_2e0;
    let (model_repository, prepared, _turn, _requests) =
        checkpoint_dispatched_delegated_approval(&fixture, seed).await?;
    let approval_repository = model_repository.approval_judge_repository();
    approval_repository.authorize(&prepared).await?;
    let rationale = ToolDecisionRationale::try_new(String::from(
        "the provider requests authority beyond the immutable dispatch fence",
    ))?;
    let unattended = approval_repository
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
                    closed_request.as_uuid().as_u128() + CLOSED_RESULT_ID_OFFSET,
                ))
            },
        )
        .await?;
    let released = release_count(&fixture).await?;

    // The operator takes the blocked goal back by hand. The resumed turn is
    // still the generation the dispatch commissioned, so it still resolves the
    // dispatch authority — the released batch, not the missing authority, is
    // what sends its escalation to a user.
    assert_applied_goal_command(
        GoalRepository::new(fixture.pool.clone())
            .handle_user_command(
                GoalUserCommand::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(RESUMED_DISPATCH_COMMAND_ID)),
                    fixture.session(0),
                    GoalUserAction::Resume(None),
                ),
                Some(GoalTurnCandidates::new(
                    AcceptedInputId::from_uuid(Uuid::from_u128(RESUMED_DISPATCH_INPUT_ID)),
                    TurnId::from_uuid(Uuid::from_u128(RESUMED_DISPATCH_TURN_ID)),
                )),
                |_| None,
            )
            .await?,
    );
    let (resumed_repository, resumed_prepared, resumed_turn, _resumed_requests) =
        checkpoint_dispatched_delegated_approval(&fixture, 0x50_300).await?;
    let resumed_approvals = resumed_repository.approval_judge_repository();
    resumed_approvals.authorize(&resumed_prepared).await?;

    let outcome = resumed_approvals
        .complete(
            &resumed_prepared,
            DelegateApprovalRecommendation::EscalateToHuman,
            rationale,
            ProviderReportedTokenUsage::unreported(),
            ApprovalJudgeCompletionIdentities::new(
                TurnAttemptId::from_uuid(Uuid::from_u128(0x50_300 + 10)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x50_300 + 11)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x50_300 + 12)),
            ),
            |closed_request| {
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    closed_request.as_uuid().as_u128() + CLOSED_RESULT_ID_OFFSET + 1,
                ))
            },
        )
        .await?;
    let lifecycle: String = sqlx::query_scalar(
        "SELECT state_kind FROM turn_lifecycle WHERE turn_id = $1 AND session_id = $2",
    )
    .bind(resumed_turn.as_uuid())
    .bind(fixture.session(0).as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let escalations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_watch_headless_approval_escalation WHERE session_id = $1",
    )
    .bind(fixture.session(0).as_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(
        unattended,
        CompleteApprovalJudgeOutcome::HeadlessEscalationTerminalized
    );
    assert_eq!(released, 1);
    assert_eq!(
        resumed_turn,
        TurnId::from_uuid(Uuid::from_u128(RESUMED_DISPATCH_TURN_ID))
    );
    assert_eq!(outcome, CompleteApprovalJudgeOutcome::EscalatedToHuman);
    assert_eq!(lifecycle, "active");
    assert_eq!(
        escalations, 1,
        "the resumed escalation records no second audit row"
    );
    assert_eq!(release_count(&fixture).await?, 1);
    Ok(())
}

/// A sibling action still pursuing keeps the batch unreleased, so the release
/// row cannot say whether a person is behind the work. The escalation record
/// can: only an operator can resume a goal an unattended escalation blocked, so
/// the resumed turn's own escalation parks for them even though nothing has
/// released.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_resumption_before_release_escalates_to_its_operator() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let seed = 0x50_360;
    let (model_repository, prepared, _turn, _requests) =
        checkpoint_dispatched_delegated_approval(&fixture, seed).await?;
    let approval_repository = model_repository.approval_judge_repository();
    approval_repository.authorize(&prepared).await?;
    let rationale = ToolDecisionRationale::try_new(String::from(
        "the provider requests authority beyond the immutable dispatch fence",
    ))?;
    let unattended = approval_repository
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
                    closed_request.as_uuid().as_u128() + CLOSED_RESULT_ID_OFFSET,
                ))
            },
        )
        .await?;
    let held = release_count(&fixture).await?;

    let resumed_turn = TurnId::from_uuid(Uuid::from_u128(HELD_BATCH_RESUME_TURN_ID));
    assert_applied_goal_command(
        GoalRepository::new(fixture.pool.clone())
            .handle_user_command(
                GoalUserCommand::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(HELD_BATCH_RESUME_COMMAND_ID)),
                    fixture.session(0),
                    GoalUserAction::Resume(None),
                ),
                Some(GoalTurnCandidates::new(
                    AcceptedInputId::from_uuid(Uuid::from_u128(HELD_BATCH_RESUME_INPUT_ID)),
                    resumed_turn,
                )),
                |_| None,
            )
            .await?,
    );
    let (resumed_repository, resumed_prepared, judged_turn, _resumed_requests) =
        checkpoint_dispatched_delegated_approval(&fixture, 0x50_380).await?;
    let resumed_approvals = resumed_repository.approval_judge_repository();
    resumed_approvals.authorize(&resumed_prepared).await?;

    let outcome = resumed_approvals
        .complete(
            &resumed_prepared,
            DelegateApprovalRecommendation::EscalateToHuman,
            rationale,
            ProviderReportedTokenUsage::unreported(),
            ApprovalJudgeCompletionIdentities::new(
                TurnAttemptId::from_uuid(Uuid::from_u128(0x50_380 + 10)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x50_380 + 11)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x50_380 + 12)),
            ),
            |closed_request| {
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    closed_request.as_uuid().as_u128() + CLOSED_RESULT_ID_OFFSET + 1,
                ))
            },
        )
        .await?;
    let lifecycle: String = sqlx::query_scalar(
        "SELECT state_kind FROM turn_lifecycle WHERE turn_id = $1 AND session_id = $2",
    )
    .bind(judged_turn.as_uuid())
    .bind(fixture.session(0).as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let escalations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_watch_headless_approval_escalation WHERE session_id = $1",
    )
    .bind(fixture.session(0).as_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(
        unattended,
        CompleteApprovalJudgeOutcome::HeadlessEscalationTerminalized
    );
    assert_eq!(
        held, 0,
        "the sibling action still pursues, so nothing released"
    );
    assert_eq!(judged_turn, resumed_turn);
    assert_eq!(outcome, CompleteApprovalJudgeOutcome::EscalatedToHuman);
    assert_eq!(lifecycle, "active");
    assert_eq!(
        escalations, 1,
        "the resumed escalation records no second audit row"
    );
    assert_eq!(release_count(&fixture).await?, 0);
    Ok(())
}

/// A released batch parks only while a person is still behind the work. The
/// reachable stale case is the resumed one: an earlier escalation terminalized
/// its turn and released the batch, an operator resumed the goal, and the goal
/// then ended while the resumed turn's judge was in flight. That escalation is
/// terminalized rather than parked for nobody, and with the authority already
/// ended no execution-failure block is appended and no requeue is owed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_released_dispatch_terminalizes_work_whose_goal_ended() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let seed = 0x50_320;
    let (model_repository, prepared, _turn, _requests) =
        checkpoint_dispatched_delegated_approval(&fixture, seed).await?;
    let approval_repository = model_repository.approval_judge_repository();
    approval_repository.authorize(&prepared).await?;
    let rationale = ToolDecisionRationale::try_new(String::from(
        "the provider requests authority beyond the immutable dispatch fence",
    ))?;
    let unattended = approval_repository
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
                    closed_request.as_uuid().as_u128() + CLOSED_RESULT_ID_OFFSET,
                ))
            },
        )
        .await?;
    let released = release_count(&fixture).await?;

    let stale_turn = TurnId::from_uuid(Uuid::from_u128(STALE_DISPATCH_TURN_ID));
    assert_applied_goal_command(
        GoalRepository::new(fixture.pool.clone())
            .handle_user_command(
                GoalUserCommand::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(STALE_DISPATCH_RESUME_COMMAND_ID)),
                    fixture.session(0),
                    GoalUserAction::Resume(None),
                ),
                Some(GoalTurnCandidates::new(
                    AcceptedInputId::from_uuid(Uuid::from_u128(STALE_DISPATCH_INPUT_ID)),
                    stale_turn,
                )),
                |_| None,
            )
            .await?,
    );
    let (resumed_repository, resumed_prepared, resumed_turn, _resumed_requests) =
        checkpoint_dispatched_delegated_approval(&fixture, 0x50_340).await?;
    let resumed_approvals = resumed_repository.approval_judge_repository();
    resumed_approvals.authorize(&resumed_prepared).await?;
    // The goal ends while this judge is in flight, which is what makes the
    // resumed work stale. The turn stays active until the completion below
    // terminalizes it: an active turn is runtime-relevant whatever its goal
    // recorded, so ending the goal releases nothing by itself.
    withdraw_dispatched_goal(
        &fixture.pool,
        fixture.session(0),
        STALE_DISPATCH_STOP_COMMAND_ID,
    )
    .await?;
    let released_before_completion = release_count(&fixture).await?;

    let outcome = resumed_approvals
        .complete(
            &resumed_prepared,
            DelegateApprovalRecommendation::EscalateToHuman,
            rationale,
            ProviderReportedTokenUsage::unreported(),
            ApprovalJudgeCompletionIdentities::new(
                TurnAttemptId::from_uuid(Uuid::from_u128(0x50_340 + 10)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x50_340 + 11)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x50_340 + 12)),
            ),
            |closed_request| {
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    closed_request.as_uuid().as_u128() + CLOSED_RESULT_ID_OFFSET + 1,
                ))
            },
        )
        .await?;
    let lifecycle: String = sqlx::query_scalar(
        "SELECT state_kind FROM turn_lifecycle WHERE turn_id = $1 AND session_id = $2",
    )
    .bind(resumed_turn.as_uuid())
    .bind(fixture.session(0).as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let latest_goal_event: String = sqlx::query_scalar(
        "SELECT event_kind
           FROM goal_event
          WHERE session_id = $1
          ORDER BY event_ordinal DESC
          LIMIT 1",
    )
    .bind(fixture.session(0).as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let escalations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_watch_headless_approval_escalation WHERE session_id = $1",
    )
    .bind(fixture.session(0).as_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(
        unattended,
        CompleteApprovalJudgeOutcome::HeadlessEscalationTerminalized
    );
    assert_eq!(released, 1);
    assert_eq!(resumed_turn, stale_turn);
    assert_eq!(
        released_before_completion, 1,
        "ending the goal releases nothing further while the resumed turn is active"
    );
    assert_eq!(
        outcome,
        CompleteApprovalJudgeOutcome::HeadlessEscalationTerminalized
    );
    assert_eq!(lifecycle, "terminal");
    assert_eq!(
        latest_goal_event, "user_stopped",
        "an ended generation records no execution-failure block"
    );
    assert_eq!(escalations, 2);
    assert_eq!(release_count(&fixture).await?, 1);
    Ok(())
}

/// Terminalizing one action in a multi-action dispatch does not claim release
/// while its sibling remains pursuing, and exact replay reports the same
/// durable effect — including after the batch releases, which is a fact about
/// the batch and never about this completion.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn headless_escalation_waits_for_a_multi_action_dispatch_sibling()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let seed = 0x50_280;
    let (model_repository, prepared, _, _) =
        checkpoint_dispatched_delegated_approval(&fixture, seed).await?;
    let repository = model_repository.approval_judge_repository();
    repository.authorize(&prepared).await?;
    let rationale = ToolDecisionRationale::try_new(String::from(
        "the unattended request needs a human decision",
    ))?;

    let outcome = repository
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
            |request| {
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    request.as_uuid().as_u128() + CLOSED_RESULT_ID_OFFSET,
                ))
            },
        )
        .await?;
    let replay = repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::EscalateToHuman,
            rationale,
            ProviderReportedTokenUsage::unreported(),
            ApprovalJudgeCompletionIdentities::new(
                TurnAttemptId::from_uuid(Uuid::from_u128(seed + 10)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 11)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 12)),
            ),
            |request| {
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    request.as_uuid().as_u128() + CLOSED_RESULT_ID_OFFSET,
                ))
            },
        )
        .await?;

    let pending_release = release_count(&fixture).await?;
    // Stands in for the sibling action settling, which is the only way this
    // batch releases and is not this completion's doing either way.
    sqlx::query("INSERT INTO repo_watch_dispatch_release (dispatch_id) VALUES ($1)")
        .bind(fixture.dispatch_id.as_uuid())
        .execute(&fixture.pool)
        .await?;
    let replay_after_release = repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::EscalateToHuman,
            ToolDecisionRationale::try_new(String::from(
                "the unattended request needs a human decision",
            ))?,
            ProviderReportedTokenUsage::unreported(),
            ApprovalJudgeCompletionIdentities::new(
                TurnAttemptId::from_uuid(Uuid::from_u128(seed + 10)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 11)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 12)),
            ),
            |request| {
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    request.as_uuid().as_u128() + CLOSED_RESULT_ID_OFFSET,
                ))
            },
        )
        .await?;

    assert_eq!(
        outcome,
        CompleteApprovalJudgeOutcome::HeadlessEscalationTerminalized
    );
    assert_eq!(replay, outcome);
    assert_eq!(replay_after_release, outcome);
    assert_eq!(pending_release, 0);
    assert_eq!(release_count(&fixture).await?, 1);
    Ok(())
}

/// The delay between attempts is what keeps a lineage that keeps failing from
/// re-dispatching at the poll cadence, so it must hold back an obligation the
/// singleton and cooldown would otherwise release immediately.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_failed_attempt_waits_out_its_delay_before_redispatch() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    withdraw_dispatched_goal(
        &fixture.pool,
        fixture.session(0),
        PARK_FIRST_STOP_COMMAND_ID,
    )
    .await?;

    let delayed = fixture
        .store
        .load_next_dispatch_obligation(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
            RepoWatchDispatchRetryPolicy::production(),
        )
        .await?;
    let undelayed = fixture
        .store
        .load_next_dispatch_obligation(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
            immediate_retry_policy(),
        )
        .await?;

    assert!(delayed.is_none());
    assert_eq!(
        undelayed.map(|obligation| obligation.failed_attempts()),
        Some(1)
    );
    Ok(())
}

/// The spend ordering numbers one repository's event stream, so it can only
/// order facts about the same target. A collapsed singleton spends facts about
/// whichever pull request it stalled on at the time, and comparing those across
/// targets would let a target that had reached a higher cursor position hold a
/// lineage parked on one whose own numbering is lower.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_spend_on_another_target_does_not_order_this_target_progress()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(repository_scoped_conflict_rule()?).await?;
    withdraw_dispatched_goal(
        &fixture.pool,
        fixture.session(0),
        CROSS_TARGET_SPEND_STOP_COMMAND_ID,
    )
    .await?;
    // Progress on the stalled pull request, recorded before the neighbour's
    // fact and therefore at a lower cursor position than it takes.
    commit_lifecycle(
        &fixture,
        observation(context(SECOND_HEAD)?)?,
        conflict_event(CROSS_TARGET_SPEND_PROGRESS_EVENT_ID, SECOND_HEAD)?,
    )
    .await?;
    // The neighbour's fact, at the higher cursor position. Committed and left
    // unevaluated: what this test needs from it is that it is durable and that
    // the lineage records a spend on it.
    let neighbour = same_repository_context(SameRepositoryContextFacts {
        number: TOP_PULL_REQUEST_NUMBER,
        head: THIRD_HEAD,
        base_branch: BASE_BRANCH,
        head_branch: TOP_AGENT_BRANCH,
    })?;
    commit_lifecycle(
        &fixture,
        mergeable_observation(vec![context(SECOND_HEAD)?, neighbour.clone()], Vec::new())?,
        conflict_event_for(CROSS_TARGET_SPEND_NEIGHBOUR_EVENT_ID, neighbour)?,
    )
    .await?;
    // A spend this lineage took on the neighbour, which the ordering arm must
    // not weigh against progress on the pull request it stalled on. Written
    // directly because the machinery only ever spends facts about the stalled
    // target, which is the asymmetry under test.
    sqlx::query(
        "INSERT INTO repo_watch_dispatch_obligation_park
            (obligation_id, transition_ordinal, transition_kind, failed_attempts,
             release_reason, release_event_id)
         SELECT obligation_id, 1, 'released', 6, 'pull_request_progress', $1
           FROM repo_watch_dispatch_obligation
          WHERE settled_kind IS NULL",
    )
    .bind(Uuid::from_u128(CROSS_TARGET_SPEND_NEIGHBOUR_EVENT_ID))
    .execute(&fixture.pool)
    .await?;

    exhaust_and_park(&fixture).await?;

    let latest_release: Option<Uuid> = sqlx::query_scalar(
        "SELECT release_event_id
           FROM repo_watch_dispatch_obligation_park
          WHERE release_event_id IS NOT NULL
          ORDER BY transition_ordinal DESC
          LIMIT 1",
    )
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        latest_release,
        Some(Uuid::from_u128(CROSS_TARGET_SPEND_PROGRESS_EVENT_ID))
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM repo_watch_parked_dispatch_obligation")
            .fetch_one(&fixture.pool)
            .await?,
        0
    );
    assert_eq!(outstanding_failed_attempts(&fixture.pool).await?, 0);
    Ok(())
}

/// Several progress facts can follow the stalled state, and parking spends only
/// the newest of them. The older one stays unevaluated by any rule that lags, so
/// a spend test comparing identity alone would let it release a later park that
/// a newer fact had already been spent on.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_progress_fact_older_than_one_already_spent_releases_nothing()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    withdraw_dispatched_goal(
        &fixture.pool,
        fixture.session(0),
        SPENT_FRONTIER_STOP_COMMAND_ID,
    )
    .await?;
    // Durable and after the stalled state, but never evaluated, so neither is
    // spent by an ordinary release.
    commit_lifecycle(
        &fixture,
        observation(context(SECOND_HEAD)?)?,
        conflict_event(SPENT_FRONTIER_OLDER_EVENT_ID, SECOND_HEAD)?,
    )
    .await?;
    commit_lifecycle(
        &fixture,
        observation(context(THIRD_HEAD)?)?,
        conflict_event(SPENT_FRONTIER_NEWER_EVENT_ID, THIRD_HEAD)?,
    )
    .await?;

    exhaust_and_park(&fixture).await?;
    let spent_on_the_newest: Option<Uuid> = sqlx::query_scalar(
        "SELECT release_event_id
           FROM repo_watch_dispatch_obligation_park
          WHERE release_event_id IS NOT NULL
          ORDER BY transition_ordinal DESC
          LIMIT 1",
    )
    .fetch_one(&fixture.pool)
    .await?;
    exhaust_and_park(&fixture).await?;

    let older_releases: i64 =
        sqlx::query_scalar("SELECT repo_watch_release_dispatch_obligation_parks_for_event($1)")
            .bind(Uuid::from_u128(SPENT_FRONTIER_OLDER_EVENT_ID))
            .fetch_one(&fixture.pool)
            .await?;

    assert_eq!(
        spent_on_the_newest,
        Some(Uuid::from_u128(SPENT_FRONTIER_NEWER_EVENT_ID))
    );
    assert_eq!(older_releases, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM repo_watch_parked_dispatch_obligation")
            .fetch_one(&fixture.pool)
            .await?,
        1
    );
    Ok(())
}

/// Drives the outstanding obligation to its budget and parks it, without
/// spending six real dispatches on a sequence whose subject is the park.
async fn exhaust_and_park(fixture: &DispatchFixture) -> Result<(), Box<dyn Error>> {
    let obligation: Uuid = sqlx::query_scalar(
        "UPDATE repo_watch_dispatch_obligation
            SET failed_attempts = repo_watch_dispatch_attempt_budget(),
                last_failed_attempt_at = clock_timestamp()
          WHERE settled_kind IS NULL
        RETURNING obligation_id",
    )
    .fetch_one(&fixture.pool)
    .await?;
    sqlx::query("SELECT repo_watch_park_exhausted_dispatch_obligation($1)")
        .bind(obligation)
        .execute(&fixture.pool)
        .await?;
    Ok(())
}

/// A batch holds its singleton until every one of its actions is terminal, so a
/// delay measured from the first sibling to fail would be spent while the batch
/// still occupied the slot and the successor would be dispatchable the instant
/// the batch released. The clock therefore starts at the release.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_delay_starts_when_the_whole_batch_releases() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    withdraw_dispatched_goal(
        &fixture.pool,
        fixture.session(0),
        BATCH_DELAY_FIRST_STOP_COMMAND_ID,
    )
    .await?;
    let stamped_at_first_termination = failed_attempt_epoch(&fixture.pool).await?;
    let held = release_count(&fixture).await?;

    withdraw_dispatched_goal(
        &fixture.pool,
        fixture.session(1),
        BATCH_DELAY_SECOND_STOP_COMMAND_ID,
    )
    .await?;

    let stamped_at_release = failed_attempt_epoch(&fixture.pool).await?;
    assert_eq!(held, 0);
    assert_eq!(release_count(&fixture).await?, 1);
    assert!(stamped_at_release > stamped_at_first_termination);
    assert!(stamped_at_release >= batch_release_epoch(&fixture).await?);
    Ok(())
}

/// The unbounded case this budget exists for: a lineage whose sessions keep
/// ending without meeting it stops being dispatched at all, rather than
/// re-dispatching for as long as the rule's cooldown allows.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_lineage_that_spends_its_attempt_budget_parks() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;

    let parked_obligation = park_dispatch_obligation(&fixture, PARK_FIRST_STOP_COMMAND_ID).await?;

    let parked: ParkedObligationVisibility = sqlx::query_as(
        "SELECT obligation_id, failed_attempts, head_sha
           FROM repo_watch_parked_dispatch_obligation",
    )
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(parked.obligation_id, parked_obligation);
    assert_eq!(
        parked.failed_attempts,
        dispatch_attempt_budget(&fixture.pool).await?
    );
    assert_eq!(parked.head_sha.as_deref(), Some(FIRST_HEAD));
    assert!(load_next_obligation(&fixture).await?.is_none());
    assert_eq!(
        park_transitions(&fixture.pool).await?,
        vec![ParkTransitionVisibility {
            transition_kind: String::from("parked"),
            failed_attempts: 6,
            release_reason: None,
            release_actor: None,
        }]
    );
    Ok(())
}

/// Parking is durable state, not a load-time verdict: the projection reports it
/// and the obligation stays out of dispatch after the store is reconstructed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_parked_obligation_survives_a_reconstructed_store() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    park_dispatch_obligation(&fixture, PARK_FIRST_STOP_COMMAND_ID).await?;

    let resumed = PostgresRepoWatchDispatchStore::new(fixture.pool.clone(), credential_pin());
    let loaded = resumed
        .load_next_dispatch_obligation(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
            immediate_retry_policy(),
        )
        .await?;

    assert!(loaded.is_none());
    Ok(())
}

/// An operator asking for another attempt is asking for the allowance a lineage
/// that never failed would have, so the release restores the whole budget.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_operator_release_restores_the_whole_budget() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let parked_obligation = park_dispatch_obligation(&fixture, PARK_FIRST_STOP_COMMAND_ID).await?;

    let release = fixture
        .store
        .release_parked_dispatch_obligation(parked_obligation, PARK_RELEASE_ACTOR)
        .await?;

    let released = load_next_obligation(&fixture).await?;
    assert_eq!(release, RepoWatchObligationParkRelease::Released);
    assert_eq!(
        released.map(|obligation| obligation.failed_attempts()),
        Some(0)
    );
    assert_eq!(
        park_transitions(&fixture.pool).await?,
        vec![
            ParkTransitionVisibility {
                transition_kind: String::from("parked"),
                failed_attempts: 6,
                release_reason: None,
                release_actor: None,
            },
            ParkTransitionVisibility {
                transition_kind: String::from("released"),
                failed_attempts: 6,
                release_reason: Some(String::from("operator")),
                release_actor: Some(String::from(PARK_RELEASE_ACTOR)),
            },
        ]
    );
    Ok(())
}

/// An obligation that is not parked has nothing to release, and reporting that
/// is not an error: an operator racing a release the pull request already
/// earned must not read as storage failure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn releasing_an_unparked_obligation_reports_that_it_was_not_parked()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    withdraw_dispatched_goal(
        &fixture.pool,
        fixture.session(0),
        PARK_FIRST_STOP_COMMAND_ID,
    )
    .await?;
    let outstanding: Uuid = sqlx::query_scalar(
        "SELECT obligation_id
           FROM repo_watch_dispatch_obligation
          WHERE settled_kind IS NULL",
    )
    .fetch_one(&fixture.pool)
    .await?;

    let release = fixture
        .store
        .release_parked_dispatch_obligation(outstanding, PARK_RELEASE_ACTOR)
        .await?;

    assert_eq!(release, RepoWatchObligationParkRelease::NotParked);
    assert!(park_transitions(&fixture.pool).await?.is_empty());
    Ok(())
}

/// A head the parked obligation has not seen is the pull request producing
/// something new, which is what buys another attempt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_new_head_releases_a_parked_obligation() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    park_dispatch_obligation(&fixture, PARK_FIRST_STOP_COMMAND_ID).await?;

    let advanced = evaluate_conflict(&fixture, PARK_RELEASING_EVENT_ID, SECOND_HEAD).await?;

    let released = load_next_obligation(&fixture).await?;
    assert_eq!(advanced.outcome, RepoWatchRuleEvaluationOutcome::Occupied);
    assert_eq!(
        released
            .as_ref()
            .map(|obligation| obligation.failed_attempts()),
        Some(0)
    );
    assert_eq!(
        released.map(|obligation| *obligation.latest_event().id().as_uuid()),
        Some(*advanced.event_id.as_uuid())
    );
    assert_eq!(
        park_transitions(&fixture.pool)
            .await?
            .last()
            .map(|transition| transition.release_reason.clone()),
        Some(Some(String::from("pull_request_progress")))
    );
    Ok(())
}

/// A parked lineage still holds its singleton. If its stalled pull request
/// closes while the latest-event projection points at a neighbour, settling
/// only through that projection would record the close as handled and leave the
/// singleton held forever.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn closing_the_stalled_pull_request_settles_a_parked_obligation() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture_for(repository_scoped_conflict_rule()?).await?;
    park_dispatch_obligation(&fixture, PARK_FIRST_STOP_COMMAND_ID).await?;
    evaluate_neighbour_conflict(&fixture).await?;
    commit_merge(&fixture, PARKED_TARGET_CUTOFF_EVENT_ID).await?;

    fixture
        .store
        .process_next_lifecycle_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(PARKED_TARGET_CUTOFF_COMMAND_ID))
        })
        .await?;

    let settlement: Option<String> = sqlx::query_scalar(
        "SELECT settled_kind
           FROM repo_watch_dispatch_obligation
          WHERE parked_state_event_id IS NOT NULL",
    )
    .fetch_one(&fixture.pool)
    .await?;
    let parked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_parked_dispatch_obligation")
            .fetch_one(&fixture.pool)
            .await?;
    assert_eq!(settlement.as_deref(), Some("target_closed"));
    assert_eq!(parked, 0);
    Ok(())
}

/// The head can move while the exhausting attempt is still running, under an
/// event no rule of the lineage matches. Nothing restates that head afterwards,
/// so a park that only ever read matched events would strand the lineage on a
/// state the pull request had already left.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_head_that_moved_during_the_last_attempt_releases_the_park() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let second =
        spend_one_attempt(&fixture, fixture.session(0), PARK_FIRST_STOP_COMMAND_ID).await?;
    let third = spend_one_attempt(&fixture, second, PARK_FIRST_STOP_COMMAND_ID + 1).await?;
    let fourth = spend_one_attempt(&fixture, third, PARK_FIRST_STOP_COMMAND_ID + 2).await?;
    let fifth = spend_one_attempt(&fixture, fourth, PARK_FIRST_STOP_COMMAND_ID + 3).await?;
    let sixth = spend_one_attempt(&fixture, fifth, PARK_FIRST_STOP_COMMAND_ID + 4).await?;

    evaluate_nonmatching_head_change(&fixture).await?;
    withdraw_dispatched_goal(&fixture.pool, sixth, PARK_FIRST_STOP_COMMAND_ID + 5).await?;

    let parked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_parked_dispatch_obligation")
            .fetch_one(&fixture.pool)
            .await?;
    assert_eq!(parked, 0);
    assert_eq!(outstanding_failed_attempts(&fixture.pool).await?, 0);
    assert!(load_next_obligation(&fixture).await?.is_some());
    Ok(())
}

/// The stalled target must survive a neighbour's match. A collapsed singleton
/// advances its latest-event projection on any matching event, so reading the
/// stalled target from that projection would hand the release condition to the
/// neighbour and take it away from the pull request that actually stalled.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_neighbours_match_does_not_move_the_parked_target() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(repository_scoped_conflict_rule()?).await?;
    park_dispatch_obligation(&fixture, PARK_FIRST_STOP_COMMAND_ID).await?;
    evaluate_neighbour_conflict(&fixture).await?;

    let progress = evaluate_nonmatching_head_change(&fixture).await?;

    let released = load_next_obligation(&fixture).await?;
    assert_eq!(progress, RepoWatchRuleEvaluationOutcome::NotMatched);
    assert_eq!(
        released.map(|obligation| obligation.failed_attempts()),
        Some(0)
    );
    Ok(())
}

/// A rule watching one narrow signal parks on a pull request that keeps
/// failing, and the head then moves under an event that rule never matches.
/// Reading progress only from matching events would leave that obligation
/// parked on an obsolete head until an operator intervened.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_nonmatching_head_change_releases_a_parked_obligation() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    park_dispatch_obligation(&fixture, PARK_FIRST_STOP_COMMAND_ID).await?;

    let progress = evaluate_nonmatching_head_change(&fixture).await?;

    let released = load_next_obligation(&fixture).await?;
    assert_eq!(progress, RepoWatchRuleEvaluationOutcome::NotMatched);
    assert_eq!(
        released.map(|obligation| obligation.failed_attempts()),
        Some(0)
    );
    assert_eq!(
        park_transitions(&fixture.pool)
            .await?
            .last()
            .map(|transition| transition.release_reason.clone()),
        Some(Some(String::from("pull_request_progress")))
    );
    Ok(())
}

/// One batch is one attempt however many of its actions fail. Counting each
/// sibling would spend the budget several times faster than the delay assumes,
/// and would let a later sibling recompute a count that a release taken since
/// the first sibling terminated had already cleared.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn one_batch_counts_one_attempt_however_many_siblings_fail() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;

    withdraw_dispatched_goal(
        &fixture.pool,
        fixture.session(0),
        SIBLING_COUNT_FIRST_STOP_COMMAND_ID,
    )
    .await?;
    let after_first_sibling = outstanding_failed_attempts(&fixture.pool).await?;
    withdraw_dispatched_goal(
        &fixture.pool,
        fixture.session(1),
        SIBLING_COUNT_SECOND_STOP_COMMAND_ID,
    )
    .await?;

    assert_eq!(after_first_sibling, 1);
    assert_eq!(outstanding_failed_attempts(&fixture.pool).await?, 1);
    Ok(())
}

/// The same guard seen from the consequence that matters: a release taken while
/// a sibling of the counted batch is still running must survive that sibling's
/// own termination, or the pull request's progress is silently discarded and
/// the lineage reparks on state it has already been given.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_release_survives_a_later_siblings_termination() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    withdraw_dispatched_goal(
        &fixture.pool,
        fixture.session(0),
        SIBLING_COUNT_FIRST_STOP_COMMAND_ID,
    )
    .await?;
    let obligation: Uuid = sqlx::query_scalar(
        "UPDATE repo_watch_dispatch_obligation
            SET failed_attempts = repo_watch_dispatch_attempt_budget()
          WHERE settled_kind IS NULL
        RETURNING obligation_id",
    )
    .fetch_one(&fixture.pool)
    .await?;
    sqlx::query("SELECT repo_watch_park_exhausted_dispatch_obligation($1)")
        .bind(obligation)
        .execute(&fixture.pool)
        .await?;
    sqlx::query("SELECT repo_watch_release_parked_dispatch_obligation($1, $2)")
        .bind(obligation)
        .bind(PARK_RELEASE_ACTOR)
        .execute(&fixture.pool)
        .await?;

    withdraw_dispatched_goal(
        &fixture.pool,
        fixture.session(1),
        SIBLING_COUNT_SECOND_STOP_COMMAND_ID,
    )
    .await?;

    let parked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_parked_dispatch_obligation")
            .fetch_one(&fixture.pool)
            .await?;
    assert_eq!(outstanding_failed_attempts(&fixture.pool).await?, 0);
    assert_eq!(parked, 0);
    Ok(())
}

/// Rule, repository, and stack singletons collapse many pull requests onto one
/// obligation. Comparing heads alone would then let ordinary traffic on any
/// neighbour restore the budget of the one pull request that keeps failing,
/// because a neighbour's head almost never matches the stalled one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn another_pull_requests_progress_leaves_a_parked_obligation_parked()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(repository_scoped_conflict_rule()?).await?;
    park_dispatch_obligation(&fixture, PARK_FIRST_STOP_COMMAND_ID).await?;

    let neighbour = evaluate_neighbour_conflict(&fixture).await?;

    let parked: ParkedObligationVisibility = sqlx::query_as(
        "SELECT obligation_id, failed_attempts, head_sha
           FROM repo_watch_parked_dispatch_obligation",
    )
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(neighbour, RepoWatchRuleEvaluationOutcome::Occupied);
    assert_eq!(
        parked.failed_attempts,
        dispatch_attempt_budget(&fixture.pool).await?
    );
    assert_eq!(parked.head_sha.as_deref(), Some(FIRST_HEAD));
    assert!(load_next_obligation(&fixture).await?.is_none());
    Ok(())
}

/// A rule that matches recomputed state on an unchanged head would otherwise
/// let churn against a hostile pull request buy attempts without limit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_matching_event_on_the_parked_head_buys_no_further_attempt() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    park_dispatch_obligation(&fixture, PARK_FIRST_STOP_COMMAND_ID).await?;

    let unchanged = evaluate_restated_conflict(&fixture).await?;

    let collapsed: ParkedObligationVisibility = sqlx::query_as(
        "SELECT obligation_id, failed_attempts, head_sha
           FROM repo_watch_parked_dispatch_obligation",
    )
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(unchanged, RepoWatchRuleEvaluationOutcome::Occupied);
    assert_eq!(
        collapsed.failed_attempts,
        dispatch_attempt_budget(&fixture.pool).await?
    );
    assert!(load_next_obligation(&fixture).await?.is_none());
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
            immediate_retry_policy(),
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
            immediate_retry_policy(),
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
            immediate_retry_policy(),
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

/// A merge-ready exact head ends the repository-watch commission and releases
/// the singleton held by its dispatched session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn converged_head_releases_singleton_after_ending_commission() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let session = fixture.session(0);
    record_merge_ready_head(&fixture, 0x54_000, FIRST_HEAD).await?;

    let processed = fixture
        .store
        .process_next_convergence_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x54_100))
        })
        .await?;
    let replayed = fixture
        .store
        .process_next_convergence_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x54_100))
        })
        .await?;
    let goal = GoalRepository::new(fixture.pool.clone())
        .load_goal(session)
        .await?
        .expect("the dispatched goal remains readable");
    let visible: (String, String, i32, i64) = sqlx::query_as(
        "SELECT head_sha, verdict_kind, unresolved_thread_count,
                gating_check_count
           FROM repo_watch_current_pull_request_convergence",
    )
    .fetch_one(&fixture.pool)
    .await?;
    let cutoff_goal_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_convergence_cutoff_goal
          WHERE session_id = $1",
    )
    .bind(session.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert!(processed);
    assert!(!replayed);
    assert_eq!(goal.current().state(), &GoalState::UserStopped);
    assert_eq!(
        visible,
        (
            fixture.observation.state().pull_requests()[0]
                .context()
                .head_sha()
                .as_str()
                .to_owned(),
            "merge_ready".to_owned(),
            0,
            0,
        )
    );
    assert_eq!(cutoff_goal_count, 1);
    assert_eq!(release_count(&fixture).await?, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn convergence_cutoff_preserves_progress_after_corrupt_goal() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    corrupt_goal_generation(&fixture.pool, fixture.session(0)).await?;
    record_merge_ready_head(&fixture, 0x54_400, FIRST_HEAD).await?;

    let error = fixture
        .store
        .process_next_convergence_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x54_410))
        })
        .await
        .expect_err("corrupt goal is reported after committing cutoff progress");
    let replayed = fixture
        .store
        .process_next_convergence_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x54_420))
        })
        .await?;
    let cutoff_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_convergence_cutoff")
            .fetch_one(&fixture.pool)
            .await?;

    assert!(matches!(
        error,
        RepoWatchDispatchRepositoryError::GoalCutoff(
            signalbox_persistence::goal::GoalRepositoryError::Corruption(_)
        )
    ));
    assert!(!replayed);
    assert_eq!(cutoff_count, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn equal_current_convergence_evidence_is_idempotent() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    record_merge_ready_head(&fixture, 0x54_500, FIRST_HEAD).await?;
    let event_store = PostgresRepoWatchStore::new(fixture.pool.clone());
    let current = event_store
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");

    event_store
        .record_convergence_assessments(
            &fixture.repository,
            current.generation(),
            &[merge_ready_assessment(FIRST_HEAD)?],
        )
        .await?;
    let assessment_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_pull_request_convergence_assessment")
            .fetch_one(&fixture.pool)
            .await?;

    assert_eq!(assessment_count, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stale_review_clearance_records_intent_before_terminal_audit() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let observation = pull_request_observation(
        context(FIRST_HEAD)?,
        RepoWatchPullRequestLifecycle::Open,
        MergeableState::Mergeable,
    )?;
    let committed = store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                None,
                RepoWatchCursorCandidate::new(observation),
                vec![identified_event(opened_event(0x54_510, FIRST_HEAD)?)],
            ),
        )
        .await?;
    let RepoWatchCommitOutcome::Committed(cursor) = committed else {
        panic!("fixture's first cursor write commits");
    };
    let assessment =
        assessment_with_review_decision(FIRST_HEAD, RepoWatchReviewDecision::ChangesRequested)?;
    store
        .record_convergence_assessments(
            &repository,
            cursor.generation(),
            std::slice::from_ref(&assessment),
        )
        .await?;
    let candidate = RepoWatchStaleReviewClearanceCandidate::try_new(
        &assessment,
        STALE_REVIEW_NODE_ID.to_owned(),
        RepoWatchAuthorLogin::try_new(STALE_REVIEWER.to_owned())?,
        CommitSha::try_new(INITIAL_HEAD.to_owned())?,
    )?;

    let planned = store
        .plan_stale_review_clearances(
            &repository,
            cursor.generation(),
            std::slice::from_ref(&candidate),
        )
        .await?;
    let replayed = store
        .plan_stale_review_clearances(
            &repository,
            cursor.generation(),
            std::slice::from_ref(&candidate),
        )
        .await?;
    let pending = store
        .load_pending_stale_review_clearances(&repository)
        .await?;
    store
        .record_stale_review_clearance_outcome(
            planned[0].clearance_id(),
            RepoWatchStaleReviewClearanceOutcome::Dismissed,
            RepoWatchObservedReviewState::Dismissed,
        )
        .await?;
    let settled = store
        .load_pending_stale_review_clearances(&repository)
        .await?;
    let audit: (String, String, String) = sqlx::query_as(
        "SELECT clearance.reason_kind, result.outcome_kind,
                result.provider_review_state
           FROM repo_watch_stale_review_clearance AS clearance
           JOIN repo_watch_stale_review_clearance_result AS result
             ON result.clearance_id = clearance.clearance_id",
    )
    .fetch_one(&pool)
    .await?;

    assert_eq!(planned[0].clearance_id(), replayed[0].clearance_id());
    assert_eq!(pending[0].review_node_id(), STALE_REVIEW_NODE_ID);
    assert_eq!(pending[0].reviewer().as_str(), STALE_REVIEWER);
    assert!(settled.is_empty());
    assert_eq!(
        audit,
        (
            "only_stale_review_blocks".to_owned(),
            "dismissed".to_owned(),
            "dismissed".to_owned(),
        )
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stale_base_review_clearance_is_rejected_before_forge_mutation()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool);
    let observation = pull_request_observation(
        context(FIRST_HEAD)?,
        RepoWatchPullRequestLifecycle::Open,
        MergeableState::Mergeable,
    )?;
    let committed = store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                None,
                RepoWatchCursorCandidate::new(observation),
                vec![identified_event(opened_event(0x54_515, FIRST_HEAD)?)],
            ),
        )
        .await?;
    let RepoWatchCommitOutcome::Committed(cursor) = committed else {
        panic!("fixture's first cursor write commits");
    };
    let assessment = assessment_at_base(
        FIRST_HEAD,
        ADVANCED_BASE_REVISION,
        RepoWatchReviewDecision::ChangesRequested,
    )?;
    store
        .record_convergence_assessments(
            &repository,
            cursor.generation(),
            std::slice::from_ref(&assessment),
        )
        .await?;
    let candidate = RepoWatchStaleReviewClearanceCandidate::try_new(
        &assessment,
        STALE_REVIEW_NODE_ID.to_owned(),
        RepoWatchAuthorLogin::try_new(STALE_REVIEWER.to_owned())?,
        CommitSha::try_new(INITIAL_HEAD.to_owned())?,
    )?;

    let result = store
        .plan_stale_review_clearances(
            &repository,
            cursor.generation(),
            std::slice::from_ref(&candidate),
        )
        .await;

    assert!(matches!(
        result,
        Err(signalbox_persistence::repo_watch::RepoWatchStoreError::StaleReviewClearanceMismatch)
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stale_cutoff_waits_until_its_sealed_identity_is_current_again()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    record_merge_ready_head(&fixture, 0x54_600, FIRST_HEAD).await?;
    let second_generation = commit_mergeable_head(&fixture, 0x54_601, SECOND_HEAD).await?;
    record_assessment_at_base(
        &fixture,
        second_generation,
        SECOND_HEAD,
        BASE_REVISION,
        RepoWatchReviewDecision::ChangesRequested,
    )
    .await?;

    let stale = fixture
        .store
        .process_next_convergence_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x54_602))
        })
        .await?;
    let stale_cutoff_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_convergence_cutoff")
            .fetch_one(&fixture.pool)
            .await?;
    let restored_generation = commit_mergeable_head(&fixture, 0x54_603, FIRST_HEAD).await?;
    record_assessment_at_base(
        &fixture,
        restored_generation,
        FIRST_HEAD,
        BASE_REVISION,
        RepoWatchReviewDecision::Approved,
    )
    .await?;
    let restored = fixture
        .store
        .process_next_convergence_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x54_604))
        })
        .await?;
    let restored_cutoff_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_convergence_cutoff")
            .fetch_one(&fixture.pool)
            .await?;

    assert!(!stale);
    assert_eq!(stale_cutoff_count, 0);
    assert!(restored);
    assert_eq!(restored_cutoff_count, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn base_advance_requires_a_fresh_seal_for_cutoff_and_admission() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let (loaded, stale_observation) = load_second_conflict(&fixture).await?;
    record_merge_ready_head(&fixture, 0x54_700, SECOND_HEAD).await?;
    let advanced_generation =
        commit_mergeable_head_at_base(&fixture, 0x54_701, SECOND_HEAD, ADVANCED_BASE_REVISION)
            .await?;
    record_assessment_at_base(
        &fixture,
        advanced_generation,
        SECOND_HEAD,
        ADVANCED_BASE_REVISION,
        RepoWatchReviewDecision::ChangesRequested,
    )
    .await?;

    let stale_cutoff = fixture
        .store
        .process_next_convergence_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x54_702))
        })
        .await?;
    let stale_admission =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                loaded,
                &fixture.rule,
                &stale_observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    record_assessment_at_base(
        &fixture,
        advanced_generation,
        SECOND_HEAD,
        ADVANCED_BASE_REVISION,
        RepoWatchReviewDecision::None,
    )
    .await?;
    let fresh_cutoff = fixture
        .store
        .process_next_convergence_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x54_703))
        })
        .await?;
    let sealed_bases: Vec<String> = sqlx::query_scalar(
        "SELECT base_revision
           FROM repo_watch_pull_request_convergence
          ORDER BY base_revision",
    )
    .fetch_all(&fixture.pool)
    .await?;

    assert!(!stale_cutoff);
    assert_eq!(stale_admission, RepoWatchRuleEvaluationOutcome::Occupied);
    assert!(fresh_cutoff);
    assert_eq!(
        sealed_bases,
        vec![BASE_REVISION.to_owned(), ADVANCED_BASE_REVISION.to_owned()]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stale_base_assessment_cannot_cut_off_or_suppress_base_advance_work()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let (loaded, stale_observation) = load_second_conflict(&fixture).await?;
    record_merge_ready_head(&fixture, 0x54_710, SECOND_HEAD).await?;
    commit_mergeable_head_at_base(&fixture, 0x54_711, SECOND_HEAD, ADVANCED_BASE_REVISION).await?;

    let cutoff = fixture
        .store
        .process_next_convergence_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x54_712))
        })
        .await?;
    let admission =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                loaded,
                &fixture.rule,
                &stale_observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;

    assert!(!cutoff);
    assert_eq!(admission, RepoWatchRuleEvaluationOutcome::Occupied);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn database_rejects_seal_for_nonconverged_assessment() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let event_store = PostgresRepoWatchStore::new(fixture.pool.clone());
    let current = event_store
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    let pull_request = &current.candidate().observation().state().pull_requests()[0];
    let assessment =
        RepoWatchConvergenceAssessment::try_new(RepoWatchConvergenceAssessmentInput {
            number: pull_request.context().number(),
            head_sha: pull_request.context().head_sha().clone(),
            base_branch: pull_request.context().base_branch().clone(),
            base_revision: CommitSha::try_new(BASE_REVISION.to_owned())?,
            mergeable_state: pull_request.mergeable_state(),
            review_decision: RepoWatchReviewDecision::None,
            unresolved_threads: Vec::new(),
            gating_check_count: 0,
            non_green_gating_checks: Vec::new(),
        })?;
    event_store
        .record_convergence_assessments(&fixture.repository, current.generation(), &[assessment])
        .await?;
    let assessment_id: Uuid = sqlx::query_scalar(
        "SELECT assessment_id
           FROM repo_watch_pull_request_convergence_assessment",
    )
    .fetch_one(&fixture.pool)
    .await?;

    let error = sqlx::query(
        "INSERT INTO repo_watch_pull_request_convergence
                (repository, pull_request_number, head_sha, base_revision,
                 assessment_id, convergence_kind)
         SELECT repository, pull_request_number, head_sha, base_revision,
                assessment_id, 'merge_ready'
           FROM repo_watch_pull_request_convergence_assessment
          WHERE assessment_id = $1",
    )
    .bind(assessment_id)
    .execute(&fixture.pool)
    .await
    .expect_err("a nonconverged assessment cannot back a convergence seal");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("repo_watch_convergence_assessment_matches")
    );
    Ok(())
}

/// Once one exact head has converged, later review activity on that unchanged
/// head remains visible but cannot reopen dispatch against it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn later_review_on_sealed_head_does_not_reopen_dispatch() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let (loaded, stale_observation) = load_second_conflict(&fixture).await?;
    let batches_before: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
        .fetch_one(&fixture.pool)
        .await?;
    record_merge_ready_head(&fixture, 0x55_000, SECOND_HEAD).await?;
    let event_store = PostgresRepoWatchStore::new(fixture.pool.clone());
    let current = event_store
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    event_store
        .record_convergence_assessments(
            &fixture.repository,
            current.generation(),
            &[assessment_with_review_decision(
                SECOND_HEAD,
                RepoWatchReviewDecision::ChangesRequested,
            )?],
        )
        .await?;

    let outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                loaded,
                &fixture.rule,
                &stale_observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    let latest_verdict: String =
        sqlx::query_scalar("SELECT verdict_kind FROM repo_watch_current_pull_request_convergence")
            .fetch_one(&fixture.pool)
            .await?;
    let seal_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_pull_request_convergence")
            .fetch_one(&fixture.pool)
            .await?;
    let batches_after: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
        .fetch_one(&fixture.pool)
        .await?;

    assert_eq!(outcome, RepoWatchRuleEvaluationOutcome::TargetConverged);
    assert_eq!(latest_verdict, "not_converged");
    assert_eq!(seal_count, 1);
    assert_eq!(batches_after, batches_before);
    Ok(())
}

/// Owed work for an exact head that later converges is settled rather than
/// dispatched after the convergence cutoff frees its singleton.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn converged_target_settles_owed_work_without_dispatch() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let occupied = evaluate_second_conflict(&fixture).await?;
    record_merge_ready_head(&fixture, 0x56_000, SECOND_HEAD).await?;
    fixture
        .store
        .process_next_convergence_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x56_100))
        })
        .await?;
    let obligation = fixture
        .store
        .load_next_dispatch_obligation(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?
        .expect("released converged-target obligation becomes eligible for settlement");
    let cursor = PostgresRepoWatchStore::new(fixture.pool.clone())
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");

    let outcome =
        evaluate_obligation(&fixture, obligation, cursor.candidate().observation()).await?;
    let settlement: String = sqlx::query_scalar(
        "SELECT settled_kind
           FROM repo_watch_dispatch_obligation",
    )
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(occupied, RepoWatchRuleEvaluationOutcome::Occupied);
    assert_eq!(outcome, RepoWatchRuleEvaluationOutcome::TargetConverged);
    assert_eq!(settlement, "target_converged");
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
        .expect("the elapsed cooldown makes retained work ready");
    let retained =
        evaluate_obligation(&fixture, obligation, cursor.candidate().observation()).await?;

    assert!(release_age_seconds >= 7_199.0);
    assert_eq!(outcome, RepoWatchRuleEvaluationOutcome::Occupied);
    assert!(outcome_is_dispatched(&retained));
    Ok(())
}

/// A terminal session cannot leave an open, unconverged pull request with no
/// durable path to another delivery.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn released_unconverged_dispatch_retains_a_follow_up_obligation() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    sqlx::query("INSERT INTO repo_watch_dispatch_release (dispatch_id) VALUES ($1)")
        .bind(fixture.dispatch_id.as_uuid())
        .execute(&fixture.pool)
        .await?;
    let visible: (Uuid, Uuid, i64, bool) = sqlx::query_as(
        "SELECT obligation_id, latest_event_id, matched_event_count, ready
           FROM repo_watch_outstanding_dispatch_obligation",
    )
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
        .expect("released unconverged work is owed again");
    let cursor = PostgresRepoWatchStore::new(fixture.pool.clone())
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    let outcome =
        evaluate_obligation(&fixture, obligation, cursor.candidate().observation()).await?;

    assert_eq!(visible.0, *fixture.dispatch_id.as_uuid());
    assert_eq!(visible.1, *fixture.event.id().as_uuid());
    assert_eq!(visible.2, 1);
    assert!(visible.3);
    assert!(outcome_is_dispatched(&outcome));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn released_dispatch_follow_up_settles_after_its_target_closes() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    sqlx::query("INSERT INTO repo_watch_dispatch_release (dispatch_id) VALUES ($1)")
        .bind(fixture.dispatch_id.as_uuid())
        .execute(&fixture.pool)
        .await?;
    commit_merge(&fixture, 0x20_100).await?;
    let obligation = fixture
        .store
        .load_next_dispatch_obligation(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?
        .expect("closed-target work remains available for settlement");
    let cursor = PostgresRepoWatchStore::new(fixture.pool.clone())
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    let outcome =
        evaluate_obligation(&fixture, obligation, cursor.candidate().observation()).await?;
    let settlement: String = sqlx::query_scalar(
        "SELECT settled_kind
           FROM repo_watch_dispatch_obligation
          WHERE obligation_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(outcome, RepoWatchRuleEvaluationOutcome::TargetClosed);
    assert_eq!(settlement, "target_closed");
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
        "ALTER TABLE repo_watch_dispatch_release
         DISABLE TRIGGER repo_watch_dispatch_release_retains_obligation",
    )
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        "INSERT INTO repo_watch_dispatch_release (dispatch_id, released_at)
         VALUES ($1, clock_timestamp() + interval '2 seconds')",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        "ALTER TABLE repo_watch_dispatch_release
         ENABLE TRIGGER repo_watch_dispatch_release_retains_obligation",
    )
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
async fn operator_status_reader_projects_dispatch_visibility() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let occupied = evaluate_conflict(&fixture, 102, SECOND_HEAD).await?;
    let mut reader = ProcessOperatorStatusRepository::new(fixture.pool.clone())
        .open()
        .await?;
    let held = held_status_item(
        reader
            .next_item()
            .await?
            .expect("the active fixture dispatch holds one slot"),
    );
    let queued = queued_status_item(
        reader
            .next_item()
            .await?
            .expect("the occupied match creates one queued obligation"),
    );

    assert_eq!(held.dispatch_id(), *fixture.dispatch_id.as_uuid());
    assert_eq!(held.session_ids(), session_uuids(&fixture));
    assert_eq!(
        held.blockers(),
        [
            ProcessOperatorStatusHeldSlotBlocker::DeliveryTurnRuntimeRelevant,
            ProcessOperatorStatusHeldSlotBlocker::LiveRuntimeTurn,
            ProcessOperatorStatusHeldSlotBlocker::PursuingGoal,
        ]
    );
    assert_eq!(queued.latest_event_id(), *occupied.event_id.as_uuid());
    assert_eq!(occupied.outcome, RepoWatchRuleEvaluationOutcome::Occupied);
    assert_eq!(
        queued.occupying_dispatch_id(),
        Some(*fixture.dispatch_id.as_uuid())
    );
    assert_eq!(queued.occupying_session_ids(), session_uuids(&fixture));
    assert!(!queued.ready());
    Ok(())
}

#[track_caller]
fn held_status_item(item: ProcessOperatorStatusItem) -> ProcessOperatorStatusHeldSlot {
    match item {
        ProcessOperatorStatusItem::HeldSlot(item) => item,
        item => panic!("fixture expected held status first, got {item:?}"),
    }
}

#[track_caller]
fn queued_status_item(item: ProcessOperatorStatusItem) -> ProcessOperatorStatusQueuedObligation {
    match item {
        ProcessOperatorStatusItem::QueuedObligation(item) => item,
        item => panic!("fixture expected queued status second, got {item:?}"),
    }
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
            immediate_retry_policy(),
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
            immediate_retry_policy(),
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

// --- Operator-commissioned dispatch: fence consumption and escalation ---

const COMMISSION_COMMAND_ID: u128 = 0x60_100;
const COMMISSION_TEMPLATE: &str = "review-response";
const COMMISSION_STATEMENT: &str =
    "Address the review findings on pull request 41 and push fixes to its head branch.";
const COMMISSION_CONTEXT: &str = "Respond to the open review threads.";

struct CommissionedFixture {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
    store: PostgresCommissionedDispatchStore,
    dispatch_id: CommissionedDispatchId,
    session: SessionId,
}

fn commissioned_fence() -> Result<CommissionedDispatchFence, Box<dyn Error>> {
    Ok(CommissionedDispatchFence::PullRequest {
        repository: repository()?,
        pull_request: PullRequestNumber::new(BOTTOM_PULL_REQUEST_NUMBER.try_into()?),
        head_sha: CommitSha::try_new(FIRST_HEAD.to_owned())?,
        head_repository: RepositorySlug::try_new(HEAD_REPOSITORY.to_owned())?,
        head_branch: BranchName::try_new(HEAD_BRANCH.to_owned())?,
        base_branch: BranchName::try_new(BASE_BRANCH.to_owned())?,
    })
}

fn commission_request_with_fence(
    command: u128,
    fence: CommissionedDispatchFence,
) -> Result<CommissionDispatchRequest, Box<dyn Error>> {
    Ok(CommissionDispatchRequest::try_new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        SessionTemplateName::try_new(COMMISSION_TEMPLATE.to_owned())?,
        fence,
        GoalStatement::try_new(COMMISSION_STATEMENT.to_owned())?,
        UserContent::try_text(COMMISSION_CONTEXT.to_owned())
            .expect("the fixture context is admitted"),
    )?)
}

fn commission_request_with_content(
    command: u128,
    content: &str,
) -> Result<CommissionDispatchRequest, Box<dyn Error>> {
    Ok(CommissionDispatchRequest::try_new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        SessionTemplateName::try_new(COMMISSION_TEMPLATE.to_owned())?,
        commissioned_fence()?,
        GoalStatement::try_new(COMMISSION_STATEMENT.to_owned())?,
        UserContent::try_text(content.to_owned()).expect("the fixture context is admitted"),
    )?)
}

/// The exact template shape the dispatch fixtures resolve, under the
/// commissioned template name.
fn commissioned_template() -> (SessionTemplateProvenance, SessionConfigurationDefaults) {
    (
        SessionTemplateProvenance::new(
            SessionTemplateName::try_new(COMMISSION_TEMPLATE.to_owned())
                .expect("the fixture template name is admitted"),
            SessionTemplateContentDigest::from_bytes([7; 32]),
        ),
        SessionConfigurationDefaults::complete(
            ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(Uuid::from_u128(
                TEMPLATE_MODEL_SELECTION_ID,
            ))),
            DangerousToolAutoApproval::Disabled,
            Some(
                SessionSystemPrompt::try_new("Respond to review findings.".to_owned())
                    .expect("the fixture prompt is admitted"),
            ),
        ),
    )
}

async fn commissioned_fixture() -> Result<CommissionedFixture, Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let store = PostgresCommissionedDispatchStore::new(pool.clone(), credential_pin());
    let (provenance, defaults) = commissioned_template();
    let prepared = commission_request_with_fence(COMMISSION_COMMAND_ID, commissioned_fence()?)?
        .prepare(
            &mut UuidV7CommissionedDispatchIdGenerator,
            provenance,
            defaults,
        )?;
    let CommissionDispatchOutcome::Dispatched { dispatch, session } =
        store.commission(prepared, |_| None).await?
    else {
        panic!("the fixture commission dispatches fresh")
    };
    Ok(CommissionedFixture {
        _container: container,
        pool,
        store,
        dispatch_id: dispatch,
        session,
    })
}

#[derive(Debug, sqlx::FromRow)]
struct CommissionedEscalationVisibility {
    lifecycle_state: String,
    terminal_disposition: Option<String>,
    goal_event_kind: String,
    blocked_reason: Option<String>,
    need: Option<String>,
    rationale: String,
}

/// A commissioned session's first turn is judged under the exact fence its
/// commission recorded, through the same authority loading a repository-watch
/// dispatch feeds, and an unattended escalation terminalizes the turn with a
/// commissioned audit row instead of pooling forever in the approval wait.
/// The exact replay is stable and a mismatched replay identity is refused.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_commissioned_escalation_terminalizes_under_its_recorded_fence()
-> Result<(), Box<dyn Error>> {
    let fixture = commissioned_fixture().await?;
    let seed = 0x61_240;
    let (model_repository, prepared, turn, requests) = checkpoint_delegated_approval_at(
        &fixture.pool,
        fixture.session,
        seed,
        &[("exec", r#"{"cmd":"git fetch origin main"}"#)],
    )
    .await?;
    let [request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let authority = prepared_pull_request_authority(&prepared);
    assert_eq!(
        authority.dispatch(),
        ApprovalJudgeDispatchProvenance::Commissioned(fixture.dispatch_id)
    );
    assert_eq!(authority.repository(), &repository()?);
    assert_eq!(authority.pull_request().get(), BOTTOM_PULL_REQUEST_NUMBER);
    assert_eq!(authority.head_sha().as_str(), FIRST_HEAD);
    assert_eq!(authority.head_repository().as_str(), HEAD_REPOSITORY);
    assert_eq!(authority.head_branch().as_str(), HEAD_BRANCH);
    assert_eq!(authority.base_branch().as_str(), BASE_BRANCH);

    let approval_repository = model_repository.approval_judge_repository();
    approval_repository.authorize(&prepared).await?;
    let rationale = ToolDecisionRationale::try_new(String::from(
        "the provider requests authority beyond the immutable commissioned fence",
    ))?;
    let identities = ApprovalJudgeCompletionIdentities::new(
        TurnAttemptId::from_uuid(Uuid::from_u128(seed + 10)),
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 11)),
        ContextFrontierId::from_uuid(Uuid::from_u128(seed + 12)),
    );
    let closed_result = |closed_request: ToolRequestId| {
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
            closed_request.as_uuid().as_u128() + CLOSED_RESULT_ID_OFFSET,
        ))
    };

    let outcome = approval_repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::EscalateToHuman,
            rationale.clone(),
            ProviderReportedTokenUsage::unreported(),
            identities,
            closed_result,
        )
        .await?;
    assert_eq!(
        outcome,
        CompleteApprovalJudgeOutcome::HeadlessEscalationTerminalized
    );

    let audit: CommissionedEscalationVisibility = sqlx::query_as(
        "SELECT lifecycle.state_kind AS lifecycle_state,
                lifecycle.terminal_disposition_kind AS terminal_disposition,
                latest_goal.event_kind AS goal_event_kind,
                latest_goal.blocked_reason,
                latest_goal.need,
                audit.rationale
           FROM commissioned_dispatch_headless_approval_escalation_audit AS audit
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.session_id = audit.session_id
            AND lifecycle.turn_id = audit.turn_id
           JOIN LATERAL (
                SELECT event_kind, blocked_reason, need
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
    assert_eq!(audit.lifecycle_state, "terminal");
    assert_eq!(audit.terminal_disposition.as_deref(), Some("failed"));
    assert_eq!(audit.goal_event_kind, "blocked");
    assert_eq!(audit.blocked_reason.as_deref(), Some("execution_failure"));
    assert!(
        audit
            .need
            .as_deref()
            .is_some_and(|need| need.contains("commissioned directly")),
        "the commissioned block names its own repair, not a repository-watch redispatch"
    );
    assert_eq!(audit.rationale, rationale.as_str());
    let audited_dispatch: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1
              FROM commissioned_dispatch_headless_approval_escalation
             WHERE dispatch_id = $1 AND session_id = $2
        )",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .bind(fixture.session.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert!(audited_dispatch);

    let replay = approval_repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::EscalateToHuman,
            rationale.clone(),
            ProviderReportedTokenUsage::unreported(),
            identities,
            closed_result,
        )
        .await?;
    assert_eq!(
        replay,
        CompleteApprovalJudgeOutcome::HeadlessEscalationTerminalized
    );
    let mismatched = approval_repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::EscalateToHuman,
            rationale.clone(),
            ProviderReportedTokenUsage::unreported(),
            ApprovalJudgeCompletionIdentities::new(
                TurnAttemptId::from_uuid(Uuid::from_u128(seed + 30)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 11)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 12)),
            ),
            closed_result,
        )
        .await;
    assert!(matches!(
        mismatched,
        Err(ApprovalJudgeRepositoryError::Corruption(
            ApprovalJudgeCorruption::Inconsistent("completed judge replay")
        ))
    ));
    assert_eq!(prepared.request().id(), *request);
    assert_eq!(turn, prepared.request().turn());
    Ok(())
}

/// One commission commits one session, one queued goal-adopted turn, and one
/// fence row; the same command identity replays to the committed session, and
/// the same identity naming a different fence is a conflicting reuse.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_replayed_commission_returns_its_committed_session() -> Result<(), Box<dyn Error>> {
    let fixture = commissioned_fixture().await?;

    let commissioned_turn: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM goal_turn WHERE session_id = $1 AND goal_generation = 1
        )",
    )
    .bind(fixture.session.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert!(
        commissioned_turn,
        "the commission adopts its reserved turn as the goal's first turn"
    );

    let (provenance, defaults) = commissioned_template();
    let replay = commission_request_with_fence(COMMISSION_COMMAND_ID, commissioned_fence()?)?
        .prepare(
            &mut UuidV7CommissionedDispatchIdGenerator,
            provenance,
            defaults,
        )?;
    assert_eq!(
        fixture.store.commission(replay, |_| None).await?,
        CommissionDispatchOutcome::Replayed {
            dispatch: fixture.dispatch_id,
            session: fixture.session,
        }
    );

    let (provenance, defaults) = commissioned_template();
    let conflicting = commission_request_with_fence(
        COMMISSION_COMMAND_ID,
        CommissionedDispatchFence::Branch {
            repository: repository()?,
            branch: BranchName::try_new(BASE_BRANCH.to_owned())?,
        },
    )?
    .prepare(
        &mut UuidV7CommissionedDispatchIdGenerator,
        provenance,
        defaults,
    )?;
    assert_eq!(
        fixture.store.commission(conflicting, |_| None).await?,
        CommissionDispatchOutcome::ConflictingReuse
    );

    // The committed commission is loadable by its command identity alone, and
    // that record answers the daemon's pre-template replay equality — so a
    // retry of the exact request replays even if configuration later removed
    // or renamed the template it was commissioned from.
    let recorded = fixture
        .store
        .load(DurableCommandId::from_uuid(Uuid::from_u128(
            COMMISSION_COMMAND_ID,
        )))
        .await?
        .expect("the committed commission is loadable by its command identity");
    assert_eq!(recorded.dispatch(), fixture.dispatch_id);
    assert_eq!(recorded.session(), fixture.session);
    assert!(recorded.matches(&commission_request_with_fence(
        COMMISSION_COMMAND_ID,
        commissioned_fence()?
    )?));
    assert!(!recorded.matches(&commission_request_with_fence(
        COMMISSION_COMMAND_ID,
        CommissionedDispatchFence::Branch {
            repository: repository()?,
            branch: BranchName::try_new(BASE_BRANCH.to_owned())?,
        },
    )?));

    // A retry that changes only the initial content names different intent:
    // the recorded digest refuses it instead of silently replaying a session
    // whose first input was something else.
    let changed_content =
        commission_request_with_content(COMMISSION_COMMAND_ID, "Different context entirely.")?;
    assert!(!recorded.matches(&changed_content));
    let (provenance, defaults) = commissioned_template();
    let changed = changed_content.prepare(
        &mut UuidV7CommissionedDispatchIdGenerator,
        provenance,
        defaults,
    )?;
    assert_eq!(
        fixture.store.commission(changed, |_| None).await?,
        CommissionDispatchOutcome::ConflictingReuse
    );

    // An ordinary template creation retried against the commission's command
    // identity is a different wire operation: it must refuse rather than adopt
    // the commissioned session as its own replay.
    let (provenance, defaults) = commissioned_template();
    let ordinary = CreateSession::new_from_template(
        DurableCommandId::from_uuid(Uuid::from_u128(COMMISSION_COMMAND_ID)),
        SessionCreationProvenance::new(
            SessionCreationCause::UserInitiated,
            TranscriptAncestry::None,
        ),
        provenance,
        defaults,
    )
    .prepare(SessionId::from_uuid(Uuid::from_u128(0x60_300)))
    .expect("the fixture creation command prepares");
    let ordinary_repository = CreateSessionRepository::new(fixture.pool.clone(), credential_pin());
    let ordinary_outcome = ordinary_repository.handle(ordinary).await?;
    let CreateSessionHandlingOutcome::ConflictingReuse { .. } = ordinary_outcome else {
        panic!("ordinary creation must refuse a commission's command identity");
    };

    // The replay probe the daemon runs before handling refuses the same way:
    // a commission-claimed identity reads as a different command kind, never
    // as an equal ordinary creation.
    let probe = ordinary_repository
        .load(DurableCommandId::from_uuid(Uuid::from_u128(
            COMMISSION_COMMAND_ID,
        )))
        .await;
    let Err(CreateSessionRepositoryError::DifferentCommandKind { .. }) = probe else {
        panic!("the ordinary replay probe must refuse a commission's command identity");
    };

    // A command identity already claimed by another kind entirely is the same
    // refusal, not a fail-closed corruption.
    let foreign_command = 0x60_200;
    assert_applied_goal_command(
        GoalRepository::new(fixture.pool.clone())
            .handle_user_command(
                GoalUserCommand::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(foreign_command)),
                    fixture.session,
                    GoalUserAction::Stop {
                        descendant_scope: DescendantTerminationScope::ParentAlone,
                    },
                ),
                None,
                |_| None,
            )
            .await?,
    );
    let (provenance, defaults) = commissioned_template();
    let reused = commission_request_with_fence(foreign_command, commissioned_fence()?)?.prepare(
        &mut UuidV7CommissionedDispatchIdGenerator,
        provenance,
        defaults,
    )?;
    assert_eq!(
        fixture.store.commission(reused, |_| None).await?,
        CommissionDispatchOutcome::ConflictingReuse
    );

    // The registry read the daemon runs on the unknown-template path sees the
    // same claim, so a foreign identity refuses as conflicting reuse even
    // when no template resolves; an unseen identity claims nothing.
    assert!(
        fixture
            .store
            .identity_claimed(DurableCommandId::from_uuid(Uuid::from_u128(
                foreign_command
            )))
            .await?
    );
    assert!(
        !fixture
            .store
            .identity_claimed(DurableCommandId::from_uuid(Uuid::from_u128(0x60_201)))
            .await?
    );
    Ok(())
}
